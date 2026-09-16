use alloy_primitives::{
    Address, B256, Bytes, U64, U256, address,
    aliases::{I24, U24, U160},
};
use alloy_provider::{ProviderBuilder, RootProvider, mock::Asserter};
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    Dex, DiscoverPools, Error, EvmClient, ROBINHOOD_CHAIN_ID,
    dex::{Currency, ExactInput, Quote, QuoteRequest, SwapLimits, SwapTransaction},
    uniswap::v3::{UniswapV3, V3PoolQuery},
    uniswap::{DiscoveryOutcome, SearchScope, V3Deployment, V3Pool},
};

const FACTORY: Address = address!("1f7d7550b1b028f7571e69a784071f0205fd2efa");
const ROUTER: Address = address!("caf681a66d020601342297493863e78c959e5cb2");
const QUOTER: Address = address!("33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7");
const WETH: Address = address!("0bd7d308f8e1639fab988df18a8011f41eacad73");
const TOKEN: Address = address!("5fc5360d0400a0fd4f2af552add042d716f1d168");
const POOL: Address = address!("52e65b17fb6e5ba00ed806f37afcd2daa50271ca");
const BLOCK: B256 = B256::repeat_byte(7);
const OWNER: Address = Address::repeat_byte(1);
const RECIPIENT: Address = Address::repeat_byte(2);

sol! {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
    function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
    function refundETH() external payable;
    function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
    function sweepToken(address token, uint256 amountMinimum, address recipient) external payable;
}

#[tokio::test]
async fn connection_rejects_conflicting_router_or_quoter_contracts() {
    for index in [0, 2, 3] {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        let mut addresses = [FACTORY, WETH, FACTORY, WETH];
        addresses[index] = Address::repeat_byte(9);
        connection_returns(&rpc, addresses);

        assert!(
            UniswapV3::connect(client, deployment(), BLOCK)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn loading_requires_both_factory_registration_and_the_canonical_address() {
    let (adapter, rpc) = adapter().await;
    pool_returns(&rpc, POOL);
    assert_eq!(adapter.load_pool(POOL, BLOCK).await.unwrap(), pool());

    let impostor = Address::repeat_byte(9);
    for (candidate, registered) in [(POOL, impostor), (impostor, impostor)] {
        pool_returns(&rpc, registered);
        assert!(matches!(
            adapter.load_pool(candidate, BLOCK).await,
            Err(Error::InvalidPool(_))
        ));
    }
}

#[tokio::test]
async fn token_swap_keeps_the_deadline_stricter_minimum_and_approval_target() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    let mut limits = limits();
    limits.minimum_amount_out = U256::from(950);
    let plan = adapter.build_swap(&quote, limits).unwrap();
    let batch = batch(&plan);
    let swap = exactInputSingleCall::abi_decode_validate(&batch.data[0])
        .unwrap()
        .params;

    assert_eq!(quote.amount_out, U256::from(900));
    assert_eq!(batch.deadline, U256::from(limits.deadline_unix_seconds));
    assert_eq!(batch.data.len(), 1);
    assert_eq!(
        (swap.tokenIn, swap.tokenOut, swap.fee),
        (WETH, TOKEN, U24::from(100))
    );
    assert_eq!(
        (swap.amountIn, swap.amountOutMinimum),
        (U256::from(1_000), U256::from(950))
    );
    assert_eq!(swap.sqrtPriceLimitX96, U160::ZERO);
    assert_eq!(swap.recipient, RECIPIENT);
    assert_eq!(plan.transaction.from, Some(OWNER));
    assert_eq!(plan.transaction.to, Some(ROUTER.into()));
    assert_eq!(plan.transaction.chain_id, Some(ROBINHOOD_CHAIN_ID));
    assert_eq!(plan.transaction.value.unwrap_or_default(), U256::ZERO);
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
async fn native_buy_attaches_value_and_refunds_unused_eth() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(&adapter, &rpc, Currency::Native, Currency::Erc20(TOKEN)).await;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let batch = batch(&plan);
    let swap = exactInputSingleCall::abi_decode_validate(&batch.data[0])
        .unwrap()
        .params;

    assert_eq!(batch.data.len(), 2);
    refundETHCall::abi_decode_validate(&batch.data[1]).unwrap();
    assert_eq!(
        (swap.tokenIn, swap.tokenOut, swap.recipient),
        (WETH, TOKEN, RECIPIENT)
    );
    assert_eq!(swap.amountIn, U256::from(1_000));
    assert_eq!(swap.amountOutMinimum, U256::from(850));
    assert_eq!(plan.transaction.value, Some(U256::from(1_000)));
    assert!(plan.approval.is_none());
}

#[tokio::test]
async fn native_sell_unwraps_the_reverse_swap_to_the_requested_recipient() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(&adapter, &rpc, Currency::Erc20(TOKEN), Currency::Native).await;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let batch = batch(&plan);
    let swap = exactInputSingleCall::abi_decode_validate(&batch.data[0])
        .unwrap()
        .params;
    let unwrap = unwrapWETH9Call::abi_decode_validate(&batch.data[1]).unwrap();

    assert_eq!(batch.data.len(), 2);
    assert_eq!(
        (swap.tokenIn, swap.tokenOut, swap.recipient),
        (TOKEN, WETH, ROUTER)
    );
    assert_eq!(
        (unwrap.recipient, unwrap.amountMinimum),
        (RECIPIENT, U256::from(850))
    );
    assert_eq!(plan.transaction.value.unwrap_or_default(), U256::ZERO);
    assert_eq!(plan.approval.unwrap().token, TOKEN);
}

#[tokio::test]
async fn literal_recipients_are_not_reinterpreted_as_router_aliases() {
    let (adapter, rpc) = adapter().await;
    let mut quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    for value in [0, 1, 2] {
        let recipient = Address::from_word(B256::from(U256::from(value)));
        quote.request.trade.recipient = recipient;
        let batch = batch(&adapter.build_swap(&quote, limits()).unwrap());
        let swap = exactInputSingleCall::abi_decode_validate(&batch.data[0])
            .unwrap()
            .params;
        if value == 0 {
            assert_eq!(swap.recipient, recipient);
            assert_eq!(batch.data.len(), 1);
        } else {
            assert_eq!(swap.recipient, ROUTER);
            assert_eq!(batch.data.len(), 2);
            let sweep = sweepTokenCall::abi_decode_validate(&batch.data[1]).unwrap();
            assert_eq!(
                (sweep.token, sweep.recipient, sweep.amountMinimum),
                (TOKEN, recipient, U256::from(850))
            );
        }
    }
}

#[tokio::test]
async fn discovery_keeps_pools_when_another_fee_tier_fails() {
    let (adapter, rpc) = adapter().await;
    returned(&rpc, POOL);
    pool_returns(&rpc, POOL);
    rpc.push_failure_msg("historical state unavailable");
    returned(&rpc, Address::ZERO);

    let search = adapter
        .find_pools(query(&[100, 500, 3_000]), BLOCK)
        .await
        .unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = search.outcome
    else {
        panic!("configured V3 must report its search");
    };
    assert_eq!(pools, vec![pool()]);
    assert_eq!(coverage.requested, scopes(&[100, 500, 3_000]));
    assert_eq!(coverage.completed, scopes(&[100, 3_000]));
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].scope, SearchScope::V3FeeTier(U24::from(500)));
    assert!(matches!(failures[0].error, Error::Rpc(_)));
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn no_requested_fee_tiers_reports_no_search_coverage() {
    let (adapter, rpc) = adapter().await;
    let search = adapter.find_pools(query(&[]), BLOCK).await.unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = search.outcome
    else {
        panic!("configured V3 must report its search");
    };
    assert!(pools.is_empty() && failures.is_empty());
    assert!(coverage.requested.is_empty() && coverage.completed.is_empty());
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn quotes_reject_zero_and_malformed_quoter_output() {
    let (adapter, rpc) = adapter().await;
    for data in [
        Bytes::new(),
        Bytes::from((U256::ZERO, U160::from(1), 0u32, U256::from(1)).abi_encode()),
    ] {
        pool_returns(&rpc, POOL);
        rpc.push_success(&data);
        assert!(
            adapter
                .quote(request(Currency::Erc20(WETH), Currency::Erc20(TOKEN)))
                .await
                .is_err()
        );
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn quote_reverts_and_rpc_failures_remain_distinct() {
    let (adapter, rpc) = adapter().await;
    pool_returns(&rpc, POOL);
    rpc.push_failure(
        serde_json::from_value(serde_json::json!({
            "code": 3, "message": "execution reverted: no liquidity", "data": "0xdeadbeef"
        }))
        .unwrap(),
    );
    let error = adapter
        .quote(request(Currency::Erc20(WETH), Currency::Erc20(TOKEN)))
        .await
        .unwrap_err();
    let Error::QuoteFailed {
        reason,
        revert_data,
    } = error
    else {
        panic!("contract reverts must be quote failures");
    };
    assert!(reason.contains("no liquidity"));
    assert_eq!(revert_data, Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]));

    pool_returns(&rpc, POOL);
    rpc.push_failure_msg("upstream unavailable");
    assert!(matches!(
        adapter
            .quote(request(Currency::Erc20(WETH), Currency::Erc20(TOKEN)))
            .await,
        Err(Error::Rpc(_))
    ));
}

#[tokio::test]
async fn swaps_reject_input_amounts_with_different_router_semantics() {
    let (adapter, _) = adapter().await;
    for amount in [U256::ZERO, U256::from(1) << 255, U256::MAX] {
        let mut request = request(Currency::Erc20(WETH), Currency::Erc20(TOKEN));
        request.trade.amount_in = amount;
        assert!(matches!(
            adapter.quote(request.clone()).await,
            Err(Error::InvalidTrade(_))
        ));
        let quote = Quote {
            request,
            amount_out: U256::from(900),
        };
        assert!(matches!(
            adapter.build_swap(&quote, limits()),
            Err(Error::InvalidTrade(_))
        ));
    }
}

#[tokio::test]
async fn building_rechecks_the_selected_pool_and_chain() {
    let (adapter, rpc) = adapter().await;
    let quote = quote(
        &adapter,
        &rpc,
        Currency::Erc20(WETH),
        Currency::Erc20(TOKEN),
    )
    .await;
    let mut changed = quote.clone();
    changed.request.pool.address = Address::repeat_byte(9);
    assert!(matches!(
        adapter.build_swap(&changed, limits()),
        Err(Error::InvalidPool(_))
    ));
    changed = quote.clone();
    changed.request.pool.fee = U24::from(500);
    assert!(matches!(
        adapter.build_swap(&changed, limits()),
        Err(Error::InvalidPool(_))
    ));
    changed = quote.clone();
    changed.request.pool.chain_id = 1;
    assert!(matches!(
        adapter.build_swap(&changed, limits()),
        Err(Error::ChainMismatch { .. })
    ));
    changed = quote;
    changed.request.trade.chain_id = 1;
    assert!(matches!(
        adapter.build_swap(&changed, limits()),
        Err(Error::ChainMismatch { .. })
    ));
}

async fn adapter() -> (UniswapV3<RootProvider>, Asserter) {
    let rpc = Asserter::new();
    let client = client(&rpc).await;
    connection_returns(&rpc, [FACTORY, WETH, FACTORY, WETH]);
    (
        UniswapV3::connect(client, deployment(), BLOCK)
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

fn deployment() -> V3Deployment {
    V3Deployment {
        factory: FACTORY,
        swap_router_02: ROUTER,
        quoter_v2: QUOTER,
        deployment_block: 8930,
    }
}

fn connection_returns(rpc: &Asserter, addresses: [Address; 4]) {
    rpc.push_success(&Bytes::from_static(&[0x60, 0x00]));
    for address in addresses {
        returned(rpc, address);
    }
    rpc.push_success(&Bytes::from_static(&[0x60, 0x00]));
}

fn pool_returns(rpc: &Asserter, registered: Address) {
    for address in [FACTORY, WETH, TOKEN] {
        returned(rpc, address);
    }
    returned(rpc, U24::from(100));
    returned(rpc, I24::try_from(1).unwrap());
    returned(rpc, registered);
}

fn returned(rpc: &Asserter, value: impl SolValue) {
    rpc.push_success(&Bytes::from(value.abi_encode()));
}

fn pool() -> V3Pool {
    V3Pool {
        chain_id: ROBINHOOD_CHAIN_ID,
        factory: FACTORY,
        address: POOL,
        token0: WETH,
        token1: TOKEN,
        fee: U24::from(100),
        tick_spacing: I24::try_from(1).unwrap(),
    }
}

fn request(currency_in: Currency, currency_out: Currency) -> QuoteRequest<V3Pool> {
    QuoteRequest {
        pool: pool(),
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
    }
}

async fn quote(
    adapter: &UniswapV3<RootProvider>,
    rpc: &Asserter,
    currency_in: Currency,
    currency_out: Currency,
) -> Quote<V3Pool> {
    pool_returns(rpc, POOL);
    returned(
        rpc,
        (U256::from(900), U160::from(1), 0u32, U256::from(40_000)),
    );
    adapter
        .quote(request(currency_in, currency_out))
        .await
        .unwrap()
}

fn limits() -> SwapLimits {
    SwapLimits {
        minimum_amount_out: U256::from(850),
        deadline_unix_seconds: 2_000_000_000,
    }
}

fn batch(plan: &SwapTransaction) -> multicallCall {
    multicallCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap()
}

fn query(fees: &[u32]) -> V3PoolQuery {
    V3PoolQuery {
        chain_id: ROBINHOOD_CHAIN_ID,
        currency_a: Currency::Erc20(WETH),
        currency_b: Currency::Erc20(TOKEN),
        fee_tiers: fees.iter().copied().map(U24::from).collect(),
    }
}

fn scopes(fees: &[u32]) -> Vec<SearchScope> {
    fees.iter()
        .copied()
        .map(|fee| SearchScope::V3FeeTier(U24::from(fee)))
        .collect()
}
