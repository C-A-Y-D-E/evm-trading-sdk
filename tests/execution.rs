use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_primitives::{
    Address, B256, Bytes, U256, address,
    aliases::{I24, U24, U160},
};
use alloy_provider::{ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::{Block, Transaction};
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    BuyWith, Error, EvmClient, FeeRouter, PairSwap, ROBINHOOD_CHAIN_ID, TradeSide,
    dex::SwapLimits,
    execution::{
        Execution, ExecutionFailure, ExecutionJournal, ExecutionOptions, ExecutionRecord,
        ExecutionStage, ExecutionStatus, SubmissionState, TradeRequest, TransactionKind,
    },
    fee_router::ROBINHOOD_FEE_ROUTER,
    uniswap::UniswapDeployment,
};
use serde_json::{Value, json};

const WALLET: Address = Address::repeat_byte(1);
const WETH: Address = address!("0bd7d308f8e1639fab988df18a8011f41eacad73");
const TOKEN: Address = address!("d0601ce157db5bdc3162bbac2a2c8af5320d9eec");
const POOL: Address = address!("62ab521f71431f78ac374cdbadc6cda3c8916b6c");

#[derive(Clone, Copy, Default)]
struct Scenario {
    reject_simulation: bool,
    reject_wallet: bool,
    unknown_send: bool,
    pending: bool,
    revert: bool,
    receipt_error: bool,
    changed_payload: bool,
    false_approval: bool,
    wrong_chain: bool,
    refresh_error: bool,
    fail_hash_save: bool,
    low_gas_balance: bool,
    invisible_allowance: bool,
}

#[derive(Default)]
struct State {
    calls: Vec<(String, Value)>,
    sent: Vec<Value>,
    journal: Vec<ExecutionRecord>,
    approved: bool,
}

#[derive(Clone)]
struct Journal {
    state: Arc<Mutex<State>>,
    fail_hash_save: bool,
}

impl ExecutionJournal for Journal {
    fn begin(
        &mut self,
        record: &ExecutionRecord,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut state = self.state.lock().unwrap();
        if !state.journal.is_empty() {
            return Err("unresolved execution".into());
        }
        state.journal.push(record.clone());
        Ok(())
    }
    fn save(
        &mut self,
        record: &ExecutionRecord,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.fail_hash_save && record.swap_hash().is_some() {
            return Err("disk unavailable".into());
        }
        self.state.lock().unwrap().journal.push(record.clone());
        Ok(())
    }
}

struct Rpc {
    url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    scenario: Scenario,
}

impl Rpc {
    fn new(scenario: Scenario) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = state.clone();
        let shutdown = stop.clone();
        let thread = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &shared, scenario),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("mock listener: {error}"),
                }
            }
        });
        Self {
            url,
            state,
            stop,
            thread: Some(thread),
            scenario,
        }
    }

    fn provider(&self) -> RootProvider {
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(self.url.parse().unwrap())
    }

    async fn router(&self) -> FeeRouter<RootProvider> {
        let client = EvmClient::connect(self.provider(), ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        FeeRouter::connect(client, B256::repeat_byte(7))
            .await
            .unwrap()
    }

    fn execution(&self) -> Execution<RootProvider, Journal> {
        Execution::new(
            self.provider(),
            Journal {
                state: self.state.clone(),
                fail_hash_save: self.scenario.fail_hash_save,
            },
            ExecutionOptions {
                confirmation_timeout: if self.scenario.pending {
                    Duration::from_millis(60)
                } else {
                    Duration::from_secs(2)
                },
                poll_interval: Duration::from_millis(20),
                check_rpc_account: true,
            },
        )
    }

    fn sends(&self) -> usize {
        self.state.lock().unwrap().sent.len()
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn serve(stream: TcpStream, shared: &Mutex<State>, scenario: Scenario) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        // Deadline cancellation can leave an idle or partially written HTTP connection.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return;
        }
        Err(error) => panic!("mock request read failed: {error}"),
    };
    let method = request["method"].as_str().unwrap();
    let params = &request["params"];
    let mut state = shared.lock().unwrap();
    state.calls.push((method.to_owned(), params.clone()));
    let result = response(method, params, &mut state, scenario);
    let payload = match result {
        Ok(value) => json!({"jsonrpc":"2.0", "id":request["id"], "result":value}),
        Err(code) => json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":code, "message":"mock rejection"}}),
    }.to_string();
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
}

fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value
                .trim()
                .parse()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn response(
    method: &str,
    params: &Value,
    state: &mut State,
    scenario: Scenario,
) -> Result<Value, i64> {
    Ok(match method {
        "eth_chainId" => json!(if scenario.wrong_chain
            && !state
                .calls
                .iter()
                .all(|(method, _)| method == "eth_chainId")
        {
            "0x1"
        } else {
            "0x1237"
        }),
        "eth_getCode" => json!("0x6000"),
        "eth_accounts" => json!([WALLET]),
        "eth_getBalance" => json!(U256::from(if scenario.low_gas_balance {
            10000
        } else {
            10_000_000_000u64
        })),
        "eth_gasPrice" => json!("0x1"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_getTransactionCount" => json!(format!("0x{:x}", state.sent.len())),
        "eth_call" => contract_call(&params[0], state, scenario)?,
        "eth_sendTransaction" => {
            let tx = params[0].clone();
            let saved = state
                .journal
                .last()
                .expect("record must precede submission")
                .transactions
                .last()
                .unwrap();
            assert_eq!(saved.submission, SubmissionState::Unknown);
            assert_eq!(serde_json::to_value(&saved.transaction).unwrap(), tx);
            assert!(tx.get("input").is_none(), "Frame requires data");
            assert_ne!(tx["data"], "0x");
            state.sent.push(tx);
            if scenario.reject_wallet {
                return Err(4001);
            }
            if scenario.unknown_send {
                return Err(-32000);
            }
            json!(B256::repeat_byte(state.sent.len() as u8))
        }
        "eth_getTransactionReceipt" => {
            if scenario.receipt_error {
                return Err(-32000);
            }
            if scenario.pending {
                return Ok(Value::Null);
            }
            let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
            let tx = &state.sent[hash[0] as usize - 1];
            if tx["to"] == json!(TOKEN) && !scenario.revert {
                state.approved = true;
            }
            json!({"transactionHash":hash, "transactionIndex":"0x0", "blockHash":B256::repeat_byte(9), "blockNumber":"0x9", "from":WALLET, "to":tx["to"], "cumulativeGasUsed":"0x5208", "gasUsed":"0x5208", "effectiveGasPrice":"0x1", "contractAddress":null, "logs":[], "logsBloom":format!("0x{}", "00".repeat(256)), "status":if scenario.revert { "0x0" } else { "0x1" }, "type":"0x0"})
        }
        "eth_getTransactionByHash" => {
            let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
            let mut tx = state.sent[hash[0] as usize - 1].clone();
            tx["input"] = tx["data"].take();
            if scenario.changed_payload {
                tx["input"] = json!("0x");
            }
            tx
        }
        "eth_getBlockByNumber" => {
            if scenario.refresh_error {
                return Err(-32000);
            }
            let mut block = Block::<Transaction>::default();
            block.header.hash = B256::repeat_byte(8);
            block.header.number = 8;
            block.header.timestamp = 100;
            serde_json::to_value(block).unwrap()
        }
        _ => panic!("unexpected RPC method {method}"),
    })
}

fn encoded(value: impl SolValue) -> Value {
    json!(Bytes::from(value.abi_encode()))
}

fn contract_call(tx: &Value, state: &State, scenario: Scenario) -> Result<Value, i64> {
    let to: Address = serde_json::from_value(tx["to"].clone()).unwrap();
    let input: Bytes =
        serde_json::from_value(tx.get("input").or_else(|| tx.get("data")).unwrap().clone())
            .unwrap();
    let deployment = UniswapDeployment::robinhood_mainnet();
    let selector = &input[..4];
    let signature = |name: &str| alloy_primitives::keccak256(name)[..4].to_vec();
    if to == ROBINHOOD_FEE_ROUTER && input.len() > 4 || selector == approveCall::SELECTOR {
        if scenario.reject_simulation {
            return Err(3);
        }
        return Ok(if selector == approveCall::SELECTOR {
            encoded(!scenario.false_approval)
        } else {
            encoded(U256::from(2000))
        });
    }
    for (method, address) in [
        ("feeRecipient()", Address::repeat_byte(2)),
        ("wrappedNative()", WETH),
        ("v2Factory()", deployment.v2.unwrap().factory),
        ("v3Factory()", deployment.v3.as_ref().unwrap().factory),
        ("poolManager()", deployment.v4.unwrap().pool_manager),
        ("factory()", deployment.v3.as_ref().unwrap().factory),
        ("WETH9()", WETH),
        ("token0()", WETH),
        ("token1()", TOKEN),
    ] {
        if selector == signature(method) {
            return Ok(encoded(address));
        }
    }
    Ok(if selector == signature("FEE_BPS()") {
        encoded(U256::from(100))
    } else if selector == signature("fee()") {
        encoded(U24::from(500))
    } else if selector == signature("tickSpacing()") {
        encoded(I24::try_from(10).unwrap())
    } else if selector == signature("getPool(address,address,uint24)") {
        encoded(POOL)
    } else if selector == signature("balanceOf(address)") {
        encoded(U256::from(100000))
    } else if selector == signature("allowance(address,address)") {
        encoded(if state.approved && !scenario.invisible_allowance {
            U256::from(10000)
        } else {
            U256::ZERO
        })
    } else if to == deployment.v3.unwrap().quoter_v2 {
        encoded((
            U256::from(if state.approved { 3000 } else { 2000 }),
            U160::from(1),
            0u32,
            U256::from(50000),
        ))
    } else {
        panic!("unexpected contract call {to}: {input}")
    })
}

fn request(side: TradeSide) -> TradeRequest {
    TradeRequest::Pair(PairSwap {
        pair: POOL,
        token_address: TOKEN,
        side,
        amount_in: U256::from(10000),
    })
}

fn limits() -> SwapLimits {
    SwapLimits {
        slippage_bps: 50,
        deadline_unix_seconds: u64::MAX,
    }
}

fn execution_error(error: Error) -> Box<evm_trading_sdk::execution::ExecutionError> {
    let Error::Execution(error) = error else {
        panic!("expected execution context: {error:?}")
    };
    error
}

#[tokio::test]
async fn submit_returns_the_hash_without_waiting_and_status_never_resends() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = router.trader(WALLET).buy_with(BuyWith::Native);
    let execution = &mut rpc.execution();
    let result = trader
        .submit(execution, request(TradeSide::Buy), limits())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Pending);
    assert_eq!(result.record.swap_hash(), Some(B256::repeat_byte(1)));
    assert!(
        !rpc.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "eth_getTransactionReceipt")
    );
    for _ in 0..2 {
        assert_eq!(
            trader
                .status(execution, &result.record)
                .await
                .unwrap()
                .status,
            ExecutionStatus::Confirmed
        );
    }
    assert_eq!(rpc.sends(), 1);
}

#[tokio::test]
async fn swap_confirms_approval_then_refreshes_before_submitting() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let result = router
        .trader(WALLET)
        .swap(&mut rpc.execution(), request(TradeSide::Sell), limits())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Confirmed);
    assert_eq!(result.record.transactions.len(), 2);
    assert_eq!(
        result.record.transactions[0].kind,
        TransactionKind::Approval
    );
    assert_eq!(result.record.swap_hash(), Some(B256::repeat_byte(2)));
    let state = rpc.state.lock().unwrap();
    let calls: Vec<_> = state.calls.iter().map(|(name, _)| name.as_str()).collect();
    let receipt = calls
        .iter()
        .position(|name| *name == "eth_getTransactionReceipt")
        .unwrap();
    let refresh = calls
        .iter()
        .position(|name| *name == "eth_getBlockByNumber")
        .unwrap();
    let swap = calls
        .iter()
        .rposition(|name| *name == "eth_sendTransaction")
        .unwrap();
    assert!(receipt < refresh && refresh < swap);
    let approval = approveCall::abi_decode(
        &serde_json::from_value::<Bytes>(state.sent[0]["data"].clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(approval.amount, U256::from(10000));
    let swap = swapWithFeeCall::abi_decode(
        &serde_json::from_value::<Bytes>(state.sent[1]["data"].clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(swap.request.amountIn, U256::from(10000));
    assert_eq!(swap.request.deadline, U256::from(u64::MAX));
    assert_eq!(swap.request.minimumAmountOut, U256::from(2985));
    assert_eq!(swap.hops[0].pool, POOL);
}

#[tokio::test]
async fn simulation_failure_never_submits() {
    let rpc = Rpc::new(Scenario {
        reject_simulation: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let error = router
        .trader(WALLET)
        .buy_with(BuyWith::Native)
        .submit(&mut rpc.execution(), request(TradeSide::Buy), limits())
        .await
        .unwrap_err();
    assert_eq!(execution_error(error).stage, ExecutionStage::Simulation);
    assert_eq!(rpc.sends(), 0);
}

#[tokio::test]
async fn wallet_rejection_is_distinct_from_unknown_submission() {
    for rejected in [true, false] {
        let rpc = Rpc::new(Scenario {
            reject_wallet: rejected,
            unknown_send: !rejected,
            ..Default::default()
        });
        let router = rpc.router().await;
        let error = execution_error(
            router
                .trader(WALLET)
                .buy_with(BuyWith::Native)
                .submit(&mut rpc.execution(), request(TradeSide::Buy), limits())
                .await
                .unwrap_err(),
        );
        assert_eq!(
            matches!(error.failure, ExecutionFailure::Rejected(_)),
            rejected
        );
        assert_eq!(
            matches!(error.failure, ExecutionFailure::UnknownSubmission(_)),
            !rejected
        );
        assert_eq!(
            error.record.transactions[0].submission,
            if rejected {
                SubmissionState::Rejected
            } else {
                SubmissionState::Unknown
            }
        );
        assert_eq!(rpc.sends(), 1);
        let result = router
            .trader(WALLET)
            .status(&rpc.execution(), &error.record)
            .await
            .unwrap();
        assert_eq!(
            result.status,
            if rejected {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Pending
            }
        );
        assert_eq!(rpc.sends(), 1);
    }
}

#[tokio::test]
async fn confirmation_timeout_retains_pending_hash_without_resending() {
    let rpc = Rpc::new(Scenario {
        pending: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let result = router
        .trader(WALLET)
        .buy_with(BuyWith::Native)
        .swap(&mut rpc.execution(), request(TradeSide::Buy), limits())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Pending);
    assert_eq!(result.record.swap_hash(), Some(B256::repeat_byte(1)));
    assert_eq!(rpc.sends(), 1);
}

#[tokio::test]
async fn pending_or_reverted_approval_never_starts_swap() {
    for pending in [true, false] {
        let rpc = Rpc::new(Scenario {
            pending,
            revert: !pending,
            ..Default::default()
        });
        let router = rpc.router().await;
        let result = router
            .trader(WALLET)
            .swap(&mut rpc.execution(), request(TradeSide::Sell), limits())
            .await
            .unwrap();
        assert_eq!(
            result.status,
            if pending {
                ExecutionStatus::Pending
            } else {
                ExecutionStatus::Failed
            }
        );
        assert_eq!(result.record.swap_hash(), None);
        assert_eq!(
            result.record.transactions[0].submission,
            SubmissionState::Submitted(B256::repeat_byte(1))
        );
        assert_eq!(rpc.sends(), 1);
    }
}

#[tokio::test]
async fn mined_revert_returns_failed_with_its_hash() {
    let rpc = Rpc::new(Scenario {
        revert: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let result = router
        .trader(WALLET)
        .buy_with(BuyWith::Native)
        .swap(&mut rpc.execution(), request(TradeSide::Buy), limits())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert_eq!(result.record.swap_hash(), Some(B256::repeat_byte(1)));
}

#[tokio::test]
async fn confirmation_and_storage_errors_retain_submitted_identifiers() {
    for scenario in [
        Scenario {
            receipt_error: true,
            ..Default::default()
        },
        Scenario {
            changed_payload: true,
            ..Default::default()
        },
        Scenario {
            fail_hash_save: true,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let error = execution_error(
            router
                .trader(WALLET)
                .buy_with(BuyWith::Native)
                .swap(&mut rpc.execution(), request(TradeSide::Buy), limits())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.record.swap_hash(), Some(B256::repeat_byte(1)));
        assert_eq!(rpc.sends(), 1);
    }
}

#[tokio::test]
async fn failed_refresh_retains_approval_without_submitting_swap() {
    let rpc = Rpc::new(Scenario {
        refresh_error: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let error = execution_error(
        router
            .trader(WALLET)
            .submit(&mut rpc.execution(), request(TradeSide::Sell), limits())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.stage, ExecutionStage::Refresh);
    assert_eq!(
        error.record.transactions[0].submission,
        SubmissionState::Submitted(B256::repeat_byte(1))
    );
    assert_eq!(rpc.sends(), 1);
}

#[tokio::test]
async fn false_approval_or_wrong_network_stops_before_send() {
    for scenario in [
        Scenario {
            false_approval: true,
            ..Default::default()
        },
        Scenario {
            wrong_chain: true,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let error = execution_error(
            router
                .trader(WALLET)
                .submit(&mut rpc.execution(), request(TradeSide::Sell), limits())
                .await
                .unwrap_err(),
        );
        assert!(matches!(
            error.stage,
            ExecutionStage::Simulation | ExecutionStage::Wallet
        ));
        assert_eq!(rpc.sends(), 0);
    }
}

#[tokio::test]
async fn missing_gas_funds_or_expired_deadline_never_requests_a_signature() {
    for low_gas_balance in [true, false] {
        let rpc = Rpc::new(Scenario {
            low_gas_balance,
            ..Default::default()
        });
        let router = rpc.router().await;
        let limits = if low_gas_balance {
            limits()
        } else {
            SwapLimits {
                deadline_unix_seconds: 1,
                ..limits()
            }
        };
        let error = execution_error(
            router
                .trader(WALLET)
                .buy_with(BuyWith::Native)
                .submit(&mut rpc.execution(), request(TradeSide::Buy), limits)
                .await
                .unwrap_err(),
        );
        assert!(matches!(
            error.failure,
            ExecutionFailure::InsufficientFunds { .. } | ExecutionFailure::Expired
        ));
        assert_eq!(rpc.sends(), 0);
    }
}

#[tokio::test]
async fn approval_must_be_visible_at_the_refreshed_block() {
    let rpc = Rpc::new(Scenario {
        invisible_allowance: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let error = execution_error(
        router
            .trader(WALLET)
            .submit(&mut rpc.execution(), request(TradeSide::Sell), limits())
            .await
            .unwrap_err(),
    );
    assert!(matches!(
        error.failure,
        ExecutionFailure::AllowanceNotVisible
    ));
    assert_eq!(error.record.transactions.len(), 1);
    assert_eq!(rpc.sends(), 1);
}

#[tokio::test]
async fn another_wallet_cannot_check_a_record_as_its_own() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let execution = &mut rpc.execution();
    let result = router
        .trader(WALLET)
        .buy_with(BuyWith::Native)
        .submit(execution, request(TradeSide::Buy), limits())
        .await
        .unwrap();
    let error = execution_error(
        router
            .trader(Address::repeat_byte(4))
            .status(execution, &result.record)
            .await
            .unwrap_err(),
    );
    assert!(matches!(error.failure, ExecutionFailure::RecordMismatch));
    assert_eq!(error.record.swap_hash(), result.record.swap_hash());
    assert_eq!(rpc.sends(), 1);
}

sol! {
    function approve(address spender, uint256 amount) external returns (bool);
    struct Key { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct Hop { uint8 version; address tokenIn; address tokenOut; address pool; uint24 fee; Key key; bytes hookData; }
    struct Request { address tokenIn; address tokenOut; uint256 amountIn; uint256 minimumAmountOut; address recipient; uint256 deadline; }
    function swapWithFee(Request request, Hop[] hops, uint256 expectedFeeBps) external payable returns (uint256);
}
