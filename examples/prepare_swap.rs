use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use evm_trading_sdk::{
    Address, EvmClient, FeeRouter, PairSwap, TradeSide, U256, dex::SwapLimits,
    uniswap::UniswapDeployment,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() > 5 {
        return Err("usage: prepare_swap [buy|sell] [input_base_units] [slippage_bps] [pair_address] [target_token]".into());
    }
    let token_address: Address = args
        .get(4)
        .map(String::as_str)
        .unwrap_or("0xd0601ce157db5bdc3162bbac2a2c8af5320d9eec")
        .parse()?;
    let pool_address: Address = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("0xb944cec30bd4175855215d767adc81f39e5f7e2b")
        .parse()?;
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
    let prepared = trader
        .prepare_swap(
            PairSwap {
                pair: pool_address,
                token_address,
                side,
                amount_in,
            },
            SwapLimits {
                slippage_bps,
                deadline_unix_seconds: header.timestamp + 300,
            },
        )
        .await?;
    let plan = prepared.plan;
    println!(
        "Block {} ({}), selected pool {}",
        header.number, header.hash, pool_address
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
