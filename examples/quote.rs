use alloy_primitives::{Address, B256, U256, address, aliases::U24};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use evm_trading_sdk::{
    Dex, DiscoverPools, EvmClient, Result,
    dex::{Currency, ExactInput, QuoteRequest},
    uniswap::{
        DiscoveryOutcome, UniswapDeployment,
        v2::{UniswapV2, V2PoolQuery},
        v3::{UniswapV3, V3PoolQuery},
    },
};

const RPC_URL: &str = "https://rpc.mainnet.chain.robinhood.com";
const USDG: Address = address!("5fc5360d0400a0fd4f2af552add042d716f1d168");
const AC: Address = address!("fad40755de679b262337f8d964d75cac51f8c643");
const WALLET: Address = address!("37c264d30ddde7a8f1817e6e8917c8a9926e26ce");

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let token_out: Address = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(AC);
    let amount_in: U256 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(U256::from(100_000_000));
    let requested_block: Option<B256> = args.next().map(|value| value.parse()).transpose()?;
    if args.next().is_some() {
        return Err(
            "usage: cargo run --example quote -- [output_token] [input_base_units] [block_hash]"
                .into(),
        );
    }
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(RPC_URL.parse()?);
    let deployment = UniswapDeployment::robinhood_mainnet();
    let client = EvmClient::connect(provider, deployment.chain_id).await?;
    let block = match requested_block {
        Some(block) => block,
        None => {
            client
                .provider()
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or("latest block was not returned")?
                .header
                .hash
        }
    };
    println!("Chain {} at block {block}", deployment.chain_id);
    println!("Input: {amount_in} USDG base units; output token: {token_out}; wallet: {WALLET}");
    println!("Quote only; no wallet balance requirement or swap simulation.");
    for token in [USDG, token_out] {
        match client.token_info(token, block).await {
            Ok(info) => println!(
                "Token {token}: symbol={:?}, decimals={:?}",
                info.symbol, info.decimals
            ),
            Err(error) => println!("Token {token} metadata failed: {error:?}"),
        }
    }
    let trade = ExactInput {
        chain_id: deployment.chain_id,
        currency_in: Currency::Erc20(USDG),
        currency_out: Currency::Erc20(token_out),
        amount_in,
        sender: WALLET,
        recipient: WALLET,
    };
    let mut quotes = Vec::new();
    if let Some(config) = deployment.v2 {
        match UniswapV2::connect(client.clone(), config, block).await {
            Ok(adapter) => {
                let query = V2PoolQuery {
                    chain_id: trade.chain_id,
                    currency_a: trade.currency_in,
                    currency_b: trade.currency_out,
                };
                match adapter.find_pools(query, block).await {
                    Ok(search) => {
                        for pool in print_search("V2", search.outcome) {
                            let address = pool.address;
                            let result = quote_pool(&adapter, pool, &trade, block).await;
                            record_quote("V2", address, result, &mut quotes);
                        }
                    }
                    Err(error) => println!("V2 discovery failed: {error:?}"),
                }
            }
            Err(error) => println!("V2 connection failed: {error:?}"),
        }
    }
    if let Some(config) = deployment.v3 {
        match UniswapV3::connect(client, config, block).await {
            Ok(adapter) => {
                let query = V3PoolQuery {
                    chain_id: trade.chain_id,
                    currency_a: trade.currency_in,
                    currency_b: trade.currency_out,
                    fee_tiers: [100, 500, 3000, 10_000].map(U24::from).to_vec(),
                };
                match adapter.find_pools(query, block).await {
                    Ok(search) => {
                        for pool in print_search("V3", search.outcome) {
                            let address = pool.address;
                            println!("V3 pool {address}, fee tier {}", pool.fee);
                            let result = quote_pool(&adapter, pool, &trade, block).await;
                            record_quote("V3", address, result, &mut quotes);
                        }
                    }
                    Err(error) => println!("V3 discovery failed: {error:?}"),
                }
            }
            Err(error) => println!("V3 connection failed: {error:?}"),
        }
    }
    match quotes.into_iter().max_by_key(|(_, amount)| *amount) {
        Some((pool, amount)) => println!(
            "Highest output among successful quotes: {amount} output base units via {pool}; excludes gas."
        ),
        None => println!("No successful quote in the searched direct V2/V3 scope."),
    }
    println!("V4 pools and multi-hop routes were not searched.");
    Ok(())
}

fn print_search<P>(version: &str, outcome: DiscoveryOutcome<P>) -> Vec<P> {
    match outcome {
        DiscoveryOutcome::NotConfigured => {
            println!("{version} is not configured.");
            Vec::new()
        }
        DiscoveryOutcome::Searched {
            pools,
            coverage,
            failures,
        } => {
            println!(
                "{version} requested: {:?}; completed: {:?}; pools: {}",
                coverage.requested,
                coverage.completed,
                pools.len()
            );
            for failure in failures {
                println!("{version} {:?} failed: {:?}", failure.scope, failure.error);
            }
            pools
        }
    }
}

async fn quote_pool<D: Dex<QuoteOptions = ()>>(
    adapter: &D,
    pool: D::Pool,
    trade: &ExactInput,
    block_hash: B256,
) -> Result<U256> {
    let quote = adapter
        .quote(QuoteRequest {
            pool,
            trade: trade.clone(),
            block_hash,
            options: (),
        })
        .await?;
    Ok(quote.amount_out)
}

fn record_quote(
    version: &str,
    pool: Address,
    result: Result<U256>,
    quotes: &mut Vec<(String, U256)>,
) {
    match result {
        Ok(amount) => {
            println!("{version} pool {pool}: {amount} output base units");
            quotes.push((format!("{version} pool {pool}"), amount));
        }
        Err(error) => println!("{version} pool {pool} quote failed: {error:?}"),
    }
}
