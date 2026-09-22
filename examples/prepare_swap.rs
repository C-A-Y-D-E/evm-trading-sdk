use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use evm_trading_sdk::{
    Address, EvmClient, FeeRouter, PairSwap, TradeSide, U256,
    dex::SwapLimits,
    uniswap::{
        UniswapDeployment,
        v4::{V4PoolLookup, V4QuoteOptions},
    },
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() > 6 {
        return Err("usage: prepare_swap [buy|sell] [input_base_units] [slippage_bps] [pair_address_or_v4_id] [target_token] [v4_initialization_block]".into());
    }
    let token_address: Address = args
        .get(4)
        .map(String::as_str)
        .unwrap_or("0xd0601ce157db5bdc3162bbac2a2c8af5320d9eec")
        .parse()?;
    let pool_id = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("0xb944cec30bd4175855215d767adc81f39e5f7e2b");
    let wallet: Address = "0xd87013169365b833f86acec8f6084a57a0617f00".parse()?;
    let (side, default_amount) = match args.first().map(String::as_str).unwrap_or("buy") {
        "buy" => (TradeSide::Buy, "100000000000000"),
        "sell" => (TradeSide::Sell, "1000000000000000000"),
        _ => return Err("side must be buy or sell".into()),
    };
    let amount_in: U256 = args
        .get(1)
        .map(String::as_str)
        .unwrap_or(default_amount)
        .parse()?;
    let slippage_bps: u16 = args.get(2).map(String::as_str).unwrap_or("50").parse()?;
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http("https://rpc.mainnet.chain.robinhood.com".parse()?);
    let deployment = UniswapDeployment::robinhood_mainnet();
    let client = EvmClient::connect(provider, deployment.chain_id).await?;
    let header = client
        .provider()
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or("latest block was not returned")?
        .header;
    let router = FeeRouter::connect(client.clone(), header.hash).await?;
    let trader = router.trader(wallet);
    let limits = SwapLimits {
        slippage_bps,
        deadline_unix_seconds: header.timestamp + 300,
    };
    let prepared = if pool_id.len() == 66 {
        let initialization_block: u64 = args
            .get(5)
            .ok_or("a V4 pool ID needs its initialization block")?
            .parse()?;
        trader
            .prepare_v4_swap(
                PairSwap {
                    pair: V4PoolLookup::Id {
                        id: pool_id.parse()?,
                        ranges: vec![initialization_block..=initialization_block],
                    },
                    token_address,
                    side,
                    amount_in,
                },
                V4QuoteOptions::default(),
                limits,
            )
            .await?
    } else {
        trader
            .prepare_swap(
                PairSwap {
                    pair: pool_id.parse()?,
                    token_address,
                    side,
                    amount_in,
                },
                limits,
            )
            .await?
    };
    let plan = prepared.plan;
    println!(
        "Block {} ({}), selected pool {}",
        header.number, header.hash, pool_id
    );
    println!(
        "Gross input: {amount_in}; fee: {}; quoted output: {}",
        router.input_fee(amount_in),
        prepared.quote.amount_out
    );
    println!(
        "FeeRouter: {}; slippage: {slippage_bps} bps",
        router.address()
    );
    match &prepared.price_impact {
        Ok(impact) => println!(
            "Price impact excluding fees and taxes: {} bps; pre-swap pool-price reference after platform fee: {} output base units; known core fees: {:?} pips; known hook fees: {:?}",
            impact.price_impact_bps,
            impact.market_amount_out,
            impact.pool_fee_pips,
            impact.hook_fees
        ),
        Err(reason) => println!("Price impact unavailable: {reason}"),
    }
    println!("Unsigned transaction: {:?}", plan.transaction);
    if let Some(approval) = plan.approval {
        let allowance = client
            .allowance(
                approval.token,
                approval.owner,
                approval.spender,
                header.hash,
            )
            .await?;
        println!(
            "Allowance: {allowance}; required: {}; spender: {}",
            approval.amount, approval.spender
        );
        println!(
            "Approval needed at this block: {}",
            allowance < approval.amount
        );
    }
    println!("Read-only example: no approval, wallet signature, or swap was submitted.");
    Ok(())
}
