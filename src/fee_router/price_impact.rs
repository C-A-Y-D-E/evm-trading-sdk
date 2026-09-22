use alloy_primitives::{Address, B256, U256, Uint, address, aliases::U160};
use alloy_provider::Provider;
use alloy_sol_types::sol;
use futures_util::future::join_all;

use super::RouteHop;
use crate::{
    Error, EvmClient,
    dex::Currency,
    uniswap::{UniswapDeployment, UniswapPool, V4Pool},
};

type Wide = Uint<1024, 16>;
const PONS_V2_HOOK: Address = address!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");
const SWAP_HOOK_FLAGS: u8 = (1 << 7) | (1 << 6) | (1 << 3) | (1 << 2);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookFees {
    pub hook_fee_bps: u16,
    pub creator_tax_bps: u16,
}

/// Same-block route estimate. Amounts are output base units, rounded down only at the end.
/// Basis points truncate toward zero; negative impact means a better-than-reference quote.
/// `market_amount_out` uses gross input and the product of pool spot prices, excluding fees.
/// `price_impact_bps` compares quoted output to that gross reference, including all quoted fees.
/// `total_cost_bps` is the same value, retained for compatibility. Gas is excluded.
/// Fee breakdown fields are optional and never determine whether impact can be calculated.
/// Pool fees are in millionths (100 pips = 1 bp). Gas and external market prices are excluded.
/// `hook_fees` lists known output fee/tax rates per hop; None means unknown, not zero.
/// V4 uses core slot0 as its reference even when a hook uses other pricing; this is not an oracle valuation.
/// Pool rounding and price boundaries can also affect the quote; this is an estimate, not a fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceImpact {
    pub market_amount_out: U256,
    pub market_amount_out_after_fees: Option<U256>,
    pub price_impact_bps: i32,
    pub total_cost_bps: i32,
    pub pool_fee_pips: Vec<Option<u32>>,
    pub hook_fees: Vec<Option<HookFees>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PriceImpactUnavailable {
    #[error("V4 swap hook or dynamic fee has no supported pricing model")]
    HookPricing,
    #[error("hook launch configuration does not match the selected pool or supported fee model")]
    HookConfiguration,
    #[error("repeated pools cannot use independent pre-trade prices")]
    RepeatedPool,
    #[error("pool state could not be read: {0}")]
    State(#[source] Box<Error>),
    #[error("reference price is zero or exceeds supported arithmetic precision")]
    Arithmetic,
}

pub(super) async fn estimate<P: Provider>(
    client: &EvmClient<P>,
    hops: &[RouteHop],
    block: B256,
    gross_input: U256,
    net_input: U256,
    amount_out: U256,
) -> Result<PriceImpact, PriceImpactUnavailable> {
    let references = join_all(hops.iter().map(|hop| reference(client, hop, block)))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    calculate(&references, gross_input, net_input, amount_out)
        .ok_or(PriceImpactUnavailable::Arithmetic)
}

struct Reference {
    numerator: Wide,
    denominator: Wide,
    fee_pips: Option<u32>,
    hook_fees: Option<HookFees>,
}

enum V4PriceModel {
    Core,
    PonsV2,
}

fn v4_model(chain: u64, pool: &V4Pool) -> Result<V4PriceModel, PriceImpactUnavailable> {
    if pool.key.fee.to::<u32>() == 0x800000 {
        return Err(PriceImpactUnavailable::HookPricing);
    }
    if pool.key.hooks.as_slice()[19] & SWAP_HOOK_FLAGS == 0 {
        return Ok(V4PriceModel::Core);
    }
    if chain == crate::ROBINHOOD_CHAIN_ID
        && pool.chain_id == chain
        && pool.key.hooks == PONS_V2_HOOK
        && Some(pool.manager)
            == UniswapDeployment::robinhood_mainnet()
                .v4
                .map(|v4| v4.pool_manager)
    {
        return Ok(V4PriceModel::PonsV2);
    }
    Err(PriceImpactUnavailable::HookPricing)
}

async fn hook_fees<P: Provider>(
    client: &EvmClient<P>,
    pool: &V4Pool,
    block: B256,
) -> Result<HookFees, PriceImpactUnavailable> {
    if matches!(v4_model(client.chain_id(), pool)?, V4PriceModel::Core) {
        return Ok(HookFees::default());
    }
    // This non-proxy deployment charges saved launch terms on exact-input output, in both directions.
    // Source: https://sourcify.dev/server/v2/contract/4663/0xe5e702641ea86f4ae6cc3cdaed2b886f976be044?fields=sources
    let info = client
        .call(
            pool.key.hooks,
            launchesCall {
                poolId: pool.key.id(),
            },
            block,
        )
        .await
        .map_err(state_error)?;
    let (memecoin, quote) = if info.memecoinIsCurrency0 {
        (pool.key.currency0, pool.key.currency1)
    } else {
        (pool.key.currency1, pool.key.currency0)
    };
    if !info.registered
        || info.memecoin != memecoin
        || info.quoteToken != quote
        || info.hookFeeBps > 1000
        || u32::from(info.hookFeeBps) + u32::from(info.creatorTaxBps) > 2000
    {
        return Err(PriceImpactUnavailable::HookConfiguration);
    }
    Ok(HookFees {
        hook_fee_bps: info.hookFeeBps,
        creator_tax_bps: info.creatorTaxBps,
    })
}

async fn reference<P: Provider>(
    client: &EvmClient<P>,
    hop: &RouteHop,
    block: B256,
) -> Result<Reference, PriceImpactUnavailable> {
    Ok(match &hop.pool {
        UniswapPool::V2(pool) => {
            let reserves = client
                .call(pool.address, getReservesCall {}, block)
                .await
                .map_err(state_error)?;
            let (input, output) = if hop.currency_in == Currency::Erc20(pool.token0) {
                (reserves.reserve0, reserves.reserve1)
            } else {
                (reserves.reserve1, reserves.reserve0)
            };
            Reference {
                numerator: Wide::from(output),
                denominator: Wide::from(input),
                fee_pips: Some(3000),
                hook_fees: Some(HookFees::default()),
            }
        }
        UniswapPool::V3(pool) => {
            let state = client
                .call(pool.address, slot0Call {}, block)
                .await
                .map_err(state_error)?;
            sqrt_reference(
                state.sqrtPriceX96,
                hop.currency_in == Currency::Erc20(pool.token0),
                pool.fee.to(),
            )
        }
        UniswapPool::V4(pool) => {
            let config = UniswapDeployment::robinhood_mainnet()
                .v4
                .expect("configured V4 route");
            let state_read = async {
                client
                    .call(
                        config.state_view,
                        getSlot0Call {
                            poolId: pool.key.id(),
                        },
                        block,
                    )
                    .await
                    .map_err(state_error)
            };
            let (state, hook_fees) =
                futures_util::join!(state_read, hook_fees(client, pool, block));
            let state = state?;
            let zero_for_one = match hop.currency_in {
                Currency::Native => pool.key.currency0 == Address::ZERO,
                Currency::Erc20(token) => token == pool.key.currency0,
            };
            let protocol = state.protocolFee.to::<u32>();
            let protocol = if zero_for_one {
                protocol & 0xfff
            } else {
                protocol >> 12
            };
            let lp = state.lpFee.to::<u32>();
            let fee = (protocol <= 1000 && lp <= 1_000_000 && pool.key.fee.to::<u32>() != 0x800000)
                .then(|| protocol + lp - (u64::from(protocol) * u64::from(lp) / 1_000_000) as u32);
            Reference {
                fee_pips: fee,
                hook_fees: hook_fees.ok(),
                ..sqrt_reference(state.sqrtPriceX96, zero_for_one, 0)
            }
        }
    })
}

fn state_error(error: Error) -> PriceImpactUnavailable {
    PriceImpactUnavailable::State(Box::new(error))
}

fn sqrt_reference(sqrt: U160, zero_for_one: bool, fee_pips: u32) -> Reference {
    let squared = Wide::from(sqrt) * Wide::from(sqrt);
    let scale = Wide::from(1) << 192;
    let (numerator, denominator) = if zero_for_one {
        (squared, scale)
    } else {
        (scale, squared)
    };
    Reference {
        numerator,
        denominator,
        fee_pips: Some(fee_pips),
        hook_fees: Some(HookFees::default()),
    }
}

fn calculate(
    references: &[Reference],
    gross: U256,
    net: U256,
    output: U256,
) -> Option<PriceImpact> {
    let mut market = Fraction::amount(gross);
    for reference in references {
        market.multiply(reference.numerator, reference.denominator)?;
    }
    let price_impact_bps = market.difference_bps(output)?;
    Some(PriceImpact {
        market_amount_out: market.floor()?,
        market_amount_out_after_fees: after_fee_reference(references, net),
        price_impact_bps,
        total_cost_bps: price_impact_bps,
        pool_fee_pips: references
            .iter()
            .map(|reference| reference.fee_pips)
            .collect(),
        hook_fees: references
            .iter()
            .map(|reference| reference.hook_fees.clone())
            .collect(),
    })
}

fn after_fee_reference(references: &[Reference], net: U256) -> Option<U256> {
    let mut after_fees = Fraction::amount(net);
    for reference in references {
        after_fees.multiply(reference.numerator, reference.denominator)?;
        after_fees.multiply(
            Wide::from(1_000_000u32.checked_sub(reference.fee_pips?)?),
            Wide::from(1_000_000),
        )?;
        // Both Pons cuts apply to the same output, so their rates add rather than compound.
        let fees = reference.hook_fees.as_ref()?;
        let hook_bps = u32::from(fees.hook_fee_bps) + u32::from(fees.creator_tax_bps);
        after_fees.multiply(
            Wide::from(10_000u32.checked_sub(hook_bps)?),
            Wide::from(10_000),
        )?;
    }
    after_fees.floor()
}

struct Fraction {
    numerator: Wide,
    denominator: Wide,
}

impl Fraction {
    fn amount(amount: U256) -> Self {
        Self {
            numerator: Wide::from(amount),
            denominator: Wide::from(1),
        }
    }

    // Reduce before multiplication, retaining fractional base units across all hops.
    fn multiply(&mut self, numerator: Wide, denominator: Wide) -> Option<()> {
        if numerator.is_zero() || denominator.is_zero() {
            return None;
        }
        let factor = gcd(numerator, denominator);
        let numerator = numerator / factor;
        let denominator = denominator / factor;
        let left = gcd(self.numerator, denominator);
        let right = gcd(numerator, self.denominator);
        self.numerator = (self.numerator / left).checked_mul(numerator / right)?;
        self.denominator = (self.denominator / right).checked_mul(denominator / left)?;
        Some(())
    }

    fn floor(&self) -> Option<U256> {
        let value = self.numerator / self.denominator;
        (value <= Wide::from(U256::MAX)).then(|| value.to())
    }

    fn difference_bps(&self, actual: U256) -> Option<i32> {
        if self.numerator.is_zero() {
            return None;
        }
        let actual = Wide::from(actual).checked_mul(self.denominator)?;
        let difference = self.numerator.abs_diff(actual);
        let bps = difference.checked_mul(Wide::from(10_000))? / self.numerator;
        if bps > Wide::from(i32::MAX as u32) {
            return None;
        }
        let bps = bps.to::<u32>() as i32;
        Some(if actual > self.numerator { -bps } else { bps })
    }
}

fn gcd(mut left: Wide, mut right: Wide) -> Wide {
    while !right.is_zero() {
        (left, right) = (right, left % right);
    }
    left
}

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 timestamp);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
    function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
    function launches(bytes32 poolId) external view returns (
        bool registered, bool memecoinIsCurrency0, address memecoin, address quoteToken,
        address creator, address buybackCreatorRecipient, address protocolFeeRecipient,
        uint16 creatorTaxBps, uint16 protocolFeeShareBps, uint16 buybackBurnBps,
        uint16 hookFeeBps, uint16 maxInternalPriceImpactBps, bool buybackEnabled
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ROBINHOOD_CHAIN_ID,
        uniswap::{V2Pool, V3Pool, V4Pool, V4PoolKey},
    };
    use alloy_primitives::{
        Bytes, U64,
        aliases::{I24, U24, U112},
    };
    use alloy_provider::{ProviderBuilder, mock::Asserter};
    use alloy_sol_types::{SolCall, SolValue};

    fn ratio(numerator: u64, denominator: u64, fee_pips: u32) -> Reference {
        Reference {
            numerator: Wide::from(numerator),
            denominator: Wide::from(denominator),
            fee_pips: Some(fee_pips),
            hook_fees: Some(HookFees::default()),
        }
    }

    #[test]
    fn route_impact_compounds_and_separates_fees() {
        let impact = calculate(
            &[ratio(2, 1, 10_000), ratio(3, 1, 20_000)],
            U256::from(10000),
            U256::from(9900),
            U256::from(57629),
        )
        .unwrap();
        assert_eq!(impact.market_amount_out, U256::from(60000));
        assert_eq!(impact.market_amount_out_after_fees, Some(U256::from(57629)));
        assert_eq!(impact.price_impact_bps, 395);
        assert_eq!(impact.total_cost_bps, 395);
        assert_eq!(impact.pool_fee_pips, [Some(10000), Some(20000)]);
        let impact = calculate(
            &[ratio(1, 1, 0), ratio(1, 1, 0)],
            U256::from(10000),
            U256::from(10000),
            U256::from(8100),
        )
        .unwrap();
        assert_eq!(impact.price_impact_bps, 1900);
    }

    #[tokio::test]
    async fn half_the_input_value_is_fifty_percent_for_every_pool_and_hook() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        let mut pools = hops();
        for dynamic in [false, true] {
            let mut hop = hops().remove(2);
            let UniswapPool::V4(pool) = &mut hop.pool else {
                unreachable!()
            };
            pool.key.hooks = Address::repeat_byte(9);
            if dynamic {
                pool.key.fee = U24::from(0x800000);
            }
            pools.push(hop);
        }
        for hop in pools {
            match &hop.pool {
                UniswapPool::V2(_) => rpc.push_success(&Bytes::from(
                    (U112::from(100), U112::from(100), 0u32).abi_encode(),
                )),
                UniswapPool::V3(_) => rpc.push_success(&Bytes::from(
                    (U160::from(1) << 96, I24::ZERO, 0u16, 0u16, 0u16, 0u16, true).abi_encode(),
                )),
                UniswapPool::V4(_) => rpc.push_success(&Bytes::from(
                    (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::from(3000)).abi_encode(),
                )),
            }
            let impact = estimate(
                &client,
                std::slice::from_ref(&hop),
                B256::repeat_byte(7),
                U256::from(100),
                U256::from(99),
                U256::from(50),
            )
            .await
            .unwrap();
            assert_eq!(impact.market_amount_out, U256::from(100));
            assert_eq!(impact.price_impact_bps, 5000);
            assert_eq!(impact.total_cost_bps, 5000);
            if let UniswapPool::V4(pool) = hop.pool {
                if !pool.key.hooks.is_zero() {
                    assert_eq!(impact.hook_fees, [None]);
                    assert_eq!(impact.market_amount_out_after_fees, None);
                }
                if pool.key.fee.to::<u32>() == 0x800000 {
                    assert_eq!(impact.pool_fee_pips, [None]);
                }
            }
        }
        assert!(rpc.read_q().is_empty());
    }

    #[tokio::test]
    async fn failed_optional_hook_reads_do_not_hide_price_impact() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        for invalid_terms in [false, true] {
            rpc.push_success(&Bytes::from(
                (U160::from(1) << 96, I24::ZERO, U24::ZERO, U24::ZERO).abi_encode(),
            ));
            if invalid_terms {
                let mut info = launch_info();
                info.registered = false;
                rpc.push_success(&Bytes::from(launchesCall::abi_encode_returns(&info)));
            } else {
                rpc.push_failure_msg("optional hook terms unavailable");
            }
            let impact = estimate(
                &client,
                &[pons_hop()],
                B256::repeat_byte(7),
                U256::from(100),
                U256::from(99),
                U256::from(50),
            )
            .await
            .unwrap();
            assert_eq!(impact.price_impact_bps, 5000);
            assert_eq!(impact.market_amount_out_after_fees, None);
            assert_eq!(impact.hook_fees, [None]);
        }
        assert!(rpc.read_q().is_empty());
    }

    #[test]
    fn optional_fee_arithmetic_does_not_change_the_gross_reference() {
        let impact = calculate(
            &[ratio(2, 1, 1_000_000)],
            U256::from(100),
            U256::ZERO,
            U256::from(100),
        )
        .unwrap();
        assert_eq!(impact.market_amount_out, U256::from(200));
        assert_eq!(impact.price_impact_bps, 5000);
        assert_eq!(impact.market_amount_out_after_fees, None);
    }

    #[test]
    fn fractional_base_units_survive_intermediate_hops() {
        let impact = calculate(
            &[ratio(1, 1000, 0), ratio(1000, 1, 0)],
            U256::from(100),
            U256::from(100),
            U256::from(90),
        )
        .unwrap();
        assert_eq!(impact.market_amount_out, U256::from(100));
        assert_eq!(impact.price_impact_bps, 1000);
    }

    #[test]
    fn token_decimals_do_not_change_the_ratio() {
        let impact = calculate(
            &[ratio(1, 1_000_000_000_000, 0)],
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(900000),
        )
        .unwrap();
        assert_eq!(impact.market_amount_out, U256::from(1000000));
        assert_eq!(impact.price_impact_bps, 1000);
    }

    #[test]
    fn sqrt_prices_support_both_directions_and_large_values() {
        for (reverse, expected) in [(false, 400), (true, 25)] {
            let price = sqrt_reference(U160::from(2) << 96, !reverse, 0);
            let impact = calculate(
                &[price],
                U256::from(100),
                U256::from(100),
                U256::from(expected),
            )
            .unwrap();
            assert_eq!(impact.market_amount_out, U256::from(expected));
            assert_eq!(impact.price_impact_bps, 0);
        }
        let huge = sqrt_reference(U160::MAX, true, 0);
        assert!(calculate(&[huge], U256::MAX, U256::MAX, U256::from(1)).is_none());
        let prices = [
            sqrt_reference(U160::MAX, true, 0),
            sqrt_reference(U160::MAX, false, 0),
        ];
        let impact = calculate(&prices, U256::MAX, U256::MAX, U256::MAX).unwrap();
        assert_eq!(impact.market_amount_out, U256::MAX);
        assert_eq!(impact.price_impact_bps, 0);
    }

    #[test]
    fn improvements_remain_signed_and_invalid_prices_are_unavailable() {
        let impact = calculate(
            &[ratio(1, 1, 0)],
            U256::from(100),
            U256::from(100),
            U256::from(110),
        )
        .unwrap();
        assert_eq!(impact.price_impact_bps, -1000);
        for price in [ratio(0, 1, 0), ratio(1, 0, 0)] {
            assert!(
                calculate(&[price], U256::from(100), U256::from(100), U256::from(90)).is_none()
            );
        }
    }

    fn hops() -> Vec<RouteHop> {
        let config = UniswapDeployment::robinhood_mainnet();
        let tokens = [Address::repeat_byte(1), Address::repeat_byte(2)];
        let pools = [
            UniswapPool::V2(V2Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                factory: config.v2.unwrap().factory,
                address: Address::repeat_byte(3),
                token0: tokens[0],
                token1: tokens[1],
            }),
            UniswapPool::V3(V3Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                factory: config.v3.unwrap().factory,
                address: Address::repeat_byte(4),
                token0: tokens[0],
                token1: tokens[1],
                fee: U24::from(500),
                tick_spacing: I24::try_from(10).unwrap(),
            }),
            UniswapPool::V4(V4Pool {
                chain_id: ROBINHOOD_CHAIN_ID,
                manager: config.v4.unwrap().pool_manager,
                key: V4PoolKey {
                    currency0: Address::ZERO,
                    currency1: tokens[1],
                    fee: U24::from(3000),
                    tick_spacing: I24::try_from(60).unwrap(),
                    hooks: Address::ZERO,
                },
            }),
        ];
        pools
            .into_iter()
            .enumerate()
            .map(|(index, pool)| RouteHop {
                pool,
                currency_in: if index == 2 {
                    Currency::Native
                } else {
                    Currency::Erc20(tokens[0])
                },
                currency_out: Currency::Erc20(tokens[1]),
                hook_data: Bytes::new(),
            })
            .collect()
    }

    async fn client(rpc: &Asserter) -> EvmClient<alloy_provider::RootProvider> {
        rpc.push_success(&U64::from(ROBINHOOD_CHAIN_ID));
        EvmClient::connect(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_mocked_client(rpc.clone()),
            ROBINHOOD_CHAIN_ID,
        )
        .await
        .unwrap()
    }

    fn pons_hop() -> RouteHop {
        let mut hop = hops().remove(2);
        let UniswapPool::V4(pool) = &mut hop.pool else {
            unreachable!()
        };
        pool.key.hooks = PONS_V2_HOOK;
        hop
    }

    fn launch_info() -> launchesReturn {
        launchesReturn {
            registered: true,
            memecoinIsCurrency0: false,
            memecoin: Address::repeat_byte(2),
            quoteToken: Address::ZERO,
            creator: Address::repeat_byte(3),
            buybackCreatorRecipient: Address::repeat_byte(3),
            protocolFeeRecipient: Address::repeat_byte(4),
            creatorTaxBps: 300,
            protocolFeeShareBps: 3000,
            buybackBurnBps: 5000,
            hookFeeBps: 100,
            maxInternalPriceImpactBps: 300,
            buybackEnabled: false,
        }
    }

    #[tokio::test]
    async fn pons_impact_separates_saved_hook_tax_and_core_fees_in_both_directions() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        for reverse in [false, true] {
            let mut hop = pons_hop();
            if reverse {
                std::mem::swap(&mut hop.currency_in, &mut hop.currency_out);
            }
            rpc.push_success(&Bytes::from(
                (U160::from(2) << 96, I24::ZERO, U24::ZERO, U24::from(10_000)).abi_encode(),
            ));
            rpc.push_success(&Bytes::from(launchesCall::abi_encode_returns(
                &launch_info(),
            )));
            let price = reference(&client, &hop, B256::repeat_byte(7))
                .await
                .unwrap();
            let input = if reverse { 4_000_000 } else { 250_000 };
            let impact = calculate(
                &[price],
                U256::from(input),
                U256::from(input),
                U256::from(855_360),
            )
            .unwrap();
            assert_eq!(impact.market_amount_out, U256::from(1_000_000));
            assert_eq!(
                impact.market_amount_out_after_fees,
                Some(U256::from(950_400))
            );
            assert_eq!(impact.price_impact_bps, 1446);
            assert_eq!(impact.total_cost_bps, 1446);
            assert_eq!(impact.pool_fee_pips, [Some(10_000)]);
            assert_eq!(
                impact.hook_fees,
                [Some(HookFees {
                    hook_fee_bps: 100,
                    creator_tax_bps: 300
                })]
            );
        }
        assert!(rpc.read_q().is_empty());
    }

    #[tokio::test]
    async fn pons_supports_zero_fees_and_both_memecoin_positions() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        for first in [false, true] {
            let mut hop = pons_hop();
            let UniswapPool::V4(pool) = &mut hop.pool else {
                unreachable!()
            };
            let mut info = launch_info();
            info.hookFeeBps = 0;
            info.creatorTaxBps = 0;
            if first {
                pool.key.currency0 = Address::repeat_byte(1);
                info.memecoinIsCurrency0 = true;
                info.memecoin = pool.key.currency0;
                info.quoteToken = pool.key.currency1;
            }
            rpc.push_success(&Bytes::from(launchesCall::abi_encode_returns(&info)));
            assert_eq!(
                hook_fees(&client, pool, B256::ZERO).await.unwrap(),
                HookFees::default()
            );
        }
    }

    #[tokio::test]
    async fn invalid_or_missing_hook_terms_never_become_zero_impact() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        let hop = pons_hop();
        let UniswapPool::V4(pool) = &hop.pool else {
            unreachable!()
        };
        for index in 0..6 {
            let mut info = launch_info();
            match index {
                0 => info.registered = false,
                1 => info.memecoin = Address::repeat_byte(8),
                2 => info.quoteToken = Address::repeat_byte(8),
                3 => info.memecoinIsCurrency0 = true,
                4 => info.hookFeeBps = 1001,
                _ => info.creatorTaxBps = 2000,
            }
            rpc.push_success(&Bytes::from(launchesCall::abi_encode_returns(&info)));
            assert!(matches!(
                hook_fees(&client, pool, B256::ZERO).await,
                Err(PriceImpactUnavailable::HookConfiguration)
            ));
        }
        rpc.push_failure_msg("hook state unavailable");
        assert!(matches!(
            hook_fees(&client, pool, B256::ZERO).await,
            Err(PriceImpactUnavailable::State(_))
        ));
        rpc.push_success(&Bytes::from_static(b"bad ABI"));
        assert!(matches!(
            hook_fees(&client, pool, B256::ZERO).await,
            Err(PriceImpactUnavailable::State(_))
        ));
    }

    #[test]
    fn hook_models_require_known_swap_behavior_and_deployment() {
        let mut hop = pons_hop();
        let UniswapPool::V4(pool) = &mut hop.pool else {
            unreachable!()
        };
        assert!(matches!(
            v4_model(ROBINHOOD_CHAIN_ID, pool),
            Ok(V4PriceModel::PonsV2)
        ));
        assert!(matches!(
            v4_model(1, pool),
            Err(PriceImpactUnavailable::HookPricing)
        ));
        let manager = pool.manager;
        pool.manager = Address::repeat_byte(9);
        assert!(matches!(
            v4_model(ROBINHOOD_CHAIN_ID, pool),
            Err(PriceImpactUnavailable::HookPricing)
        ));
        pool.manager = manager;
        pool.key.fee = U24::from(0x800000);
        assert!(matches!(
            v4_model(ROBINHOOD_CHAIN_ID, pool),
            Err(PriceImpactUnavailable::HookPricing)
        ));
        pool.key.fee = U24::ZERO;
        for flag in [0, 1, 2, 16, 32] {
            pool.key.hooks = Address::with_last_byte(flag);
            assert!(matches!(
                v4_model(ROBINHOOD_CHAIN_ID, pool),
                Ok(V4PriceModel::Core)
            ));
        }
        for flag in [4, 8, 64, 128] {
            pool.key.hooks = Address::with_last_byte(flag);
            assert!(matches!(
                v4_model(ROBINHOOD_CHAIN_ID, pool),
                Err(PriceImpactUnavailable::HookPricing)
            ));
        }
    }

    #[tokio::test]
    async fn pool_state_prices_follow_direction_and_v4_protocol_fees() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        for mut hop in hops() {
            for reverse in [false, true] {
                let expected_fee = match &hop.pool {
                    UniswapPool::V2(_) => {
                        rpc.push_success(&Bytes::from(
                            (U112::from(100), U112::from(400), 0u32).abi_encode(),
                        ));
                        3000
                    }
                    UniswapPool::V3(_) => {
                        rpc.push_success(&Bytes::from(
                            (U160::from(2) << 96, I24::ZERO, 0u16, 0u16, 0u16, 0u16, true)
                                .abi_encode(),
                        ));
                        500
                    }
                    UniswapPool::V4(_) => {
                        rpc.push_success(&Bytes::from(
                            (
                                U160::from(2) << 96,
                                I24::ZERO,
                                U24::from(1000 | (500 << 12)),
                                U24::from(3000),
                            )
                                .abi_encode(),
                        ));
                        if reverse { 3499 } else { 3997 }
                    }
                };
                if reverse {
                    std::mem::swap(&mut hop.currency_in, &mut hop.currency_out);
                }
                let price = reference(&client, &hop, B256::repeat_byte(7))
                    .await
                    .unwrap();
                assert_eq!(price.fee_pips, Some(expected_fee));
                let mut amount = Fraction::amount(U256::from(100));
                amount.multiply(price.numerator, price.denominator).unwrap();
                assert_eq!(
                    amount.floor().unwrap(),
                    U256::from(if reverse { 25 } else { 400 })
                );
            }
        }
        assert!(rpc.read_q().is_empty());
    }

    #[tokio::test]
    async fn repeated_pools_use_pre_trade_references_and_rpc_failures_stay_errors() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        let hop = hops().remove(0);
        let mut reverse = hop.clone();
        std::mem::swap(&mut reverse.currency_in, &mut reverse.currency_out);
        for _ in 0..2 {
            rpc.push_success(&Bytes::from(
                (U112::from(100), U112::from(200), 0u32).abi_encode(),
            ));
        }
        let impact = estimate(
            &client,
            &[hop.clone(), reverse],
            B256::ZERO,
            U256::from(100),
            U256::from(99),
            U256::from(80),
        )
        .await
        .unwrap();
        assert_eq!(impact.market_amount_out, U256::from(100));
        assert_eq!(impact.price_impact_bps, 2000);
        rpc.push_failure_msg("state unavailable");
        assert!(matches!(
            estimate(
                &client,
                &[hop],
                B256::ZERO,
                U256::from(100),
                U256::from(99),
                U256::from(80)
            )
            .await,
            Err(PriceImpactUnavailable::State(_))
        ));
    }
}
