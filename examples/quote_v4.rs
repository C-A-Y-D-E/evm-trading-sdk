use alloy_primitives::{B256, U256, address};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use evm_trading_sdk::{
    Dex, EvmClient, ResolvePool,
    dex::{Currency, ExactInput, QuoteRequest, SwapLimits},
    uniswap::{
        UniswapDeployment,
        v4::{UniswapV4, V4PoolLookup, V4QuoteOptions},
    },
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() && args.len() != 3 {
        return Err(
            "usage: quote_v4 [pool_id initialization_block token0_amount_base_units]".into(),
        );
    }
    let pool_id: B256 = args
        .first()
        .map(String::as_str)
        .unwrap_or("0x027f6c0fd8847365e77eae2910169a1b92c513c6f1dadd38fde3ecd7806df5d1")
        .parse()?;
    let initialization_block: u64 = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("61179710")
        .parse()?;
    let amount_in: U256 = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("1000000000000000000")
        .parse()?;
    let deployment = UniswapDeployment::robinhood_mainnet();
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http("https://rpc.mainnet.chain.robinhood.com".parse()?);
    let client = EvmClient::connect(provider, deployment.chain_id).await?;
    let header = client
        .provider()
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or("missing block")?
        .header;
    let adapter = UniswapV4::connect(
        client,
        deployment.v4.ok_or("V4 not configured")?,
        header.hash,
    )
    .await?;
    let pool = adapter
        .resolve_pool(
            V4PoolLookup::Id {
                id: pool_id,
                ranges: vec![initialization_block..=initialization_block],
            },
            header.hash,
        )
        .await?;
    let key = &pool.key;
    let wallet = address!("37c264d30ddde7a8f1817e6e8917c8a9926e26ce");
    let trade = ExactInput {
        chain_id: deployment.chain_id,
        currency_in: if key.currency0.is_zero() {
            Currency::Native
        } else {
            Currency::Erc20(key.currency0)
        },
        currency_out: Currency::Erc20(key.currency1),
        amount_in,
        sender: wallet,
        recipient: wallet,
    };
    println!(
        "Token0 {}, token1 {}, fee {}, tick spacing {}, hook {}",
        key.currency0, key.currency1, key.fee, key.tick_spacing, key.hooks
    );
    let quote = adapter
        .quote(QuoteRequest {
            pool,
            trade,
            block_hash: header.hash,
            options: V4QuoteOptions::default(),
        })
        .await?;
    let plan = adapter.build_swap(
        &quote,
        SwapLimits {
            minimum_amount_out: quote.amount_out * U256::from(99) / U256::from(100),
            deadline_unix_seconds: header.timestamp + 300,
        },
    )?;
    println!(
        "Block {} ({}), pool {}",
        header.number,
        header.hash,
        quote.request.pool.key.id()
    );
    println!(
        "{} token0 base units -> {} token1 base units",
        amount_in, quote.amount_out
    );
    println!(
        "Unsigned swap to {:?}, {} calldata bytes",
        plan.transaction.to,
        plan.transaction.input.input().map_or(0, |data| data.len())
    );
    if plan.token_approval.is_some() {
        println!("Requires ERC-20 approval to Permit2 and Permit2 allowance to Universal Router.");
    }
    println!(
        "No transaction sent; this direct Universal Router plan does not use FeeRouter or charge its 1% fee."
    );
    Ok(())
}
