use std::{fs, path::Path, time::Duration};

use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use alloy_transport_http::reqwest;
use evm_trading_sdk::{
    Address, B256, BuyWith, EvmClient, FeeRouter, PairSwap, PreparedSwap, ROBINHOOD_CHAIN_ID,
    ResolvePool, TradeSide, U256,
    dex::SwapLimits,
    execution::{
        Execution, ExecutionJournal, ExecutionOptions, ExecutionRecord, ExecutionResult,
        ExecutionStatus, Readiness, SubmissionState, TradeRequest, TransactionKind,
        TransactionRecord,
    },
    uniswap::{
        DiscoveryOutcome, UniswapDeployment, UniswapPool, V4PoolKey,
        v4::{UniswapV4, V4PoolLookup, V4QuoteOptions},
    },
};
use serde_json::Value;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const FROM_WALLET: &str = "0xd87013169365B833f86aCEc8F6084a57a0617f00";
const PAIR_ADDRESS: &str = "0x5eac4195930d5ff8b7a34a347f2bde951c42eb730d6e26075bea3dd7c66feb40";
const TOKEN_ADDRESS: &str = "0x11B70d0243baf75E85CE03201A92b5B7C33BEB59";
const SIDE: TradeSide = TradeSide::Buy;
const AMOUNT_IN: &str = "0.0001";
const BUY_WITH: BuyWith = BuyWith::Native;
const SLIPPAGE_BPS: u16 = 50;
const NATIVE_SETTLEMENT: bool = false;
const V4_INITIALIZATION_BLOCK: Option<u64> = Some(55_278_952);
const FRAME_RPC: &str = "http://127.0.0.1:1248";
const READ_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";
const PENDING: &str = "swap-runs/pending.json";
const HELP: &str = "Edit FROM_WALLET, PAIR_ADDRESS, TOKEN_ADDRESS, SIDE, AMOUNT_IN, BUY_WITH and SLIPPAGE_BPS at the top of this file.\nBUY_WITH: BuyWith::Native for ETH, or BuyWith::Usdc (requires a verified chain deployment; not configured on Robinhood yet).\nPreview: cargo run --example swap_frame\nSign through Frame: cargo run --example swap_frame -- --broadcast\nOptional overrides: buy|sell HUMAN_AMOUNT SLIPPAGE_BPS [PAIR TARGET_TOKEN] [--native] [--wallet ADDRESS]\nBuy amounts use BUY_WITH. Sell amounts always use the target token. The selected pool does not need to be an ETH/USDC pair.\nAmounts are human-readable, NOT base units.\nFrame must select Robinhood mainnet (4663).\nCheck a saved pending transaction without resending: cargo run --example swap_frame -- --resume";

struct Options {
    pair: Pair,
    token_address: Address,
    side: TradeSide,
    amount: String,
    slippage_bps: u16,
    native: bool,
    buy_with: BuyWith,
    broadcast: bool,
    wallet: Option<Address>,
}

#[derive(Debug, PartialEq, Eq)]
enum Pair {
    Address(Address),
    V4Id(B256),
}

impl Pair {
    fn parse(value: &str) -> Result<Self> {
        if value.len() == 66 {
            Ok(Self::V4Id(value.parse()?))
        } else {
            Ok(Self::Address(value.parse()?))
        }
    }
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut positional = Vec::new();
        let mut native = NATIVE_SETTLEMENT;
        let mut broadcast = false;
        let mut wallet = Some(FROM_WALLET.parse()?);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--native" => native = true,
                "--broadcast" => broadcast = true,
                "--wallet" => {
                    wallet = Some(args.next().ok_or("--wallet needs an address")?.parse()?)
                }
                flag if flag.starts_with("--") => {
                    return Err(format!("Unknown option: {flag}").into());
                }
                _ => positional.push(arg.as_str()),
            }
        }
        if !matches!(positional.len(), 0 | 3 | 5) {
            return Err(HELP.into());
        }
        let side = match positional.first().copied() {
            None => SIDE,
            Some("buy") => TradeSide::Buy,
            Some("sell") => TradeSide::Sell,
            _ => return Err("Side must be buy or sell".into()),
        };
        let pair = Pair::parse(positional.get(3).copied().unwrap_or(PAIR_ADDRESS))?;
        let token_address = positional
            .get(4)
            .copied()
            .unwrap_or(TOKEN_ADDRESS)
            .parse()?;
        let amount = positional.get(1).copied().unwrap_or(AMOUNT_IN).to_owned();
        decimal_parts(&amount)?;
        let slippage_bps = positional
            .get(2)
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(SLIPPAGE_BPS);
        if slippage_bps > 10_000 {
            return Err("Slippage exceeds 10000 basis points".into());
        }
        if !amount.bytes().any(|digit| matches!(digit, b'1'..=b'9')) {
            return Err("Input amount must be nonzero".into());
        }
        Ok(Self {
            pair,
            token_address,
            side,
            amount,
            slippage_bps,
            native,
            buy_with: BUY_WITH,
            broadcast,
            wallet,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    let frame = provider(FRAME_RPC, Duration::from_secs(600))?;
    let reads = provider(READ_RPC, Duration::from_secs(30))?;
    let client = EvmClient::connect(reads, ROBINHOOD_CHAIN_ID).await?;
    let mut execution = Execution::new(
        frame.clone(),
        FileJournal,
        ExecutionOptions {
            check_rpc_account: true,
            ..Default::default()
        },
    );
    if args == ["--resume"] {
        let record = load_record()?;
        let block = client
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or("Latest block was not returned")?
            .header;
        let router = FeeRouter::connect(client, block.hash).await?;
        let wallet = record
            .transactions
            .first()
            .and_then(|tx| tx.transaction.from)
            .ok_or("Pending record has no wallet")?;
        let result = router.trader(wallet).status(&execution, &record).await?;
        present(&result)?;
        println!("Checked the saved transaction only. No new approval or swap was requested.");
        return Ok(());
    }
    let options = Options::parse(&args)?;
    if options.broadcast && Path::new(PENDING).exists() {
        return Err("A transaction is unresolved. Run --resume before another broadcast; no transaction was sent".into());
    }
    check_chain(&frame).await?;
    let wallet = select_wallet(frame.get_accounts().await?, options.wallet)?;
    println!("Frame wallet: {wallet}; Robinhood mainnet (4663)");
    let trial = prepare(&client, wallet, &options).await?;
    if !options.broadcast {
        match execution.preview(&client, &trial.prepared).await? {
            Readiness::ApprovalRequired => {
                println!("Swap simulation requires the approval first; no approval was sent.")
            }
            Readiness::Ready => println!("Swap simulation passed at current state."),
        }
        println!("Preview only. Add --broadcast to request signatures in Frame.");
        return Ok(());
    }
    let trader = trial.router.trader(wallet).buy_with(options.buy_with);
    let trader = if options.native {
        trader.with_native_settlement()
    } else {
        trader
    };
    let result = trader.swap_prepared(&mut execution, trial.prepared).await?;
    present(&result)?;
    Ok(())
}

fn provider(url: &str, timeout: Duration) -> Result<RootProvider> {
    let http = reqwest::Client::builder().timeout(timeout).build()?;
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_client(RpcClient::new_http_with_client(http, url.parse()?)))
}

fn select_wallet(accounts: Vec<Address>, requested: Option<Address>) -> Result<Address> {
    if let Some(wallet) = requested {
        if accounts.contains(&wallet) {
            return Ok(wallet);
        }
        return Err(
            "The configured wallet is not authorized in Frame; check FROM_WALLET or --wallet"
                .into(),
        );
    }
    match accounts.as_slice() {
        [wallet] => Ok(*wallet),
        [] => Err("Unlock Frame and allow its native connection to this app".into()),
        _ => Err("Frame exposed multiple accounts; select one using --wallet ADDRESS".into()),
    }
}

async fn check_chain(frame: &impl Provider) -> Result<()> {
    if frame.get_chain_id().await? != ROBINHOOD_CHAIN_ID {
        return Err("Select Robinhood mainnet (4663) in Frame".into());
    }
    Ok(())
}

struct Trial {
    router: FeeRouter<RootProvider>,
    prepared: PreparedSwap,
}

async fn prepare(
    client: &EvmClient<RootProvider>,
    wallet: Address,
    options: &Options,
) -> Result<Trial> {
    let block = client
        .provider()
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or("Latest block was not returned")?
        .header;
    let router = FeeRouter::connect(client.clone(), block.hash).await?;
    let (tokens, v4_key) = resolve_tokens(client, options, block.hash).await?;
    let decimals = input_decimals(client.provider(), tokens, options, block.hash).await?;
    let amount_in = parse_amount(&options.amount, decimals)?;
    let mut trader = router.trader(wallet).buy_with(options.buy_with);
    if options.native {
        trader = trader.with_native_settlement();
    }
    let limits = SwapLimits {
        slippage_bps: options.slippage_bps,
        deadline_unix_seconds: block.timestamp + 300,
    };
    let request = if let Some(key) = v4_key {
        TradeRequest::V4 {
            request: PairSwap {
                pair: V4PoolLookup::Key(key),
                token_address: options.token_address,
                side: options.side,
                amount_in,
            },
            options: V4QuoteOptions::default(),
        }
    } else {
        let Pair::Address(pair) = options.pair else {
            return Err("V4 pool key was not resolved".into());
        };
        TradeRequest::Pair(PairSwap {
            pair,
            token_address: options.token_address,
            side: options.side,
            amount_in,
        })
    };
    let prepared = match request.clone() {
        TradeRequest::Pair(request) => trader.prepare_swap(request, limits).await?,
        TradeRequest::V4 { request, options } => {
            trader.prepare_v4_swap(request, options, limits).await?
        }
    };
    let trade = &prepared.quote.request.trade;
    for (index, hop) in prepared.quote.request.pool.iter().enumerate() {
        let pool = match &hop.pool {
            UniswapPool::V2(pool) => pool.address.to_string(),
            UniswapPool::V3(pool) => pool.address.to_string(),
            UniswapPool::V4(pool) => pool.key.id().to_string(),
        };
        println!(
            "Hop {}: {:?} → {:?}; pool {pool}",
            index + 1,
            hop.currency_in,
            hop.currency_out
        );
    }
    if let Some(funding) = &prepared.funding {
        if let DiscoveryOutcome::Searched {
            coverage, failures, ..
        } = &funding.discovery.outcome
        {
            println!("Funding search: {coverage:?}");
            for failure in failures {
                println!("Funding discovery failure: {failure:?}");
            }
        }
        for failure in &funding.quote_failures {
            println!("Funding quote failure: {failure:?}");
        }
    }
    println!(
        "Block {}; {:?} → {:?}; gross input {} tokens ({} base units); fee {} tokens ({} bps); quoted output {} base units; slippage {} bps",
        block.number,
        trade.currency_in,
        trade.currency_out,
        options.amount,
        trade.amount_in,
        display_amount(router.input_fee(trade.amount_in), decimals),
        router.fee_bps(),
        prepared.quote.amount_out,
        options.slippage_bps
    );
    println!(
        "Minimum output: {} base units",
        prepared.minimum_amount_out()
    );
    match &prepared.price_impact {
        Ok(impact) => println!(
            "Estimated route impact excluding fees: {} bps; total cost including fees: {} bps; pool fees per hop: {:?} pips",
            impact.price_impact_bps, impact.total_cost_bps, impact.pool_fee_pips
        ),
        Err(reason) => println!("Price impact unavailable: {reason}"),
    }
    Ok(Trial { router, prepared })
}

async fn resolve_tokens(
    client: &EvmClient<RootProvider>,
    options: &Options,
    block: B256,
) -> Result<([Address; 2], Option<V4PoolKey>)> {
    match options.pair {
        Pair::Address(pair) => {
            let (token0, token1) = futures_util::try_join!(
                read_contract(client.provider(), pair, token0Call {}, block),
                read_contract(client.provider(), pair, token1Call {}, block),
            )?;
            Ok(([token0, token1], None))
        }
        Pair::V4Id(id) => {
            let initialized = V4_INITIALIZATION_BLOCK.ok_or("For a V4 pool ID, set V4_INITIALIZATION_BLOCK = Some(block_number) so the SDK can resolve its currencies")?;
            let deployment = UniswapDeployment::robinhood_mainnet()
                .v4
                .ok_or("V4 is not configured")?;
            let adapter = UniswapV4::connect(client.clone(), deployment, block).await?;
            let pool = adapter
                .resolve_pool(
                    V4PoolLookup::Id {
                        id,
                        ranges: vec![initialized..=initialized],
                    },
                    block,
                )
                .await?;
            Ok(([pool.key.currency0, pool.key.currency1], Some(pool.key)))
        }
    }
}

async fn input_decimals(
    provider: &impl Provider,
    [token0, token1]: [Address; 2],
    options: &Options,
    block: B256,
) -> Result<u8> {
    if options.token_address != token0 && options.token_address != token1 {
        return Err("Target token is not in the selected pair".into());
    }
    let input = match options.side {
        TradeSide::Buy => match options.buy_with.currency(ROBINHOOD_CHAIN_ID)? {
            evm_trading_sdk::dex::Currency::Native => return Ok(18),
            evm_trading_sdk::dex::Currency::Erc20(address) => address,
        },
        TradeSide::Sell => options.token_address,
    };
    if input.is_zero() {
        return Ok(18);
    }
    // The SDK validates the pool before quoting; these reads only determine the amount's scale.
    read_contract(provider, input, decimalsCall {}, block).await
}

async fn read_contract<C: SolCall>(
    provider: &impl Provider,
    address: Address,
    call: C,
    block: B256,
) -> Result<C::Return> {
    let data = provider
        .call(TransactionRequest {
            to: Some(address.into()),
            input: call.abi_encode().into(),
            ..Default::default()
        })
        .block(BlockId::hash_canonical(block))
        .await?;
    Ok(C::abi_decode_returns_validate(&data)?)
}

fn decimal_parts(amount: &str) -> Result<(&str, &str)> {
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (amount.contains('.') && fraction.is_empty())
    {
        return Err(
            "Amount must be a decimal string such as 1 or 0.25; no signs, commas or exponents"
                .into(),
        );
    }
    Ok((whole, fraction.trim_end_matches('0')))
}

fn parse_amount(amount: &str, decimals: u8) -> Result<U256> {
    let (whole, fraction) = decimal_parts(amount)?;
    let decimals = usize::from(decimals);
    if fraction.len() > decimals {
        return Err(
            format!("Amount {amount} exceeds the input token's {decimals} decimal places").into(),
        );
    }
    let digits = format!("{whole}{fraction}{}", "0".repeat(decimals - fraction.len()));
    let value = digits
        .trim_start_matches('0')
        .parse::<U256>()
        .map_err(|_| "Amount is zero or exceeds the supported integer range")?;
    if value.is_zero() {
        return Err("Input amount must be nonzero".into());
    }
    Ok(value)
}

fn display_amount(amount: U256, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    let digits = format!(
        "{:0>width$}",
        amount.to_string(),
        width = usize::from(decimals) + 1
    );
    let (whole, fraction) = digits.split_at(digits.len() - usize::from(decimals));
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}

struct FileJournal;

impl ExecutionJournal for FileJournal {
    fn begin(
        &mut self,
        record: &ExecutionRecord,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        fs::create_dir_all("swap-runs")?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(PENDING)?;
        serde_json::to_writer_pretty(&file, record)?;
        file.sync_all()?;
        println!("Review the request in Frame. Pending record: {PENDING}");
        Ok(())
    }

    fn save(
        &mut self,
        record: &ExecutionRecord,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let file = fs::File::create("swap-runs/pending.json.tmp")?;
        serde_json::to_writer_pretty(&file, record)?;
        file.sync_all()?;
        fs::rename("swap-runs/pending.json.tmp", PENDING)?;
        Ok(())
    }
}

fn load_record() -> Result<ExecutionRecord> {
    let value: Value = serde_json::from_slice(&fs::read(PENDING)?)?;
    if value.get("transactions").is_some() {
        return Ok(serde_json::from_value(value)?);
    }
    let kind = match value["kind"].as_str() {
        Some("approval") => TransactionKind::Approval,
        Some("swap") => TransactionKind::Swap,
        _ => return Err("Unknown pending record kind".into()),
    };
    let submission = match value.get("hash") {
        Some(hash) => SubmissionState::Submitted(serde_json::from_value(hash.clone())?),
        None => SubmissionState::Unknown,
    };
    Ok(ExecutionRecord {
        transactions: vec![TransactionRecord {
            kind,
            submission,
            transaction: serde_json::from_value(value["transaction"].clone())?,
            receipt: None,
            signed_hash: None,
        }],
    })
}

fn present(result: &ExecutionResult) -> Result<()> {
    println!("Execution: {:?}", result.status);
    if result.status == ExecutionStatus::Confirmed && result.record.swap_hash().is_none() {
        println!(
            "Approval confirmed; no swap was submitted. Resume never starts another transaction."
        );
    }
    for transaction in &result.record.transactions {
        println!("{:?}: {:?}", transaction.kind, transaction.submission);
    }
    if result.status == ExecutionStatus::Pending {
        println!("Pending or unknown. --resume only checks status; it never resends.");
    } else {
        FileJournal
            .save(&result.record)
            .map_err(|error| -> Box<dyn std::error::Error> { error })?;
        let transaction = result
            .record
            .transactions
            .last()
            .ok_or("Empty execution record")?;
        let name = match transaction.submission {
            SubmissionState::Submitted(hash) => hash.to_string(),
            _ => format!(
                "rejected-{}",
                transaction.transaction.nonce.unwrap_or_default()
            ),
        };
        fs::rename(PENDING, format!("swap-runs/{name}.json"))?;
    }
    Ok(())
}

sol! {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function decimals() external view returns (uint8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::mock::Asserter;
    use alloy_sol_types::SolValue;
    use evm_trading_sdk::Bytes;

    #[test]
    fn commands_use_file_settings_and_require_a_broadcast_flag() {
        let args = |text: &str| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let defaults = Options::parse(&[]).unwrap();
        assert_eq!(defaults.amount, AMOUNT_IN);
        assert_eq!(defaults.pair, Pair::parse(PAIR_ADDRESS).unwrap());
        assert_eq!(
            defaults.token_address,
            TOKEN_ADDRESS.parse::<Address>().unwrap()
        );
        assert_eq!(defaults.side, SIDE);
        assert_eq!(defaults.buy_with, BUY_WITH);
        assert!(!defaults.broadcast);
        assert!(Options::parse(&args("--broadcast")).unwrap().broadcast);
        assert!(Options::parse(&args("buy")).is_err());
        assert!(Options::parse(&args("buy 0 50")).is_err());
        assert!(Options::parse(&args("buy 100 10001")).is_err());
        assert!(!Options::parse(&args("buy 1000000 50")).unwrap().broadcast);
        assert_eq!(
            Options::parse(&args("buy 1000000 50")).unwrap().wallet,
            Some(FROM_WALLET.parse().unwrap())
        );
        let override_wallet = Address::repeat_byte(9);
        assert_eq!(
            Options::parse(&args(&format!("buy 1000000 50 --wallet {override_wallet}")))
                .unwrap()
                .wallet,
            Some(override_wallet)
        );
        assert!(
            Options::parse(&args("sell 1000000 50 --broadcast"))
                .unwrap()
                .broadcast
        );
    }

    #[test]
    fn human_amounts_convert_exactly_for_token_decimals() {
        assert_eq!(parse_amount("1.5", 6).unwrap(), U256::from(1_500_000));
        assert_eq!(
            parse_amount("0.000000000000000001", 18).unwrap(),
            U256::from(1)
        );
        assert_eq!(parse_amount("12.000", 0).unwrap(), U256::from(12));
        assert_eq!(
            parse_amount("0001.2300000", 6).unwrap(),
            U256::from(1_230_000)
        );
        assert_eq!(parse_amount(&U256::MAX.to_string(), 0).unwrap(), U256::MAX);
    }

    #[test]
    fn pool_inputs_accept_addresses_and_v4_ids() {
        let address = Address::repeat_byte(1);
        let id = B256::repeat_byte(2);
        assert_eq!(
            Pair::parse(&address.to_string()).unwrap(),
            Pair::Address(address)
        );
        assert_eq!(Pair::parse(&id.to_string()).unwrap(), Pair::V4Id(id));
        assert!(Pair::parse("0x1234").is_err());
    }

    #[tokio::test]
    async fn native_v4_input_uses_eighteen_decimals_without_token_calls() {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(Asserter::new());
        let mut options = Options::parse(&[]).unwrap();
        options.side = TradeSide::Buy;
        let tokens = [Address::ZERO, options.token_address];
        assert_eq!(
            input_decimals(&provider, tokens, &options, B256::ZERO)
                .await
                .unwrap(),
            18
        );
        options.side = TradeSide::Sell;
        options.token_address = Address::ZERO;
        assert_eq!(
            input_decimals(&provider, tokens, &options, B256::ZERO)
                .await
                .unwrap(),
            18
        );
    }

    #[test]
    fn human_amounts_never_round_or_use_floating_point() {
        for amount in [
            "0",
            "0.0",
            "-1",
            "+1",
            "1e6",
            "1,000",
            "1.2.3",
            ".1",
            "1.",
            " 1",
            "0x10",
            "NaN",
            "1.0000001",
        ] {
            assert!(parse_amount(amount, 6).is_err(), "accepted {amount}");
        }
        assert!(parse_amount("1.1", 0).is_err());
        assert!(parse_amount(&U256::MAX.to_string(), 1).is_err());
    }

    #[test]
    fn human_display_preserves_small_and_large_amounts() {
        assert_eq!(display_amount(U256::from(1_500_000), 6), "1.5");
        assert_eq!(display_amount(U256::from(1), 18), "0.000000000000000001");
        assert_eq!(display_amount(U256::ZERO, 18), "0");
        assert_eq!(display_amount(U256::from(12), 0), "12");
        assert_eq!(
            parse_amount(&display_amount(U256::MAX, 255), 255).unwrap(),
            U256::MAX
        );
    }

    #[tokio::test]
    async fn missing_decimals_stop_amount_conversion() {
        let rpc = Asserter::new();
        let mut options = Options::parse(&[]).unwrap();
        options.side = TradeSide::Sell;
        rpc.push_success(&Bytes::new());
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(rpc);
        assert!(
            input_decimals(
                &provider,
                [Address::repeat_byte(1), options.token_address],
                &options,
                B256::repeat_byte(7)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn sell_amount_uses_target_decimals_and_usdc_requires_configuration() {
        let rpc = Asserter::new();
        rpc.push_success(&Bytes::from(U256::from(6).abi_encode()));
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(rpc.clone());
        let mut options = Options::parse(&[]).unwrap();
        options.side = TradeSide::Sell;
        let tokens = [Address::repeat_byte(1), options.token_address];
        let decimals = input_decimals(&provider, tokens, &options, B256::ZERO)
            .await
            .unwrap();
        assert_eq!(
            parse_amount("1.25", decimals).unwrap(),
            U256::from(1_250_000)
        );
        options.side = TradeSide::Buy;
        options.buy_with = BuyWith::Usdc;
        assert!(
            input_decimals(&provider, tokens, &options, B256::ZERO)
                .await
                .is_err()
        );
        assert!(rpc.read_q().is_empty());
    }

    #[test]
    fn multiple_frame_accounts_need_an_explicit_wallet() {
        let accounts = vec![Address::repeat_byte(1), Address::repeat_byte(2)];
        assert!(select_wallet(accounts.clone(), None).is_err());
        assert_eq!(
            select_wallet(accounts.clone(), Some(accounts[1])).unwrap(),
            accounts[1]
        );
        assert!(select_wallet(accounts, Some(Address::repeat_byte(3))).is_err());
    }
}
