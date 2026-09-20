use alloy_primitives::{Address, B256, Bytes, U64, U256, address};
use alloy_provider::{ProviderBuilder, RootProvider, mock::Asserter};
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    Dex, DiscoverPools, Error, EvmClient, ROBINHOOD_CHAIN_ID,
    dex::{Currency, ExactInput, Quote, QuoteRequest, SwapLimits},
    uniswap::v2::{UniswapV2, V2PoolQuery},
    uniswap::{DiscoveryOutcome, SearchScope, V2Deployment, V2Pool},
};

const FACTORY: Address = address!("8bceaa40b9acdfaedf85adf4ff01f5ad6517937f");
const ROUTER: Address = address!("89e5db8b5aa49aa85ac63f691524311aeb649eba");
const WETH: Address = address!("0bd7d308f8e1639fab988df18a8011f41eacad73");
const TOKEN: Address = address!("c6911796042b15d7fa4f6cde69e245ddcd3d9c31");
const POOL: Address = address!("d95e8e2cd04c207625c6f23c974d365a5f3a91d3");
const BLOCK: B256 = B256::repeat_byte(7);
const OWNER: Address = Address::repeat_byte(1);
const RECIPIENT: Address = Address::repeat_byte(2);

sol! {
    function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[] amounts);
    function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable returns (uint256[] amounts);
    function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[] amounts);
}

#[tokio::test]
async fn router_must_use_the_configured_factory() {
    let rpc = Asserter::new();
    let client = client(&rpc).await;
    connection_returns(&rpc, Address::repeat_byte(9));

    let result = UniswapV2::connect(client, deployment(), BLOCK).await;

    assert!(matches!(result, Err(Error::UnsupportedDeployment(_))));
}

#[tokio::test]
async fn pool_must_match_the_factory_registry() {
    let (adapter, rpc) = adapter().await;
    pool_returns(&rpc, Address::repeat_byte(9));

    let result = adapter.load_pool(POOL, BLOCK).await;

    assert!(matches!(result, Err(Error::InvalidPool(_))));
}

#[tokio::test]
async fn token_swap_preserves_the_quote_limits_and_approval_target() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call =
        swapExactTokensForTokensCall::abi_decode_validate(plan.transaction.input.input().unwrap())
            .unwrap();

    assert_eq!(quote.amount_out, U256::from(900));
    assert_eq!(plan.transaction.from, Some(OWNER));
    assert_eq!(plan.transaction.to, Some(ROUTER.into()));
    assert_eq!(plan.transaction.chain_id, Some(ROBINHOOD_CHAIN_ID));
    assert_eq!(call.amountIn, U256::from(1_000));
    assert_eq!(call.amountOutMin, U256::from(855));
    assert_eq!(call.path, vec![WETH, TOKEN]);
    assert_eq!(call.to, RECIPIENT);
    assert_eq!(call.deadline, U256::from(2_000_000_000u64));
    assert!(matches!(
        adapter.build_swap(
            &quote,
            SwapLimits {
                slippage_bps: 10001,
                ..limits()
            }
        ),
        Err(Error::InvalidTrade(_))
    ));
    let approval = plan.approval.unwrap();
    assert_eq!(
        (
            approval.token,
            approval.owner,
            approval.spender,
            approval.amount
        ),
        (WETH, OWNER, ROUTER, U256::from(1_000))
    );
}

#[tokio::test]
async fn native_buy_attaches_value_without_token_approval() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(&adapter, &rpc, Currency::Native, Currency::Erc20(TOKEN)).await;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call =
        swapExactETHForTokensCall::abi_decode_validate(plan.transaction.input.input().unwrap())
            .unwrap();

    assert_eq!(plan.transaction.value, Some(U256::from(1_000)));
    assert!(plan.approval.is_none());
    assert_eq!(call.path, vec![WETH, TOKEN]);
    assert_eq!(call.amountOutMin, U256::from(855));
    assert_eq!(call.to, RECIPIENT);
    assert_eq!(call.deadline, U256::from(2_000_000_000u64));
}

#[tokio::test]
async fn native_sell_uses_the_reverse_pool_direction() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(&adapter, &rpc, Currency::Erc20(TOKEN), Currency::Native).await;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call =
        swapExactTokensForETHCall::abi_decode_validate(plan.transaction.input.input().unwrap())
            .unwrap();

    assert_eq!(call.path, vec![TOKEN, WETH]);
    assert_eq!(call.amountIn, U256::from(1_000));
    assert_eq!(call.amountOutMin, U256::from(855));
    assert_eq!(call.to, RECIPIENT);
    assert_eq!(plan.transaction.value.unwrap_or_default(), U256::ZERO);
    assert_eq!(plan.approval.unwrap().token, TOKEN);
}

#[tokio::test]
async fn an_absent_pair_completes_the_search_without_failure() {
    let (adapter, rpc) = adapter().await;
    returned(&rpc, Address::ZERO);

    let search = adapter.find_pools(query(), BLOCK).await.unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = search.outcome
    else {
        panic!("configured V2 must report its search");
    };

    assert!(pools.is_empty());
    assert!(failures.is_empty());
    assert_eq!(coverage.requested, vec![SearchScope::V2Pair]);
    assert_eq!(coverage.completed, vec![SearchScope::V2Pair]);
}

#[tokio::test]
async fn an_rpc_failure_does_not_claim_that_no_pool_exists() {
    let (adapter, rpc) = adapter().await;
    rpc.push_failure_msg("upstream unavailable");

    let search = adapter.find_pools(query(), BLOCK).await.unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = search.outcome
    else {
        panic!("configured V2 must report its search");
    };

    assert!(pools.is_empty());
    assert!(coverage.completed.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].scope, SearchScope::V2Pair);
    assert!(matches!(failures[0].error, Error::Rpc(_)));
}

#[tokio::test]
async fn swap_building_rejects_a_replaced_pool_address() {
    let (adapter, rpc) = adapter().await;
    let mut quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    quote.request.pool.address = Address::repeat_byte(9);

    let result = adapter.build_swap(&quote, limits());

    assert!(matches!(result, Err(Error::InvalidPool(_))));
}

#[tokio::test]
async fn quotes_reject_missing_or_zero_router_output() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    for amounts in [vec![U256::from(1_000)], vec![U256::from(1_000), U256::ZERO]] {
        pool_returns(&rpc, POOL);
        returned(&rpc, amounts);

        let result = adapter.quote(quote.request.clone()).await;

        assert!(result.is_err());
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn quote_reverts_keep_the_reason_and_data() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    pool_returns(&rpc, POOL);
    rpc.push_failure(
        serde_json::from_value(serde_json::json!({
            "code": 3,
            "message": "execution reverted: INSUFFICIENT_LIQUIDITY",
            "data": "0xdeadbeef"
        }))
        .unwrap(),
    );

    let error = adapter.quote(quote.request).await.unwrap_err();

    let Error::QuoteFailed {
        reason,
        revert_data,
    } = error
    else {
        panic!("contract reverts must be quote failures");
    };
    assert!(reason.contains("INSUFFICIENT_LIQUIDITY"));
    assert_eq!(revert_data, Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]));
}

#[tokio::test]
async fn quote_rpc_failures_stay_distinct_from_reverts() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    pool_returns(&rpc, POOL);
    rpc.push_failure_msg("historical state unavailable");

    assert!(matches!(
        adapter.quote(quote.request).await,
        Err(Error::Rpc(_))
    ));
}

async fn adapter() -> (UniswapV2<RootProvider>, Asserter) {
    let rpc = Asserter::new();
    let client = client(&rpc).await;
    connection_returns(&rpc, FACTORY);
    (
        UniswapV2::connect(client, deployment(), BLOCK)
            .await
            .unwrap(),
        rpc,
    )
}

async fn client(rpc: &Asserter) -> EvmClient<RootProvider> {
    rpc.push_success(&U64::from(ROBINHOOD_CHAIN_ID));
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_mocked_client(rpc.clone());
    EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
        .await
        .unwrap()
}

fn deployment() -> V2Deployment {
    V2Deployment {
        factory: FACTORY,
        router_02: ROUTER,
        deployment_block: 0,
    }
}

fn connection_returns(rpc: &Asserter, factory: Address) {
    rpc.push_success(&Bytes::from_static(&[0x60, 0x00]));
    returned(rpc, factory);
    returned(rpc, WETH);
    rpc.push_success(&Bytes::from_static(&[0x60, 0x00]));
}

fn pool_returns(rpc: &Asserter, registered: Address) {
    for address in [FACTORY, WETH, TOKEN, registered] {
        returned(rpc, address);
    }
}

fn returned(rpc: &Asserter, value: impl SolValue) {
    rpc.push_success(&Bytes::from(value.abi_encode()));
}

async fn quote(
    adapter: &UniswapV2<RootProvider>,
    rpc: &Asserter,
    currency_in: Currency,
    currency_out: Currency,
) -> Quote<V2Pool> {
    pool_returns(rpc, POOL);
    returned(rpc, vec![U256::from(1_000), U256::from(900)]);
    adapter
        .quote(QuoteRequest {
            pool: V2Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                factory: FACTORY,
                address: POOL,
                token0: WETH,
                token1: TOKEN,
            },
            trade: ExactInput {
                chain_id: ROBINHOOD_CHAIN_ID,
                currency_in,
                currency_out,
                amount_in: U256::from(1_000),
                sender: OWNER,
                recipient: RECIPIENT,
            },
            block_hash: BLOCK,
            options: (),
        })
        .await
        .unwrap()
}

fn limits() -> SwapLimits {
    SwapLimits {
        slippage_bps: 500,
        deadline_unix_seconds: 2_000_000_000,
    }
}

fn query() -> V2PoolQuery {
    V2PoolQuery {
        chain_id: ROBINHOOD_CHAIN_ID,
        currency_a: Currency::Erc20(WETH),
        currency_b: Currency::Erc20(TOKEN),
    }
}
