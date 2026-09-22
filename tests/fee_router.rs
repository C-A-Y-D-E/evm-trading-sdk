use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::{
    Address, B256, Bytes, U64, U256, U512, address,
    aliases::{I24, U24, U160},
};
use alloy_provider::{ProviderBuilder, RootProvider, mock::Asserter};
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    BuyWith, Error, EvmClient, PairSwap, ROBINHOOD_CHAIN_ID, TradeSide,
    dex::{Currency, ExactInput, Quote, QuoteRequest, SwapLimits},
    fee_router::{FeeRouter, ROBINHOOD_FEE_ROUTER, RouteHop, amount_after_fee, input_fee},
    uniswap::v4::{V4PoolLookup, V4QuoteOptions},
    uniswap::{UniswapDeployment, UniswapPool, V2Pool, V3Pool, V4Pool, V4PoolKey},
};
use serde_json::{Value, json};

const ROUTER: Address = ROBINHOOD_FEE_ROUTER;
const OWNER: Address = Address::repeat_byte(1);
const RECIPIENT: Address = Address::repeat_byte(2);
const WETH: Address = address!("0bd7d308f8e1639fab988df18a8011f41eacad73");
const NVDA: Address = address!("d0601ce157db5bdc3162bbac2a2c8af5320d9eec");
const AC: Address = address!("fad40755de679b262337f8d964d75cac51f8c643");

sol! {
    struct Key { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct Hop { uint8 version; address tokenIn; address tokenOut; address pool; uint24 fee; Key key; bytes hookData; }
    struct Request { address tokenIn; address tokenOut; uint256 amountIn; uint256 minimumAmountOut; address recipient; uint256 deadline; }
    function swapWithFee(Request request, Hop[] hops, uint256 expectedFeeBps) external payable returns (uint256);
}

#[test]
fn the_input_fee_rounds_down_without_overflow() {
    for fee in [0, 1, 100, 333, 9999, 10000] {
        for amount in [U256::ZERO, U256::from(99), U256::from(10001), U256::MAX] {
            let expected = (U512::from(amount) * U512::from(fee) / U512::from(10000)).to::<U256>();
            assert_eq!(input_fee(amount, fee).unwrap(), expected);
            assert_eq!(amount_after_fee(amount, fee).unwrap() + expected, amount);
        }
    }
    assert!(input_fee(U256::MAX, 10001).is_err());
    assert!(amount_after_fee(U256::MAX, 10001).is_err());
}

#[tokio::test]
async fn router_reads_are_in_flight_together_at_the_same_block() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let server = thread::spawn(move || {
        let (mut stream, request) = receive_rpc(&listener);
        assert_eq!(request["method"], "eth_chainId");
        respond_rpc(&mut stream, &request, json!("0x1237"));
        // Withhold responses until all reads arrive; serial configuration reads cannot pass.
        let requests: Vec<_> = (0..7).map(|_| receive_rpc(&listener)).collect();
        let config = UniswapDeployment::robinhood_mainnet();
        let fields = [
            ("FEE_BPS()", U256::from(100).abi_encode()),
            ("feeRecipient()", RECIPIENT.abi_encode()),
            ("wrappedNative()", WETH.abi_encode()),
            ("v2Factory()", config.v2.unwrap().factory.abi_encode()),
            ("v3Factory()", config.v3.unwrap().factory.abi_encode()),
            (
                "poolManager()",
                config.v4.unwrap().pool_manager.abi_encode(),
            ),
        ];
        let mut selectors = Vec::new();
        for (mut stream, request) in requests.into_iter().rev() {
            assert_eq!(
                request["params"][1],
                json!({
                    "blockHash": B256::repeat_byte(7), "requireCanonical": true,
                })
            );
            let result = if request["method"] == "eth_getCode" {
                assert_eq!(request["params"][0], json!(ROUTER));
                json!("0x6000")
            } else {
                assert_eq!(request["method"], "eth_call");
                assert_eq!(request["params"][0]["to"], json!(ROUTER));
                let transaction = &request["params"][0];
                let input = transaction
                    .get("input")
                    .or_else(|| transaction.get("data"))
                    .unwrap();
                let (_, value) = fields
                    .iter()
                    .find(|(signature, _)| {
                        *input
                            == json!(Bytes::copy_from_slice(
                                &alloy_primitives::keccak256(signature.as_bytes())[..4]
                            ))
                    })
                    .unwrap();
                selectors.push(input.clone());
                json!(Bytes::copy_from_slice(value))
            };
            respond_rpc(&mut stream, &request, result);
        }
        selectors.sort_by_key(Value::to_string);
        selectors.dedup();
        assert_eq!(selectors.len(), 6);
    });
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url);
    let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
        .await
        .unwrap();
    let result = FeeRouter::connect(client, B256::repeat_byte(7)).await;
    server.join().unwrap();
    let router = result.unwrap();
    assert_eq!(router.fee_bps(), 100);
    assert_eq!(router.fee_recipient(), RECIPIENT);
}

fn receive_rpc(listener: &TcpListener) -> (TcpStream, Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "concurrent RPC did not arrive");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("mock RPC accept failed: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    (reader.into_inner(), serde_json::from_slice(&body).unwrap())
}

fn respond_rpc(stream: &mut TcpStream, request: &Value, result: Value) {
    let body = json!({"jsonrpc":"2.0", "id":request["id"], "result":result}).to_string();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
}

#[tokio::test]
async fn the_plan_matches_the_compiled_contract_and_approves_gross_input() {
    let router = router(100).await.unwrap();
    let hops = route();
    let plan = router
        .build_swap(&quote(trade(), hops.clone()), limits())
        .unwrap();
    let bytes = plan.transaction.input.input().unwrap();
    assert_eq!(&bytes[..4], &[0x14, 0xf9, 0x43, 0xf5]);
    let call = swapWithFeeCall::abi_decode_validate(bytes).unwrap();
    assert_eq!(call.expectedFeeBps, U256::from(100));
    assert_eq!(call.request.amountIn, U256::from(10000));
    assert_eq!(call.request.minimumAmountOut, U256::from(39204));
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
async fn every_pool_version_builds_a_swap_to_our_deployed_router() {
    let router = router(100).await.unwrap();
    assert_eq!(
        router.address(),
        address!("dda62d6b81689bede84170d1a88838d1f7ab1d4d")
    );
    let config = UniswapDeployment::robinhood_mainnet();
    let mut hops = route();
    hops.push(RouteHop {
        pool: UniswapPool::V2(V2Pool {
            chain_id: ROBINHOOD_CHAIN_ID,
            factory: config.v2.unwrap().factory,
            address: Address::repeat_byte(9),
            token0: WETH,
            token1: NVDA,
        }),
        currency_in: Currency::Erc20(WETH),
        currency_out: Currency::Erc20(NVDA),
        hook_data: Bytes::new(),
    });
    for hop in hops {
        let trade = ExactInput {
            currency_in: hop.currency_in,
            currency_out: hop.currency_out,
            ..trade()
        };
        let plan = router
            .build_swap(&quote(trade, vec![hop]), limits())
            .unwrap();
        assert_eq!(plan.transaction.to, Some(ROUTER.into()));
        assert_eq!(plan.approval.unwrap().spender, ROUTER);
        let call =
            swapWithFeeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
        assert_eq!(call.expectedFeeBps, U256::from(100));
    }
}

#[tokio::test]
async fn a_chain_without_our_router_is_not_configured() {
    let rpc = Asserter::new();
    rpc.push_success(&U64::from(1));
    let client = EvmClient::connect(
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(rpc),
        1,
    )
    .await
    .unwrap();
    assert!(matches!(
        FeeRouter::connect(client, B256::repeat_byte(7)).await,
        Err(Error::NotConfigured {
            chain_id: 1,
            requirement: "FeeRouter"
        })
    ));
}

#[tokio::test]
async fn native_funding_attaches_gross_value_and_allows_wrapping() {
    let router = router(100).await.unwrap();
    let mut trade = trade();
    trade.currency_in = Currency::Native;
    let plan = router
        .build_swap(&quote(trade.clone(), route()), limits())
        .unwrap();
    assert_eq!(plan.transaction.value, Some(trade.amount_in));
    assert!(plan.approval.is_none());
}

#[tokio::test]
async fn routes_cannot_skip_an_intermediate_currency() {
    let router = router(100).await.unwrap();
    let mut hops = route();
    hops.remove(0);
    assert!(matches!(
        router.build_swap(&quote(trade(), hops), limits()),
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
        router.build_swap(&quote(trade(), hops), limits()),
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn an_out_of_range_fee_is_not_accepted() {
    assert!(matches!(
        router(10001).await,
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn the_plan_preserves_the_fee_read_at_the_quote_block() {
    let router = router(333).await.unwrap();
    assert_eq!(router.fee_bps(), 333);
    assert_eq!(router.configuration_block(), B256::repeat_byte(7));
    assert_eq!(router.input_fee(U256::from(10001)), U256::from(333));
    assert_eq!(router.amount_after_fee(U256::from(10001)), U256::from(9668));
    let plan = router
        .build_swap(&quote(trade(), route()), limits())
        .unwrap();
    let call =
        swapWithFeeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.expectedFeeBps, U256::from(333));
}

#[tokio::test]
async fn zero_fees_work_and_full_input_fees_cannot_build_a_swap() {
    let free = router(0).await.unwrap();
    assert_eq!(free.amount_after_fee(trade().amount_in), trade().amount_in);
    assert!(free.build_swap(&quote(trade(), route()), limits()).is_ok());
    let full = router(10000).await.unwrap();
    assert!(matches!(
        full.build_swap(&quote(trade(), route()), limits()),
        Err(Error::InvalidTrade(_))
    ));
}

#[tokio::test]
async fn slippage_uses_the_final_quote_without_deducting_the_fee_twice() {
    let router = router(100).await.unwrap();
    let mut quote = quote(trade(), route());
    quote.amount_out = U256::from(10001);
    let plan = router.build_swap(&quote, limits()).unwrap();
    let call =
        swapWithFeeCall::abi_decode_validate(plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.request.minimumAmountOut, U256::from(9900));
    assert_eq!(call.request.amountIn, quote.request.trade.amount_in);
    assert_eq!(plan.approval.unwrap().amount, quote.request.trade.amount_in);
}

#[tokio::test]
async fn slippage_and_quote_block_are_checked_before_building() {
    let router = router(100).await.unwrap();
    let mut quote = quote(trade(), route());
    let invalid = SwapLimits {
        slippage_bps: 10001,
        ..limits()
    };
    assert!(matches!(
        router.build_swap(&quote, invalid),
        Err(Error::InvalidTrade(_))
    ));
    quote.request.block_hash = B256::repeat_byte(8);
    assert!(matches!(
        router.build_swap(&quote, limits()),
        Err(Error::InvalidTrade(_))
    ));
}

fn quote(trade: ExactInput, hops: Vec<RouteHop>) -> Quote<Vec<RouteHop>> {
    Quote {
        request: QuoteRequest {
            pool: hops,
            trade,
            block_hash: B256::repeat_byte(7),
            options: (),
        },
        amount_out: U256::from(39600),
    }
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
        slippage_bps: 100,
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
    router_with_rpc(fee).await.map(|(router, _)| router)
}

async fn router_with_rpc(fee: u64) -> Result<(FeeRouter<RootProvider>, Asserter), Error> {
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
    Ok((FeeRouter::connect(client, B256::repeat_byte(7)).await?, rpc))
}

#[tokio::test]
async fn sells_infer_both_target_positions_and_keep_gross_approvals() {
    for target in [WETH, NVDA] {
        let side = TradeSide::Sell;
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        v3_connection(&rpc);
        v3_pool(&rpc);
        v3_pool(&rpc);
        returned(
            &rpc,
            (U256::from(2000), U160::from(1), 0u32, U256::from(50000)),
        );
        let request = pair_request(target, side);
        v3_price(&rpc);
        let prepared = router
            .trader(OWNER)
            .prepare_swap(request, limits())
            .await
            .unwrap();
        let other = if target == WETH { NVDA } else { WETH };
        let (input, output) = match side {
            TradeSide::Buy => (other, target),
            TradeSide::Sell => (target, other),
        };
        let call =
            swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
        assert_eq!(call.request.tokenIn, input);
        assert_eq!(call.request.tokenOut, output);
        assert_eq!(call.request.amountIn, U256::from(10000));
        assert_eq!(call.request.minimumAmountOut, U256::from(1980));
        assert_eq!(call.expectedFeeBps, U256::from(100));
        assert_eq!(call.request.recipient, OWNER);
        assert_eq!(prepared.plan.transaction.to, Some(ROUTER.into()));
        assert_eq!(prepared.quote.request.block_hash, B256::repeat_byte(7));
        let approval = prepared.plan.approval.unwrap();
        assert_eq!(
            (approval.token, approval.spender, approval.amount),
            (input, ROUTER, U256::from(10000))
        );
    }
}

#[tokio::test]
async fn native_settlement_changes_endpoints_but_keeps_weth_in_the_hop() {
    for side in [TradeSide::Buy, TradeSide::Sell] {
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        v3_connection(&rpc);
        v3_pool(&rpc);
        v3_pool(&rpc);
        returned(
            &rpc,
            (U256::from(2000), U160::from(1), 0u32, U256::from(50000)),
        );
        v3_price(&rpc);
        let prepared = router
            .trader(OWNER)
            .with_native_settlement()
            .prepare_swap(pair_request(NVDA, side), limits())
            .await
            .unwrap();
        let call =
            swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
        match side {
            TradeSide::Buy => {
                assert_eq!(call.request.tokenIn, Address::ZERO);
                assert_eq!(call.hops[0].tokenIn, WETH);
                assert_eq!(prepared.plan.transaction.value, Some(U256::from(10000)));
                assert!(prepared.plan.approval.is_none());
            }
            TradeSide::Sell => {
                assert_eq!(call.request.tokenOut, Address::ZERO);
                assert_eq!(call.hops[0].tokenOut, WETH);
                assert_eq!(prepared.plan.transaction.value, Some(U256::ZERO));
                assert_eq!(prepared.plan.approval.unwrap().token, NVDA);
            }
        }
    }
}

#[tokio::test]
async fn pair_swaps_reject_unrelated_tokens_and_unknown_factories() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    v3_connection(&rpc);
    v3_pool(&rpc);
    assert!(
        matches!(router.trader(OWNER).prepare_swap(pair_request(AC, TradeSide::Buy), limits()).await,
        Err(Error::InvalidTrade(message)) if message.contains("target token"))
    );
    returned(&rpc, Address::repeat_byte(9));
    assert!(matches!(
        router
            .trader(OWNER)
            .prepare_swap(pair_request(NVDA, TradeSide::Buy), limits())
            .await,
        Err(Error::UnsupportedDeployment(_))
    ));
}

#[tokio::test]
async fn pair_swaps_reject_invalid_limits_wallets_and_empty_input_before_rpc() {
    let (router, _) = router_with_rpc(100).await.unwrap();
    let request = pair_request(NVDA, TradeSide::Buy);
    let invalid_limits = SwapLimits {
        slippage_bps: 10001,
        ..limits()
    };
    assert!(matches!(
        router
            .trader(OWNER)
            .prepare_swap(request.clone(), invalid_limits)
            .await,
        Err(Error::InvalidTrade(_))
    ));
    assert!(matches!(
        router
            .trader(Address::ZERO)
            .prepare_swap(request.clone(), limits())
            .await,
        Err(Error::InvalidTrade(_))
    ));
    let empty = PairSwap {
        amount_in: U256::ZERO,
        ..request
    };
    assert!(matches!(
        router.trader(OWNER).prepare_swap(empty, limits()).await,
        Err(Error::InvalidTrade(_))
    ));
}

#[tokio::test]
async fn v2_pair_selection_quotes_the_input_after_fees() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let config = UniswapDeployment::robinhood_mainnet().v2.unwrap();
    let token = address!("c6911796042b15d7fa4f6cde69e245ddcd3d9c31");
    let pair = address!("d95e8e2cd04c207625c6f23c974d365a5f3a91d3");
    returned(&rpc, config.factory);
    code(&rpc);
    returned(&rpc, config.factory);
    returned(&rpc, WETH);
    code(&rpc);
    for _ in 0..2 {
        for address in [config.factory, WETH, token, pair] {
            returned(&rpc, address);
        }
    }
    returned(&rpc, vec![U256::from(9900), U256::from(2000)]);
    returned(
        &rpc,
        (
            alloy_primitives::aliases::U112::from(100000),
            alloy_primitives::aliases::U112::from(100000),
            0u32,
        ),
    );
    let prepared = router
        .trader(OWNER)
        .prepare_swap(
            PairSwap {
                pair,
                token_address: token,
                side: TradeSide::Buy,
                amount_in: U256::from(10000),
            },
            limits(),
        )
        .await
        .unwrap();
    let call =
        swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.hops[0].version, 0);
    assert_eq!(call.hops[0].pool, pair);
    assert_eq!(prepared.quote.amount_out, U256::from(2000));
    assert_eq!(call.request.amountIn, U256::from(10000));
}

#[tokio::test]
async fn v4_pair_selection_preserves_the_key_and_hook_data() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let config = UniswapDeployment::robinhood_mainnet().v4.unwrap();
    code(&rpc);
    code(&rpc);
    for _ in 0..3 {
        returned(&rpc, config.pool_manager);
    }
    for _ in 0..2 {
        returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
    }
    returned(&rpc, (U256::from(2000), U256::from(50000)));
    returned(&rpc, (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO));
    pons_launch(&rpc, AC, NVDA, 300);
    let UniswapPool::V4(pool) = route().remove(1).pool else {
        unreachable!()
    };
    let hook_data = Bytes::from_static(b"hook");
    let prepared = router
        .trader(OWNER)
        .prepare_v4_swap(
            PairSwap {
                pair: V4PoolLookup::Key(pool.key.clone()),
                token_address: NVDA,
                side: TradeSide::Sell,
                amount_in: U256::from(10000),
            },
            V4QuoteOptions {
                hook_data: hook_data.clone(),
            },
            limits(),
        )
        .await
        .unwrap();
    let call =
        swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.hops[0].version, 2);
    assert_eq!(call.hops[0].key.hooks, pool.key.hooks);
    assert_eq!(call.hops[0].hookData, hook_data);
    assert_eq!(call.request.tokenIn, NVDA);
    assert_eq!(call.request.tokenOut, AC);
    let impact = prepared.price_impact.unwrap();
    assert_eq!(impact.hook_fees[0].as_ref().unwrap().hook_fee_bps, 100);
    assert_eq!(impact.hook_fees[0].as_ref().unwrap().creator_tax_bps, 300);
}

fn pons_launch(rpc: &Asserter, memecoin: Address, quote: Address, tax: u16) {
    returned(
        rpc,
        (
            true,
            memecoin < quote,
            memecoin,
            quote,
            OWNER,
            OWNER,
            RECIPIENT,
            tax,
            3000u16,
            5000u16,
            100u16,
            300u16,
            false,
        ),
    );
}

fn pair_request(token_address: Address, side: TradeSide) -> PairSwap {
    PairSwap {
        pair: address!("62ab521f71431f78ac374cdbadc6cda3c8916b6c"),
        token_address,
        side,
        amount_in: U256::from(10000),
    }
}

#[tokio::test]
async fn eth_buy_builds_one_funded_v4_transaction_and_keeps_discovery_failures() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let request = v4_buy_setup(&rpc);
    funding_connection(&rpc);
    rpc.push_failure_msg("fee tier unavailable");
    returned(&rpc, pair_request(NVDA, TradeSide::Buy).pair);
    v3_pool(&rpc);
    returned(&rpc, Address::ZERO);
    returned(&rpc, Address::ZERO);
    v3_pool(&rpc);
    returned(
        &rpc,
        (U256::from(990), U160::from(1), 0u32, U256::from(50000)),
    );
    returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
    returned(&rpc, (U256::from(2000), U256::from(50000)));
    let prepared = router
        .trader(OWNER)
        .with_native_buy()
        .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
        .await
        .unwrap();
    let call =
        swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.request.tokenIn, Address::ZERO);
    assert_eq!(call.request.amountIn, U256::from(10000));
    assert_eq!(prepared.plan.transaction.value, Some(U256::from(10000)));
    assert!(prepared.plan.approval.is_none());
    assert_eq!(call.expectedFeeBps, U256::from(100));
    assert_eq!(call.hops.len(), 2);
    assert_eq!((call.hops[0].tokenIn, call.hops[0].tokenOut), (WETH, NVDA));
    assert_eq!((call.hops[1].tokenIn, call.hops[1].tokenOut), (NVDA, AC));
    assert_eq!(call.request.minimumAmountOut, U256::from(1980));
    assert_eq!(prepared.quote.request.block_hash, B256::repeat_byte(7));
    let report = prepared.funding.unwrap();
    assert!(report.quote_failures.is_empty());
    let evm_trading_sdk::uniswap::DiscoveryOutcome::Searched {
        coverage, failures, ..
    } = report.discovery.outcome
    else {
        unreachable!()
    };
    assert_eq!(coverage.requested.len(), 4);
    assert_eq!(coverage.completed.len(), 3);
    assert_eq!(failures.len(), 1);
    assert!(matches!(failures[0].error, Error::Rpc(_)));
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn funded_preview_combines_both_pool_prices_and_fees() {
    for (hooked, output, after_fees, price_impact, total_cost, minimum) in [
        (false, 8905, 9895, 1000, 1095, 8815),
        (true, 8549, 9499, 1000, 1451, 8463),
    ] {
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        let mut request = v4_buy_setup(&rpc);
        let V4PoolLookup::Key(key) = &mut request.pair else {
            unreachable!()
        };
        if !hooked {
            key.hooks = Address::ZERO;
        }
        funding_connection(&rpc);
        returned(&rpc, Address::ZERO);
        returned(&rpc, pair_request(NVDA, TradeSide::Buy).pair);
        v3_pool(&rpc);
        returned(&rpc, Address::ZERO);
        returned(&rpc, Address::ZERO);
        v3_pool(&rpc);
        returned(
            &rpc,
            (U256::from(9000), U160::from(1), 0u32, U256::from(50000)),
        );
        returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
        returned(&rpc, (U256::from(output), U256::from(50000)));
        v3_price(&rpc);
        returned(&rpc, (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO));
        if hooked {
            pons_launch(&rpc, AC, NVDA, 300);
        }

        let prepared = router
            .trader(OWNER)
            .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
            .await
            .unwrap();
        let impact = prepared.price_impact.as_ref().unwrap();
        assert_eq!(impact.market_amount_out, U256::from(9900));
        assert_eq!(
            impact.market_amount_out_after_fees,
            Some(U256::from(after_fees))
        );
        assert_eq!(impact.price_impact_bps, price_impact);
        assert_eq!(impact.total_cost_bps, total_cost);
        assert_eq!(impact.pool_fee_pips, [Some(500), Some(0)]);
        assert_eq!(
            impact.hook_fees[1].as_ref().unwrap().hook_fee_bps,
            if hooked { 100 } else { 0 }
        );
        assert_eq!(
            impact.hook_fees[1].as_ref().unwrap().creator_tax_bps,
            if hooked { 300 } else { 0 }
        );
        assert_eq!(prepared.quote.request.pool.len(), 2);
        assert_eq!(prepared.router_fee_amount, U256::from(100));
        assert_eq!(prepared.minimum_amount_out(), U256::from(minimum));
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn missing_price_state_keeps_the_output_quote_and_reason() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    v3_connection(&rpc);
    v3_pool(&rpc);
    v3_pool(&rpc);
    returned(
        &rpc,
        (U256::from(2000), U160::from(1), 0u32, U256::from(50000)),
    );
    rpc.push_failure_msg("slot0 unavailable");
    let prepared = router
        .trader(OWNER)
        .prepare_swap(pair_request(NVDA, TradeSide::Buy), limits())
        .await
        .unwrap();
    assert!(matches!(
        prepared.price_impact,
        Err(evm_trading_sdk::PriceImpactUnavailable::State(_))
    ));
    assert_eq!(prepared.quote.amount_out, U256::from(2000));
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn eth_buy_without_a_funding_route_returns_the_search_report() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let request = v4_buy_setup(&rpc);
    funding_connection(&rpc);
    for _ in 0..4 {
        returned(&rpc, Address::ZERO);
    }
    let error = router
        .trader(OWNER)
        .with_native_buy()
        .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
        .await
        .unwrap_err();
    let Error::NoFundingRoute(report) = error else {
        panic!("funding report was lost")
    };
    let evm_trading_sdk::uniswap::DiscoveryOutcome::Searched {
        pools,
        coverage,
        failures,
    } = report.discovery.outcome
    else {
        unreachable!()
    };
    assert!(pools.is_empty() && failures.is_empty());
    assert_eq!(coverage.completed.len(), 4);
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn eth_buy_keeps_the_final_hop_failure() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let request = v4_buy_setup(&rpc);
    funding_connection(&rpc);
    returned(&rpc, Address::ZERO);
    returned(&rpc, pair_request(NVDA, TradeSide::Buy).pair);
    v3_pool(&rpc);
    returned(&rpc, Address::ZERO);
    returned(&rpc, Address::ZERO);
    v3_pool(&rpc);
    returned(
        &rpc,
        (U256::from(990), U160::from(1), 0u32, U256::from(50000)),
    );
    returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
    rpc.push_failure_msg("target quote unavailable");
    let error = router
        .trader(OWNER)
        .with_native_buy()
        .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
        .await
        .unwrap_err();
    let Error::NoFundingRoute(report) = error else {
        panic!("funding failure was lost")
    };
    assert_eq!(report.quote_failures.len(), 1);
    assert_eq!(
        report.quote_failures[0].pool,
        pair_request(NVDA, TradeSide::Buy).pair
    );
    assert!(matches!(report.quote_failures[0].error, Error::Rpc(_)));
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn native_buy_uses_a_direct_weth_pair_without_changing_sell_input() {
    for side in [TradeSide::Buy, TradeSide::Sell] {
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        v3_connection(&rpc);
        v3_pool(&rpc);
        v3_pool(&rpc);
        returned(
            &rpc,
            (U256::from(2000), U160::from(1), 0u32, U256::from(50000)),
        );
        v3_price(&rpc);
        let prepared = router
            .trader(OWNER)
            .buy_with(BuyWith::Native)
            .prepare_swap(pair_request(NVDA, side), limits())
            .await
            .unwrap();
        assert!(prepared.funding.is_none());
        assert_eq!(prepared.quote.request.pool.len(), 1);
        assert_eq!(
            prepared.quote.request.trade.currency_in,
            if side == TradeSide::Buy {
                Currency::Native
            } else {
                Currency::Erc20(NVDA)
            }
        );
        assert_eq!(prepared.plan.approval.is_none(), side == TradeSide::Buy);
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn buy_currency_does_not_restrict_sell_input() {
    for currency in [BuyWith::Native, BuyWith::Usdc] {
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        v3_connection(&rpc);
        v3_pool(&rpc);
        v3_pool(&rpc);
        returned(
            &rpc,
            (U256::from(2000), U160::from(1), 0u32, U256::from(50000)),
        );
        v3_price(&rpc);
        let prepared = router
            .trader(OWNER)
            .with_native_settlement()
            .buy_with(currency)
            .prepare_swap(pair_request(NVDA, TradeSide::Sell), limits())
            .await
            .unwrap();
        let input = NVDA;
        let approval = prepared.plan.approval.unwrap();
        assert_eq!(approval.token, input);
        assert_eq!(approval.amount, U256::from(10000));
        assert_eq!(approval.spender, ROUTER);
        assert_eq!(prepared.plan.transaction.value, Some(U256::ZERO));
        assert_eq!(
            prepared.quote.request.trade.currency_in,
            Currency::Erc20(input)
        );
        assert!(prepared.funding.is_none());
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn native_v4_buy_uses_no_funding_hop() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let mut request = v4_buy_setup(&rpc);
    let V4PoolLookup::Key(key) = &mut request.pair else {
        unreachable!()
    };
    key.currency0 = Address::ZERO;
    returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
    returned(&rpc, (U256::from(2000), U256::from(50000)));
    returned(&rpc, (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO));
    pons_launch(&rpc, AC, Address::ZERO, 0);
    let prepared = router
        .trader(OWNER)
        .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
        .await
        .unwrap();
    let call =
        swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
    assert_eq!(call.request.tokenIn, Address::ZERO);
    assert_eq!(call.request.amountIn, U256::from(10000));
    assert_eq!(call.expectedFeeBps, U256::from(100));
    assert_eq!(call.request.minimumAmountOut, U256::from(1980));
    assert_eq!(prepared.plan.transaction.value, Some(U256::from(10000)));
    assert!(prepared.plan.approval.is_none());
    assert_eq!(call.hops.len(), 1);
    assert_eq!(
        (call.hops[0].tokenIn, call.hops[0].tokenOut),
        (Address::ZERO, AC)
    );
    assert!(prepared.funding.is_none());
    assert!(prepared.price_impact.is_ok());
    assert!(rpc.read_q().is_empty());
}

#[tokio::test]
async fn unknown_v4_fees_keep_the_quote_without_claiming_fee_excluded_impact() {
    for dynamic in [false, true] {
        let (router, rpc) = router_with_rpc(100).await.unwrap();
        let mut request = v4_buy_setup(&rpc);
        let V4PoolLookup::Key(key) = &mut request.pair else {
            unreachable!()
        };
        key.currency0 = Address::ZERO;
        key.hooks = Address::repeat_byte(9);
        if dynamic {
            key.fee = U24::from(0x800000);
        }
        let expected_key = key.clone();
        returned(&rpc, (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO));
        returned(&rpc, (U256::from(5000), U256::from(50000)));
        returned(&rpc, (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO));
        let prepared = router
            .trader(OWNER)
            .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
            .await
            .unwrap();
        assert!(matches!(
            prepared.price_impact,
            Err(evm_trading_sdk::PriceImpactUnavailable::HookPricing)
        ));
        assert_eq!(prepared.quote.amount_out, U256::from(5000));
        assert_eq!(prepared.minimum_amount_out(), U256::from(4950));
        let call =
            swapWithFeeCall::abi_decode(prepared.plan.transaction.input.input().unwrap()).unwrap();
        assert_eq!(call.hops[0].key.hooks, expected_key.hooks);
        assert_eq!(call.hops[0].key.fee, expected_key.fee);
        assert!(rpc.read_q().is_empty());
    }
}

#[tokio::test]
async fn usdc_buy_requires_a_verified_chain_address_before_rpc() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let error = router
        .trader(OWNER)
        .buy_with(BuyWith::Usdc)
        .prepare_swap(pair_request(NVDA, TradeSide::Buy), limits())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::NotConfigured {
            requirement: "verified USDC buy currency",
            ..
        }
    ));
    assert!(rpc.read_q().is_empty());
}

fn v4_buy_setup(rpc: &Asserter) -> PairSwap<V4PoolLookup> {
    let config = UniswapDeployment::robinhood_mainnet().v4.unwrap();
    code(rpc);
    code(rpc);
    for _ in 0..3 {
        returned(rpc, config.pool_manager);
    }
    returned(rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
    let UniswapPool::V4(pool) = route().remove(1).pool else {
        unreachable!()
    };
    PairSwap {
        pair: V4PoolLookup::Key(pool.key),
        token_address: AC,
        side: TradeSide::Buy,
        amount_in: U256::from(10000),
    }
}

fn funding_connection(rpc: &Asserter) {
    let factory = UniswapDeployment::robinhood_mainnet().v3.unwrap().factory;
    code(rpc);
    for address in [factory, WETH, factory, WETH] {
        returned(rpc, address);
    }
    code(rpc);
}

#[tokio::test]
async fn eth_funding_compares_final_output_instead_of_intermediate_output() {
    let (router, rpc) = router_with_rpc(100).await.unwrap();
    let request = v4_buy_setup(&rpc);
    funding_connection(&rpc);
    for fee in [100, 500] {
        returned(&rpc, funding_pool_address(fee));
        funding_pool(&rpc, fee);
    }
    returned(&rpc, Address::ZERO);
    returned(&rpc, Address::ZERO);
    for (fee, intermediate, final_output) in [(100, 1000, 3000), (500, 2000, 2000)] {
        funding_pool(&rpc, fee);
        returned(
            &rpc,
            (
                U256::from(intermediate),
                U160::from(1),
                0u32,
                U256::from(50000),
            ),
        );
        returned(&rpc, (U160::from(1), I24::ZERO, U24::ZERO, U24::ZERO));
        returned(&rpc, (U256::from(final_output), U256::from(50000)));
    }
    let prepared = router
        .trader(OWNER)
        .with_native_buy()
        .prepare_v4_swap(request, V4QuoteOptions::default(), limits())
        .await
        .unwrap();
    let UniswapPool::V3(pool) = &prepared.quote.request.pool[0].pool else {
        unreachable!()
    };
    assert_eq!(pool.fee, U24::from(100));
    assert_eq!(prepared.quote.amount_out, U256::from(3000));
    assert!(rpc.read_q().is_empty());
}

fn funding_pool_address(fee: u32) -> Address {
    let factory = UniswapDeployment::robinhood_mainnet().v3.unwrap().factory;
    factory.create2(
        alloy_primitives::keccak256((WETH, NVDA, U24::from(fee)).abi_encode()),
        alloy_primitives::b256!("e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54"),
    )
}

fn funding_pool(rpc: &Asserter, fee: u32) {
    for address in [
        UniswapDeployment::robinhood_mainnet().v3.unwrap().factory,
        WETH,
        NVDA,
    ] {
        returned(rpc, address);
    }
    returned(rpc, U24::from(fee));
    returned(rpc, I24::try_from(if fee == 100 { 1 } else { 10 }).unwrap());
    returned(rpc, funding_pool_address(fee));
}

fn v3_connection(rpc: &Asserter) {
    let factory = UniswapDeployment::robinhood_mainnet().v3.unwrap().factory;
    returned(rpc, factory);
    code(rpc);
    for address in [factory, WETH, factory, WETH] {
        returned(rpc, address);
    }
    code(rpc);
}

fn v3_pool(rpc: &Asserter) {
    for address in [
        UniswapDeployment::robinhood_mainnet().v3.unwrap().factory,
        WETH,
        NVDA,
    ] {
        returned(rpc, address);
    }
    returned(rpc, U24::from(500));
    returned(rpc, I24::try_from(10).unwrap());
    returned(rpc, pair_request(NVDA, TradeSide::Buy).pair);
}

fn returned(rpc: &Asserter, value: impl SolValue) {
    rpc.push_success(&Bytes::from(value.abi_encode()));
}

fn v3_price(rpc: &Asserter) {
    returned(
        rpc,
        (U160::from(1) << 96, I24::ZERO, 0u16, 0u16, 0u16, 0u16, true),
    );
}

fn code(rpc: &Asserter) {
    rpc.push_success(&Bytes::from_static(&[0x60, 0]));
}
