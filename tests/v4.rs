use alloy_primitives::{
    Address, B256, Bytes, U64, U256, address,
    aliases::{I24, U24, U160},
    b256,
};
use alloy_provider::{ProviderBuilder, RootProvider, mock::Asserter};
use alloy_rpc_types_eth::{Block, Log};
use alloy_sol_types::{SolCall, SolEvent, SolValue, sol};
use evm_trading_sdk::{
    Dex, DiscoverPools, Error, EvmClient, ROBINHOOD_CHAIN_ID, ResolvePool,
    dex::{Currency, ExactInput, Quote, QuoteRequest, SwapLimits},
    uniswap::{
        DiscoveryOutcome, SearchScope, UniswapDeployment, V4Pool, V4PoolKey,
        v4::{UniswapV4, V4PoolLookup, V4PoolQuery, V4QuoteOptions},
    },
};

const BLOCK: B256 = B256::repeat_byte(7);
const OWNER: Address = Address::repeat_byte(1);
const RECIPIENT: Address = Address::repeat_byte(2);
const NVDA: Address = address!("d0601ce157db5bdc3162bbac2a2c8af5320d9eec");
const AC: Address = address!("fad40755de679b262337f8d964d75cac51f8c643");
const ID: B256 = b256!("027f6c0fd8847365e77eae2910169a1b92c513c6f1dadd38fde3ecd7806df5d1");

sol! {
    struct Key { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct Swap { Key poolKey; bool zeroForOne; uint128 amountIn; uint128 amountOutMinimum; uint256 minHopPriceX36; bytes hookData; }
    function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
    event Initialize(bytes32 indexed id, address indexed currency0, address indexed currency1, uint24 fee, int24 tickSpacing, address hooks, uint160 sqrtPriceX96, int24 tick);
}

#[test]
fn the_saved_key_derives_the_supplied_id() {
    assert_eq!(key().id(), ID);
    let mut changed = key();
    changed.hooks = Address::ZERO;
    assert_ne!(changed.id(), ID);
}

#[tokio::test]
async fn deployment_must_use_the_same_manager() {
    let rpc = Asserter::new();
    rpc.push_success(&U64::from(ROBINHOOD_CHAIN_ID));
    let client = EvmClient::connect(
        ProviderBuilder::new().connect_mocked_client(rpc.clone()),
        ROBINHOOD_CHAIN_ID,
    )
    .await
    .unwrap();
    code(&rpc);
    code(&rpc);
    returned(&rpc, Address::repeat_byte(9));
    assert!(matches!(
        UniswapV4::connect(client, deployment(), BLOCK).await,
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn loading_requires_an_initialized_pool() {
    let (adapter, rpc) = adapter().await;
    state(&rpc, 1);
    assert_eq!(adapter.load_pool(key(), BLOCK).await.unwrap(), pool());
    state(&rpc, 0);
    assert!(matches!(
        adapter.load_pool(key(), BLOCK).await,
        Err(Error::PoolNotFound)
    ));
}

#[tokio::test]
async fn quotes_keep_the_selected_hook_data() {
    let (adapter, rpc) = adapter().await;
    state(&rpc, 1);
    returned(&rpc, (U256::from(900), U256::from(40_000)));
    let quote = adapter.quote(request()).await.unwrap();
    assert_eq!(quote.amount_out, U256::from(900));
    assert_eq!(quote.request.options.hook_data, Bytes::from_static(b"hook"));
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call = executeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.commands.as_ref(), &[0x10]);
    assert_eq!(call.deadline, U256::from(1000));
    let (actions, params) = <(Bytes, Vec<Bytes>)>::abi_decode_validate(&call.inputs[0]).unwrap();
    assert_eq!(actions.as_ref(), &[0x06, 0x0c, 0x0e]);
    let swap = Swap::abi_decode_validate(&params[0]).unwrap();
    assert_eq!(swap.hookData, quote.request.options.hook_data);
    assert_eq!(swap.amountIn, 1000);
    assert_eq!(swap.amountOutMinimum, 850);
    assert_eq!(swap.minHopPriceX36, U256::ZERO);
    assert!(swap.zeroForOne);
    assert_eq!((swap.poolKey.currency0, swap.poolKey.currency1), (NVDA, AC));
    assert_eq!(
        <(Address, U256)>::abi_decode_validate(&params[1]).unwrap(),
        (NVDA, U256::from(1000))
    );
    assert_eq!(
        <(Address, Address, U256)>::abi_decode_validate(&params[2]).unwrap(),
        (AC, RECIPIENT, U256::ZERO)
    );
    assert_eq!(plan.transaction.value, Some(U256::ZERO));
    assert_eq!(plan.token_approval.unwrap().spender, deployment().permit2);
    let allowance = plan.permit2_approval.unwrap();
    assert_eq!(allowance.spender, deployment().universal_router);
    assert_eq!(allowance.amount, U160::from(1000));
    assert_eq!(allowance.expiration.to::<u64>(), 1000);
}

#[tokio::test]
async fn native_input_refunds_unused_eth_without_approvals() {
    let (adapter, _) = adapter().await;
    let mut quote = fixture_quote();
    quote.request.pool.key.currency0 = Address::ZERO;
    quote.request.trade.currency_in = Currency::Native;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call = executeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.commands.as_ref(), &[0x10, 0x04]);
    assert_eq!(
        <(Address, Address, U256)>::abi_decode_validate(&call.inputs[1]).unwrap(),
        (Address::ZERO, Address::with_last_byte(1), U256::ZERO)
    );
    assert_eq!(plan.transaction.value, Some(U256::from(1000)));
    assert!(plan.token_approval.is_none() && plan.permit2_approval.is_none());
}

#[tokio::test]
async fn native_output_uses_the_requested_recipient() {
    let (adapter, _) = adapter().await;
    let mut quote = fixture_quote();
    quote.request.pool.key.currency0 = Address::ZERO;
    quote.request.trade.currency_in = Currency::Erc20(AC);
    quote.request.trade.currency_out = Currency::Native;
    let plan = adapter.build_swap(&quote, limits()).unwrap();
    let call = executeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
    let (_, params) = <(Bytes, Vec<Bytes>)>::abi_decode_validate(&call.inputs[0]).unwrap();
    assert!(!Swap::abi_decode_validate(&params[0]).unwrap().zeroForOne);
    assert_eq!(
        <(Address, Address, U256)>::abi_decode_validate(&params[2]).unwrap(),
        (Address::ZERO, RECIPIENT, U256::ZERO)
    );
}

#[tokio::test]
async fn invalid_amounts_and_reserved_recipients_cannot_change_meaning() {
    let (adapter, _) = adapter().await;
    for amount in [U256::ZERO, U256::from(u128::MAX) + U256::from(1)] {
        let mut quote = fixture_quote();
        quote.request.trade.amount_in = amount;
        assert!(matches!(
            adapter.build_swap(&quote, limits()),
            Err(Error::InvalidTrade(_))
        ));
    }
    for recipient in [Address::with_last_byte(1), Address::with_last_byte(2)] {
        let mut quote = fixture_quote();
        quote.request.trade.recipient = recipient;
        assert!(matches!(
            adapter.build_swap(&quote, limits()),
            Err(Error::InvalidTrade(_))
        ));
    }
    let mut quote = fixture_quote();
    quote.request.pool.manager = Address::ZERO;
    assert!(matches!(
        adapter.build_swap(&quote, limits()),
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn discovery_retains_pools_when_another_page_fails() {
    let (adapter, rpc) = adapter().await;
    header(&rpc);
    rpc.push_success(&vec![event_log(ID)]);
    state(&rpc, 1);
    rpc.push_failure_msg("archive unavailable");
    code(&rpc);
    let report = adapter.find_pools(query(), BLOCK).await.unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = report.outcome
    else {
        panic!("expected search");
    };
    assert_eq!(pools, vec![pool()]);
    assert_eq!(
        coverage.completed,
        vec![SearchScope::V4Blocks { from: 10, to: 19 }]
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].scope,
        SearchScope::V4Blocks { from: 20, to: 29 }
    );
    assert!(matches!(failures[0].error, Error::Rpc(_)));
}

#[tokio::test]
async fn a_bad_log_does_not_erase_a_valid_pool_in_the_same_page() {
    let (adapter, rpc) = adapter().await;
    header(&rpc);
    rpc.push_success(&vec![event_log(ID), event_log(B256::ZERO)]);
    state(&rpc, 1);
    code(&rpc);
    let mut query = query();
    query.ranges.truncate(1);
    let report = adapter.find_pools(query, BLOCK).await.unwrap();
    let DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = report.outcome
    else {
        panic!("expected search");
    };
    assert_eq!(pools, vec![pool()]);
    assert!(coverage.completed.is_empty());
    assert_eq!(failures.len(), 1);
}

#[tokio::test]
async fn an_id_needs_matching_initialization_history() {
    let (adapter, rpc) = adapter().await;
    header(&rpc);
    rpc.push_success(&vec![event_log(ID)]);
    state(&rpc, 1);
    code(&rpc);
    assert_eq!(
        adapter
            .resolve_pool(
                V4PoolLookup::Id {
                    id: ID,
                    ranges: vec![10..=19]
                },
                BLOCK
            )
            .await
            .unwrap(),
        pool()
    );
    header(&rpc);
    rpc.push_failure_msg("logs unavailable");
    code(&rpc);
    assert!(matches!(
        adapter
            .resolve_pool(
                V4PoolLookup::Id {
                    id: ID,
                    ranges: vec![10..=19]
                },
                BLOCK
            )
            .await,
        Err(Error::UnresolvedPool(_))
    ));
}

#[tokio::test]
async fn unavailable_pinned_state_invalidates_the_search() {
    let (adapter, rpc) = adapter().await;
    header(&rpc);
    rpc.push_success(&Vec::<Log>::new());
    rpc.push_failure_msg("not canonical");
    let mut query = query();
    query.ranges.truncate(1);
    assert!(matches!(
        adapter.find_pools(query, BLOCK).await,
        Err(Error::Rpc(_))
    ));
}

#[tokio::test]
async fn malformed_and_zero_quotes_are_rejected() {
    let (adapter, rpc) = adapter().await;
    for response in [
        Bytes::new(),
        Bytes::from((U256::ZERO, U256::ZERO).abi_encode()),
    ] {
        state(&rpc, 1);
        rpc.push_success(&response);
        assert!(adapter.quote(request()).await.is_err());
    }
    state(&rpc, 1);
    rpc.push_failure_msg("node unavailable");
    assert!(matches!(adapter.quote(request()).await, Err(Error::Rpc(_))));
}

fn deployment() -> evm_trading_sdk::uniswap::V4Deployment {
    UniswapDeployment::robinhood_mainnet().v4.unwrap()
}
fn key() -> V4PoolKey {
    V4PoolKey {
        currency0: NVDA,
        currency1: AC,
        fee: U24::ZERO,
        tick_spacing: I24::try_from(200).unwrap(),
        hooks: address!("e5e702641ea86f4ae6cc3cdaed2b886f976be044"),
    }
}
fn pool() -> V4Pool {
    V4Pool {
        chain_id: ROBINHOOD_CHAIN_ID,
        manager: deployment().pool_manager,
        key: key(),
    }
}
fn request() -> QuoteRequest<V4Pool, V4QuoteOptions> {
    QuoteRequest {
        pool: pool(),
        trade: ExactInput {
            chain_id: ROBINHOOD_CHAIN_ID,
            currency_in: Currency::Erc20(NVDA),
            currency_out: Currency::Erc20(AC),
            amount_in: U256::from(1000),
            sender: OWNER,
            recipient: RECIPIENT,
        },
        block_hash: BLOCK,
        options: V4QuoteOptions {
            hook_data: Bytes::from_static(b"hook"),
        },
    }
}
fn fixture_quote() -> Quote<V4Pool, V4QuoteOptions> {
    Quote {
        request: request(),
        amount_out: U256::from(900),
    }
}
fn limits() -> SwapLimits {
    SwapLimits {
        minimum_amount_out: U256::from(850),
        deadline_unix_seconds: 1000,
    }
}
fn query() -> V4PoolQuery {
    V4PoolQuery {
        chain_id: ROBINHOOD_CHAIN_ID,
        currency_a: Currency::Erc20(NVDA),
        currency_b: Currency::Erc20(AC),
        ranges: vec![10..=19, 20..=29],
    }
}
fn returned(rpc: &Asserter, value: impl SolValue) {
    rpc.push_success(&Bytes::from(value.abi_encode()));
}
fn code(rpc: &Asserter) {
    rpc.push_success(&Bytes::from_static(&[0x60, 0]));
}
fn state(rpc: &Asserter, price: u64) {
    returned(rpc, (U160::from(price), I24::ZERO, U24::ZERO, U24::ZERO));
}
fn header(rpc: &Asserter) {
    let mut block: Block = Block::default();
    block.header.hash = BLOCK;
    block.header.inner.number = 30;
    rpc.push_success(&block);
}
fn event_log(id: B256) -> Log {
    let key = key();
    let event = Initialize {
        id,
        currency0: NVDA,
        currency1: AC,
        fee: key.fee,
        tickSpacing: key.tick_spacing,
        hooks: key.hooks,
        sqrtPriceX96: U160::from(1),
        tick: I24::ZERO,
    };
    Log {
        inner: alloy_primitives::Log {
            address: deployment().pool_manager,
            data: event.encode_log_data(),
        },
        block_number: Some(12),
        block_hash: Some(B256::repeat_byte(3)),
        ..Default::default()
    }
}
async fn adapter() -> (UniswapV4<RootProvider>, Asserter) {
    let rpc = Asserter::new();
    rpc.push_success(&U64::from(ROBINHOOD_CHAIN_ID));
    let client = EvmClient::connect(
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(rpc.clone()),
        ROBINHOOD_CHAIN_ID,
    )
    .await
    .unwrap();
    code(&rpc);
    code(&rpc);
    for _ in 0..3 {
        returned(&rpc, deployment().pool_manager);
    }
    (
        UniswapV4::connect(client, deployment(), BLOCK)
            .await
            .unwrap(),
        rpc,
    )
}
