use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::Decodable2718;
use alloy_network::EthereumWallet;
use alloy_primitives::{
    Address, B256, Bytes, U256, address,
    aliases::{I24, U24, U160},
};
use alloy_provider::{ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::{Block, Transaction as RpcTransaction, TransactionRequest};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use evm_trading_sdk::{
    BloxrouteSubmitter, BuyWith, Error, EvmClient, FeeRouter, PairSwap, ROBINHOOD_CHAIN_ID,
    SubmitError, TradeSide, Trader,
    dex::SwapLimits,
    execution::{
        Execution, ExecutionFailure, ExecutionJournal, ExecutionOptions, ExecutionRecord,
        ExecutionStage, ExecutionStatus, SubmissionState, TradeRequest, TransactionKind,
    },
    fee_router::ROBINHOOD_FEE_ROUTER,
    uniswap::UniswapDeployment,
};
use serde_json::{Value, json};

const WALLET: Address = address!("7e5f4552091a69125d5dfcb7b8c2659029395bdf");
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
    worse_quote: bool,
    changed_fee: bool,
    blox_status: u16,
    blox_wrong_hash: bool,
    blox_drop: bool,
    blox_malformed: bool,
}

#[derive(Default)]
struct State {
    calls: Vec<(String, Value)>,
    sent: Vec<Value>,
    journal: Vec<ExecutionRecord>,
    approved: bool,
    approved_wallets: HashSet<Address>,
    hashes: Vec<B256>,
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
        let wallet = record.transactions[0].transaction.from;
        if state.journal.iter().any(|saved| {
            saved
                .transactions
                .first()
                .is_some_and(|tx| tx.transaction.from == wallet)
        }) {
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

    fn execution(&self) -> Execution<'static, RootProvider, Journal> {
        Execution::new(
            self.provider(),
            self.journal(),
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

    fn journal(&self) -> Journal {
        Journal {
            state: self.state.clone(),
            fail_hash_save: self.scenario.fail_hash_save,
        }
    }

    fn blox_execution(&self) -> Execution<'static, RootProvider, Journal> {
        self.execution().with_submitter(
            EthereumWallet::from(test_signer()),
            BloxrouteSubmitter::fast(&self.url, "test-auth").unwrap(),
        )
    }

    fn blox_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(method, _)| method == "robinhood_tx")
            .count()
    }
}

fn test_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(U256::from(1))).unwrap()
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
    let (request, authorization) = match read_request(&mut reader) {
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
    let is_blox = method == "robinhood_tx";
    if is_blox {
        assert_eq!(authorization.as_deref(), Some("test-auth"));
    }
    let status = if is_blox && scenario.blox_status != 0 {
        scenario.blox_status
    } else {
        200
    };
    let result = if status == 200 {
        response(method, params, &mut state, scenario)
    } else {
        Ok(Value::Null)
    };
    if is_blox && scenario.blox_drop {
        return;
    }
    let payload = if is_blox && scenario.blox_malformed {
        "invalid JSON".to_owned()
    } else {
        match result {
        Ok(value) => json!({"jsonrpc":"2.0", "id":request["id"], "result":value}),
        Err(code) => json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":code, "message":"mock rejection"}}),
    }.to_string()
    };
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nLocation: /redirect\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
}

fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<(Value, Option<String>)>> {
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_owned());
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
        .map(|body| Some((body, authorization)))
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
        "eth_feeHistory" => {
            json!({"oldestBlock":"0x7","baseFeePerGas":["0x1","0x1"],"gasUsedRatio":[0.5],"reward":[["0x1"]]})
        }
        "eth_estimateGas" => json!("0x5208"),
        "eth_getTransactionCount" => json!(format!(
            "0x{:x}",
            state
                .sent
                .iter()
                .filter(|tx| tx["from"] == params[0])
                .count()
        )),
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
            state.hashes.push(B256::repeat_byte(state.sent.len() as u8));
            if scenario.reject_wallet {
                return Err(4001);
            }
            if scenario.unknown_send {
                return Err(-32000);
            }
            json!(B256::repeat_byte(state.sent.len() as u8))
        }
        "robinhood_tx" => {
            assert_eq!(params.as_object().unwrap().len(), 1);
            let hex = params["transaction"].as_str().unwrap();
            assert!(!hex.starts_with("0x"));
            let bytes = alloy_primitives::hex::decode(hex).unwrap();
            let mut input = bytes.as_slice();
            let envelope = TxEnvelope::decode_2718(&mut input).unwrap();
            assert!(input.is_empty());
            assert!(envelope.is_eip1559());
            assert_eq!(envelope.chain_id(), Some(ROBINHOOD_CHAIN_ID));
            let sender = envelope.recover_signer().unwrap();
            let hash = *envelope.tx_hash();
            let tx = TransactionRequest::from_transaction_with_sender(envelope, sender)
                .normalized_data();
            let saved = state
                .journal
                .iter()
                .rev()
                .flat_map(|record| record.transactions.iter().rev())
                .find(|tx| tx.signed_hash == Some(hash))
                .expect("signed hash must be durable before POST");
            assert_eq!(saved.submission, SubmissionState::Unknown);
            assert_eq!(
                saved.signed_hash,
                Some(hash),
                "hash must be durable before POST"
            );
            assert_eq!(saved.transaction.from, tx.from);
            assert_eq!(saved.transaction.to, tx.to);
            assert_eq!(saved.transaction.input, tx.input);
            assert_eq!(saved.transaction.value, tx.value);
            assert_eq!(saved.transaction.nonce, tx.nonce);
            assert_eq!(saved.transaction.gas, tx.gas);
            assert_eq!(saved.transaction.max_fee_per_gas, tx.max_fee_per_gas);
            assert_eq!(
                saved.transaction.max_priority_fee_per_gas,
                tx.max_priority_fee_per_gas
            );
            state.sent.push(serde_json::to_value(tx).unwrap());
            state.hashes.push(hash);
            if scenario.unknown_send {
                return Err(-32000);
            }
            json!({"txHash":if scenario.blox_wrong_hash { B256::repeat_byte(255) } else { hash }})
        }
        "eth_getTransactionReceipt" => {
            if scenario.receipt_error {
                return Err(-32000);
            }
            if scenario.pending {
                return Ok(Value::Null);
            }
            let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
            let Some(index) = state.hashes.iter().position(|candidate| *candidate == hash) else {
                return Ok(Value::Null);
            };
            let tx = &state.sent[index];
            if tx["to"] == json!(TOKEN) && !scenario.revert {
                state.approved = true;
                state
                    .approved_wallets
                    .insert(serde_json::from_value(tx["from"].clone()).unwrap());
            }
            json!({"transactionHash":hash, "transactionIndex":"0x0", "blockHash":B256::repeat_byte(9), "blockNumber":"0x9", "from":tx["from"], "to":tx["to"], "cumulativeGasUsed":"0x5208", "gasUsed":"0x5208", "effectiveGasPrice":"0x1", "contractAddress":null, "logs":[], "logsBloom":format!("0x{}", "00".repeat(256)), "status":if scenario.revert { "0x0" } else { "0x1" }, "type":"0x0"})
        }
        "eth_getTransactionByHash" => {
            let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
            let index = state
                .hashes
                .iter()
                .position(|candidate| *candidate == hash)
                .unwrap();
            let mut tx = state.sent[index].clone();
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
            let mut block = Block::<RpcTransaction>::default();
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
        encoded(U256::from(if state.approved && scenario.changed_fee {
            200
        } else {
            100
        }))
    } else if selector == signature("slot0()") {
        encoded((U160::from(1) << 96, I24::ZERO, 0u16, 0u16, 0u16, 0u16, true))
    } else if selector == signature("fee()") {
        encoded(U24::from(500))
    } else if selector == signature("tickSpacing()") {
        encoded(I24::try_from(10).unwrap())
    } else if selector == signature("getPool(address,address,uint24)") {
        encoded(POOL)
    } else if selector == signature("balanceOf(address)") {
        encoded(U256::from(100000))
    } else if selector == signature("allowance(address,address)") {
        let owner = Address::from_slice(&input[16..36]);
        encoded(
            if state.approved_wallets.contains(&owner) && !scenario.invisible_allowance {
                U256::from(10000)
            } else {
                U256::ZERO
            },
        )
    } else if to == deployment.v3.unwrap().quoter_v2 {
        encoded((
            U256::from(if state.approved {
                if scenario.worse_quote { 1000 } else { 3000 }
            } else {
                2000
            }),
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
async fn shared_trader_keeps_concurrent_wallets_and_approvals_separate() {
    fn sendable<T: Send>(future: T) -> T {
        future
    }

    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = Trader::new(&router);
    let blox = BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap();
    let alice = EthereumWallet::from(test_signer());
    let bob =
        EthereumWallet::from(PrivateKeySigner::from_bytes(&B256::from(U256::from(2))).unwrap());
    let bob_address = bob.default_signer().address();
    let mut alice_journal = rpc.journal();
    let mut bob_journal = rpc.journal();
    let TradeRequest::Pair(buy) = request(TradeSide::Buy) else {
        unreachable!()
    };
    let prepared = trader.prepare_swap(WALLET, buy, limits()).await.unwrap();
    assert_eq!(prepared.quote.request.trade.sender, WALLET);
    assert_eq!(prepared.quote.request.trade.recipient, WALLET);
    assert_eq!(rpc.blox_calls(), 0);

    let (alice_result, bob_result) = tokio::join!(
        sendable(trader.swap_prepared(prepared, &alice, &blox, &mut alice_journal)),
        sendable(trader.swap(
            request(TradeSide::Sell),
            limits(),
            &bob,
            &blox,
            &mut bob_journal
        )),
    );
    let alice_result = alice_result.unwrap();
    let bob_result = bob_result.unwrap();
    assert_eq!(alice_result.record.transactions.len(), 1);
    assert_eq!(bob_result.record.transactions.len(), 2);
    assert_eq!(
        bob_result.record.transactions[0].kind,
        TransactionKind::Approval
    );
    assert_ne!(
        alice_result.record.swap_hash(),
        bob_result.record.swap_hash()
    );
    for (result, wallet) in [(&alice_result, WALLET), (&bob_result, bob_address)] {
        assert_eq!(result.status, ExecutionStatus::Confirmed);
        for (nonce, tx) in result.record.transactions.iter().enumerate() {
            assert_eq!(tx.transaction.from, Some(wallet));
            assert_eq!(tx.transaction.nonce, Some(nonce as u64));
        }
        assert_eq!(
            trader.status(&result.record).await.unwrap().status,
            ExecutionStatus::Confirmed
        );
    }
    let state = rpc.state.lock().unwrap();
    assert_eq!(state.sent.len(), 3);
    for record in &state.journal {
        let owner = record.transactions[0].transaction.from;
        assert!(
            record
                .transactions
                .iter()
                .all(|tx| tx.transaction.from == owner)
        );
    }
    assert!(state.approved_wallets.contains(&bob_address));
    assert!(!state.approved_wallets.contains(&WALLET));
    assert!(
        !state
            .calls
            .iter()
            .any(|(method, _)| method == "eth_accounts")
    );
}

#[tokio::test]
async fn shared_trader_rejects_another_wallets_preview_before_signing() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = Trader::new(&router);
    let blox = BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap();
    let other =
        EthereumWallet::from(PrivateKeySigner::from_bytes(&B256::from(U256::from(2))).unwrap());
    let TradeRequest::Pair(buy) = request(TradeSide::Buy) else {
        unreachable!()
    };
    let prepared = trader.prepare_swap(WALLET, buy, limits()).await.unwrap();
    let error = execution_error(
        trader
            .submit_prepared(prepared, &other, &blox, &mut rpc.journal())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.stage, ExecutionStage::Preparation);
    assert!(error.record.transactions.is_empty());
    assert!(rpc.state.lock().unwrap().journal.is_empty());
    assert_eq!(rpc.blox_calls(), 0);
}

#[tokio::test]
async fn shared_trader_does_not_carry_a_rejected_signer_into_the_next_trade() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = Trader::new(&router);
    let blox = BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap();
    let rejected = EthereumWallet::from(test_signer().with_chain_id(Some(1)));
    let accepted =
        EthereumWallet::from(PrivateKeySigner::from_bytes(&B256::from(U256::from(2))).unwrap());
    let error = execution_error(
        trader
            .submit(
                request(TradeSide::Buy),
                limits(),
                &rejected,
                &blox,
                &mut rpc.journal(),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(error.stage, ExecutionStage::Signing);
    assert_eq!(rpc.blox_calls(), 0);
    let result = trader
        .submit(
            request(TradeSide::Buy),
            limits(),
            &accepted,
            &blox,
            &mut rpc.journal(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Pending);
    assert_eq!(result.record.transactions.len(), 1);
    assert_eq!(
        result.record.transactions[0].transaction.from,
        Some(accepted.default_signer().address())
    );
    assert_eq!(
        trader.status(&result.record).await.unwrap().status,
        ExecutionStatus::Confirmed
    );
    assert_eq!(
        trader.status(&error.record).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(rpc.blox_calls(), 1);
}

#[tokio::test]
async fn bloxroute_signs_and_submits_approval_before_the_previewed_swap() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = Trader::new(&router);
    let signer = EthereumWallet::from(test_signer());
    let blox = BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap();
    let TradeRequest::Pair(request) = request(TradeSide::Sell) else {
        unreachable!()
    };
    let prepared = trader
        .prepare_swap(WALLET, request, limits())
        .await
        .unwrap();
    let expected = prepared.plan.transaction.input.input().unwrap().clone();
    let result = trader
        .swap_prepared(prepared, &signer, &blox, &mut rpc.journal())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Confirmed);
    assert_eq!(rpc.blox_calls(), 2);
    let state = rpc.state.lock().unwrap();
    assert_eq!(state.sent[1]["data"], json!(expected));
    assert_eq!(result.record.swap_hash(), Some(state.hashes[1]));
    assert!(!state.calls.iter().any(|(method, _)| matches!(
        method.as_str(),
        "eth_accounts" | "eth_sendTransaction" | "eth_sendRawTransaction"
    )));
    let approval_receipt = state
        .calls
        .iter()
        .position(|(method, _)| method == "eth_getTransactionReceipt")
        .unwrap();
    let swap_submit = state
        .calls
        .iter()
        .rposition(|(method, _)| method == "robinhood_tx")
        .unwrap();
    assert!(approval_receipt < swap_submit);
    let stored = serde_json::to_value(&result.record).unwrap();
    assert!(stored["transactions"][0]["signed_hash"].is_string());
    let round_trip: ExecutionRecord = serde_json::from_value(stored).unwrap();
    assert_eq!(round_trip.swap_hash(), result.record.swap_hash());
}

#[tokio::test]
async fn uncertain_bloxroute_responses_keep_the_local_hash_and_never_resend() {
    for scenario in [
        Scenario {
            blox_drop: true,
            ..Default::default()
        },
        Scenario {
            blox_malformed: true,
            ..Default::default()
        },
        Scenario {
            blox_wrong_hash: true,
            ..Default::default()
        },
        Scenario {
            unknown_send: true,
            ..Default::default()
        },
        Scenario {
            blox_status: 500,
            ..Default::default()
        },
        Scenario {
            blox_status: 307,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let trader = Trader::new(&router);
        let signer = EthereumWallet::from(test_signer());
        let blox = BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap();
        let error = execution_error(
            trader
                .submit(
                    request(TradeSide::Buy),
                    limits(),
                    &signer,
                    &blox,
                    &mut rpc.journal(),
                )
                .await
                .unwrap_err(),
        );
        assert_eq!(error.stage, ExecutionStage::Submission);
        assert!(matches!(
            error.failure,
            ExecutionFailure::UnknownBroadcast(_)
        ));
        assert!(error.record.swap_hash().is_some());
        assert_eq!(
            error.record.transactions[0].submission,
            SubmissionState::Unknown
        );
        for _ in 0..2 {
            let status = trader.status(&error.record).await.unwrap();
            assert_eq!(
                status.status,
                if scenario.blox_status == 0 {
                    ExecutionStatus::Confirmed
                } else {
                    ExecutionStatus::Pending
                }
            );
        }
        assert_eq!(rpc.blox_calls(), 1);
    }
}

#[tokio::test]
async fn bloxroute_authentication_failure_is_a_definite_rejection() {
    for code in [401, 403] {
        let rpc = Rpc::new(Scenario {
            blox_status: code,
            ..Default::default()
        });
        let router = rpc.router().await;
        let trader = router.trader(WALLET);
        let execution = &mut rpc.blox_execution();
        let error = execution_error(
            trader
                .submit(execution, request(TradeSide::Buy), limits())
                .await
                .unwrap_err(),
        );
        assert!(
            matches!(error.failure, ExecutionFailure::BroadcastRejected(SubmitError::Http(status)) if status == code)
        );
        assert_eq!(
            error.record.transactions[0].submission,
            SubmissionState::Rejected
        );
        assert_eq!(
            trader
                .status(execution, &error.record)
                .await
                .unwrap()
                .status,
            ExecutionStatus::Failed
        );
        assert_eq!(rpc.blox_calls(), 1);
        assert_eq!(rpc.sends(), 0);
    }
}

#[tokio::test]
async fn signing_failure_or_wrong_signature_never_reaches_bloxroute() {
    let other = PrivateKeySigner::from_bytes(&B256::from(U256::from(2))).unwrap();
    let impostor = PrivateKeySigner::new_with_credential(other.into_credential(), WALLET, None);
    for (wallet, mismatch) in [
        (
            EthereumWallet::from(test_signer().with_chain_id(Some(1))),
            false,
        ),
        (EthereumWallet::from(impostor), true),
    ] {
        let rpc = Rpc::new(Scenario::default());
        let router = rpc.router().await;
        let mut execution = rpc.execution().with_submitter(
            wallet,
            BloxrouteSubmitter::fast(&rpc.url, "test-auth").unwrap(),
        );
        let error = execution_error(
            router
                .trader(WALLET)
                .submit(&mut execution, request(TradeSide::Buy), limits())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.stage, ExecutionStage::Signing);
        assert!(if mismatch {
            matches!(error.failure, ExecutionFailure::SignatureMismatch)
        } else {
            matches!(error.failure, ExecutionFailure::Signing(_))
        });
        assert_eq!(
            error.record.transactions[0].submission,
            SubmissionState::Rejected
        );
        assert_eq!(rpc.blox_calls(), 0);
    }
}

#[tokio::test]
async fn saving_the_signed_hash_must_succeed_before_broadcast() {
    let rpc = Rpc::new(Scenario {
        fail_hash_save: true,
        ..Default::default()
    });
    let router = rpc.router().await;
    let error = execution_error(
        router
            .trader(WALLET)
            .submit(&mut rpc.blox_execution(), request(TradeSide::Buy), limits())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.stage, ExecutionStage::Persistence);
    assert!(error.record.swap_hash().is_some());
    assert_eq!(rpc.blox_calls(), 0);
}

#[tokio::test]
async fn bloxroute_preflight_failures_do_not_sign_or_broadcast() {
    for scenario in [
        Scenario {
            reject_simulation: true,
            ..Default::default()
        },
        Scenario {
            low_gas_balance: true,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let error = execution_error(
            router
                .trader(WALLET)
                .submit(&mut rpc.blox_execution(), request(TradeSide::Buy), limits())
                .await
                .unwrap_err(),
        );
        assert_eq!(
            error.stage,
            if scenario.reject_simulation {
                ExecutionStage::Simulation
            } else {
                ExecutionStage::Funding
            }
        );
        assert!(error.record.transactions.is_empty());
        assert!(rpc.state.lock().unwrap().journal.is_empty());
        assert_eq!(rpc.blox_calls(), 0);
    }
}

#[tokio::test]
async fn bloxroute_timeout_and_revert_keep_their_transaction_hash() {
    for scenario in [
        Scenario {
            pending: true,
            ..Default::default()
        },
        Scenario {
            revert: true,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let result = router
            .trader(WALLET)
            .swap(&mut rpc.blox_execution(), request(TradeSide::Buy), limits())
            .await
            .unwrap();
        assert_eq!(
            result.status,
            if scenario.pending {
                ExecutionStatus::Pending
            } else {
                ExecutionStatus::Failed
            }
        );
        assert!(result.record.swap_hash().is_some());
        assert_eq!(rpc.blox_calls(), 1);
    }
}

#[tokio::test]
async fn prepared_submission_uses_the_preview_without_requoting() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = router.trader(WALLET);
    let TradeRequest::Pair(request) = request(TradeSide::Buy) else {
        unreachable!()
    };
    let prepared = trader.prepare_swap(request, limits()).await.unwrap();
    assert!(prepared.price_impact.is_ok());
    assert_eq!(prepared.router_fee_amount, U256::from(100));
    assert_eq!(prepared.minimum_amount_out(), U256::from(1990));
    let expected = prepared.plan.transaction.input.input().unwrap().clone();
    {
        let mut state = rpc.state.lock().unwrap();
        let slot = Bytes::copy_from_slice(&alloy_primitives::keccak256("slot0()")[..4]);
        let call = state
            .calls
            .iter()
            .find(|(method, params)| method == "eth_call" && params[0]["input"] == json!(slot))
            .unwrap();
        assert_eq!(
            call.1[1],
            json!({"blockHash":B256::repeat_byte(7),"requireCanonical":true})
        );
        state.calls.clear();
    }
    let result = trader
        .submit_prepared(&mut rpc.execution(), prepared)
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Pending);
    let state = rpc.state.lock().unwrap();
    assert_eq!(state.sent[0]["data"], json!(expected));
    let quoter = UniswapDeployment::robinhood_mainnet().v3.unwrap().quoter_v2;
    assert!(
        !state
            .calls
            .iter()
            .any(|(method, params)| method == "eth_call" && params[0]["to"] == json!(quoter))
    );
}

#[tokio::test]
async fn prepared_swap_keeps_the_displayed_calldata_after_approval() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    let trader = router.trader(WALLET);
    let TradeRequest::Pair(request) = request(TradeSide::Sell) else {
        unreachable!()
    };
    let prepared = trader.prepare_swap(request, limits()).await.unwrap();
    let expected = prepared.plan.transaction.input.input().unwrap().clone();
    let result = trader
        .swap_prepared(&mut rpc.execution(), prepared)
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Confirmed);
    let state = rpc.state.lock().unwrap();
    assert_eq!(state.sent.len(), 2);
    assert_eq!(state.sent[1]["data"], json!(expected));
    let call = swapWithFeeCall::abi_decode(&expected).unwrap();
    assert_eq!(call.request.minimumAmountOut, U256::from(1990));
    let quote_calls = state
        .calls
        .iter()
        .filter(|(method, params)| {
            method == "eth_call"
                && params[0]["to"]
                    == json!(UniswapDeployment::robinhood_mainnet().v3.unwrap().quoter_v2)
                && params[0]["input"]
                    .as_str()
                    .is_some_and(|input| input.len() > 10)
        })
        .count();
    assert_eq!(quote_calls, 2);
}

#[tokio::test]
async fn prepared_swap_stops_when_output_or_fee_changes_beyond_the_preview() {
    for scenario in [
        Scenario {
            worse_quote: true,
            ..Default::default()
        },
        Scenario {
            changed_fee: true,
            ..Default::default()
        },
    ] {
        let rpc = Rpc::new(scenario);
        let router = rpc.router().await;
        let trader = router.trader(WALLET);
        let TradeRequest::Pair(request) = request(TradeSide::Sell) else {
            unreachable!()
        };
        let prepared = trader.prepare_swap(request, limits()).await.unwrap();
        let error = execution_error(
            trader
                .submit_prepared(&mut rpc.execution(), prepared)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.stage, ExecutionStage::Refresh);
        assert_eq!(error.record.transactions.len(), 1);
        assert_eq!(
            error.record.transactions[0].submission,
            SubmissionState::Submitted(B256::repeat_byte(1))
        );
        assert_eq!(rpc.sends(), 1);
    }
}

#[tokio::test]
async fn prepared_swap_rejects_another_wallet_or_modified_plan() {
    let rpc = Rpc::new(Scenario::default());
    let router = rpc.router().await;
    for modified in [false, true] {
        let TradeRequest::Pair(request) = request(TradeSide::Buy) else {
            unreachable!()
        };
        let mut prepared = router
            .trader(WALLET)
            .prepare_swap(request, limits())
            .await
            .unwrap();
        let wallet = if modified {
            prepared.plan.transaction.value = Some(U256::from(1));
            WALLET
        } else {
            Address::repeat_byte(5)
        };
        assert!(
            router
                .trader(wallet)
                .submit_prepared(&mut rpc.execution(), prepared)
                .await
                .is_err()
        );
    }
    assert_eq!(rpc.sends(), 0);
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
