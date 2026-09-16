use alloy_primitives::{
    Address, B256, Bytes, U64, U256, address,
    aliases::{I24, U24},
};
use alloy_provider::{ProviderBuilder, RootProvider, mock::Asserter};
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    Error, EvmClient, ROBINHOOD_CHAIN_ID,
    dex::{Currency, ExactInput, SwapLimits},
    fee_router::{FeeRouter, RouteHop, amount_after_fee, input_fee},
    uniswap::{UniswapDeployment, UniswapPool, V3Pool, V4Pool, V4PoolKey},
};

const ROUTER: Address = Address::repeat_byte(5);
const OWNER: Address = Address::repeat_byte(1);
const RECIPIENT: Address = Address::repeat_byte(2);
const WETH: Address = address!("0bd7d308f8e1639fab988df18a8011f41eacad73");
const NVDA: Address = address!("d0601ce157db5bdc3162bbac2a2c8af5320d9eec");
const AC: Address = address!("fad40755de679b262337f8d964d75cac51f8c643");

sol! {
    struct Key { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct Hop { uint8 version; address tokenIn; address tokenOut; address pool; uint24 fee; Key key; bytes hookData; }
    struct Request { address tokenIn; address tokenOut; uint256 amountIn; uint256 minimumAmountOut; address recipient; uint256 deadline; }
    function swap(Request request, Hop[] hops) external payable returns (uint256);
}

#[test]
fn the_input_fee_rounds_down_without_overflow() {
    for amount in [U256::ZERO, U256::from(99), U256::from(100), U256::MAX] {
        assert_eq!(input_fee(amount), amount / U256::from(100));
        assert_eq!(amount_after_fee(amount) + input_fee(amount), amount);
    }
}

#[tokio::test]
async fn the_plan_matches_the_compiled_contract_and_approves_gross_input() {
    let router = router(100).await.unwrap();
    let hops = route();
    let plan = router.build_swap(&trade(), &hops, limits()).unwrap();
    let bytes = plan.transaction.input.input().unwrap();
    assert_eq!(&bytes[..4], &[0xb5, 0x65, 0x99, 0x0a]);
    let call = swapCall::abi_decode_validate(bytes).unwrap();
    assert_eq!(call.request.amountIn, U256::from(10000));
    assert_eq!(call.request.minimumAmountOut, U256::from(39000));
    assert_eq!(call.request.recipient, RECIPIENT);
    assert_eq!(call.request.deadline, U256::from(2000));
    assert_eq!(call.hops.len(), 2);
    assert_eq!(call.hops[0].version, 1);
    assert_eq!(
        call.hops[0].pool,
        match hops[0].pool {
            UniswapPool::V3(ref pool) => pool.address,
            _ => unreachable!(),
        }
    );
    assert_eq!(call.hops[1].version, 2);
    assert_eq!(call.hops[1].key.currency0, NVDA);
    assert_eq!(call.hops[1].hookData.as_ref(), b"hook");
    assert_eq!(plan.transaction.from, Some(OWNER));
    assert_eq!(plan.transaction.to, Some(ROUTER.into()));
    assert_eq!(plan.transaction.value, Some(U256::ZERO));
    let approval = plan.approval.unwrap();
    assert_eq!(
        (
            approval.token,
            approval.owner,
            approval.spender,
            approval.amount
        ),
        (WETH, OWNER, ROUTER, U256::from(10000))
    );
}

#[tokio::test]
async fn native_funding_attaches_gross_value_and_allows_wrapping() {
    let router = router(100).await.unwrap();
    let mut trade = trade();
    trade.currency_in = Currency::Native;
    let plan = router.build_swap(&trade, &route(), limits()).unwrap();
    assert_eq!(plan.transaction.value, Some(trade.amount_in));
    assert!(plan.approval.is_none());
}

#[tokio::test]
async fn routes_cannot_skip_an_intermediate_currency() {
    let router = router(100).await.unwrap();
    let mut hops = route();
    hops.remove(0);
    assert!(matches!(
        router.build_swap(&trade(), &hops, limits()),
        Err(Error::InvalidTrade(_))
    ));
}

#[tokio::test]
async fn a_pool_from_another_manager_is_rejected() {
    let router = router(100).await.unwrap();
    let mut hops = route();
    if let UniswapPool::V4(pool) = &mut hops[1].pool {
        pool.manager = Address::ZERO;
    }
    assert!(matches!(
        router.build_swap(&trade(), &hops, limits()),
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn a_different_fee_contract_is_not_accepted() {
    assert!(matches!(
        router(200).await,
        Err(Error::UnsupportedDeployment(_))
    ));
}

fn trade() -> ExactInput {
    ExactInput {
        chain_id: ROBINHOOD_CHAIN_ID,
        currency_in: Currency::Erc20(WETH),
        currency_out: Currency::Erc20(AC),
        amount_in: U256::from(10000),
        sender: OWNER,
        recipient: RECIPIENT,
    }
}
fn limits() -> SwapLimits {
    SwapLimits {
        minimum_amount_out: U256::from(39000),
        deadline_unix_seconds: 2000,
    }
}
fn route() -> Vec<RouteHop> {
    let config = UniswapDeployment::robinhood_mainnet();
    vec![
        RouteHop {
            pool: UniswapPool::V3(V3Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                factory: config.v3.unwrap().factory,
                address: address!("62ab521f71431f78ac374cdbadc6cda3c8916b6c"),
                token0: WETH,
                token1: NVDA,
                fee: U24::from(500),
                tick_spacing: I24::try_from(10).unwrap(),
            }),
            currency_in: Currency::Erc20(WETH),
            currency_out: Currency::Erc20(NVDA),
            hook_data: Bytes::new(),
        },
        RouteHop {
            pool: UniswapPool::V4(V4Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                manager: config.v4.unwrap().pool_manager,
                key: V4PoolKey {
                    currency0: NVDA,
                    currency1: AC,
                    fee: U24::ZERO,
                    tick_spacing: I24::try_from(200).unwrap(),
                    hooks: address!("e5e702641ea86f4ae6cc3cdaed2b886f976be044"),
                },
            }),
            currency_in: Currency::Erc20(NVDA),
            currency_out: Currency::Erc20(AC),
            hook_data: Bytes::from_static(b"hook"),
        },
    ]
}
async fn router(fee: u64) -> Result<FeeRouter<RootProvider>, Error> {
    let rpc = Asserter::new();
    rpc.push_success(&U64::from(ROBINHOOD_CHAIN_ID));
    let client = EvmClient::connect(
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(rpc.clone()),
        ROBINHOOD_CHAIN_ID,
    )
    .await?;
    rpc.push_success(&Bytes::from_static(&[0x60, 0]));
    rpc.push_success(&Bytes::from(U256::from(fee).abi_encode()));
    let config = UniswapDeployment::robinhood_mainnet();
    for address in [
        RECIPIENT,
        WETH,
        config.v2.unwrap().factory,
        config.v3.unwrap().factory,
        config.v4.unwrap().pool_manager,
    ] {
        rpc.push_success(&Bytes::from(address.abi_encode()));
    }
    FeeRouter::connect(client, ROUTER, B256::repeat_byte(7)).await
}
