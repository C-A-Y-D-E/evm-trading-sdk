use alloy_primitives::{Address, B256, U256, Uint, aliases::U160};
use alloy_provider::Provider;
use alloy_sol_types::sol;
use futures_util::future::join_all;

use super::RouteHop;
use crate::{
    Error, EvmClient,
    dex::Currency,
    uniswap::{UniswapDeployment, UniswapPool},
};

type Wide = Uint<1024, 16>;

/// Same-block route estimate. Amounts are output base units, rounded down only at the end.
/// Basis points truncate toward zero; negative impact means a better-than-reference quote.
/// `market_amount_out` uses gross input and the product of pool spot prices, excluding fees.
/// `market_amount_out_after_fees` uses net input and each pool's fee retention factor.
/// `price_impact_bps` compares quoted output to the latter; `total_cost_bps` compares to the former.
/// Pool fees are in millionths (100 pips = 1 bp). Gas and external market prices are excluded.
/// Pool rounding and price boundaries can also affect the quote; this is an estimate, not a fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceImpact {
    pub market_amount_out: U256,
    pub market_amount_out_after_fees: U256,
    pub price_impact_bps: i32,
    pub total_cost_bps: i32,
    pub pool_fee_pips: Vec<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum PriceImpactUnavailable {
    #[error("custom V4 hooks or dynamic fees need a compatible pricing model")]
    HookPricing,
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
    for (index, hop) in hops.iter().enumerate() {
        if matches!(&hop.pool, UniswapPool::V4(pool) if !pool.key.hooks.is_zero() || pool.key.fee.to::<u32>() == 0x800000)
        {
            return Err(PriceImpactUnavailable::HookPricing);
        }
        if hops[..index].iter().any(|earlier| earlier.pool == hop.pool) {
            return Err(PriceImpactUnavailable::RepeatedPool);
        }
    }
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
    fee_pips: u32,
}

async fn reference<P: Provider>(
    client: &EvmClient<P>,
    hop: &RouteHop,
    block: B256,
) -> Result<Reference, PriceImpactUnavailable> {
    let state_error = |error| PriceImpactUnavailable::State(Box::new(error));
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
                fee_pips: 3000,
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
            let state = client
                .call(
                    config.state_view,
                    getSlot0Call {
                        poolId: pool.key.id(),
                    },
                    block,
                )
                .await
                .map_err(state_error)?;
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
            if protocol > 1000 || lp > 1_000_000 {
                return Err(PriceImpactUnavailable::Arithmetic);
            }
            let fee = protocol + lp - (u64::from(protocol) * u64::from(lp) / 1_000_000) as u32;
            sqrt_reference(state.sqrtPriceX96, zero_for_one, fee)
        }
    })
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
        fee_pips,
    }
}

fn calculate(
    references: &[Reference],
    gross: U256,
    net: U256,
    output: U256,
) -> Option<PriceImpact> {
    let mut market = Fraction::amount(gross);
    let mut after_fees = Fraction::amount(net);
    for reference in references {
        market.multiply(reference.numerator, reference.denominator)?;
        after_fees.multiply(reference.numerator, reference.denominator)?;
        after_fees.multiply(
            Wide::from(1_000_000u32.checked_sub(reference.fee_pips)?),
            Wide::from(1_000_000),
        )?;
    }
    Some(PriceImpact {
        market_amount_out: market.floor()?,
        market_amount_out_after_fees: after_fees.floor()?,
        price_impact_bps: after_fees.difference_bps(output)?,
        total_cost_bps: market.difference_bps(output)?,
        pool_fee_pips: references
            .iter()
            .map(|reference| reference.fee_pips)
            .collect(),
    })
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
    use alloy_sol_types::SolValue;

    fn ratio(numerator: u64, denominator: u64, fee_pips: u32) -> Reference {
        Reference {
            numerator: Wide::from(numerator),
            denominator: Wide::from(denominator),
            fee_pips,
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
        assert_eq!(impact.market_amount_out_after_fees, U256::from(57629));
        assert_eq!(impact.price_impact_bps, 0);
        assert_eq!(impact.total_cost_bps, 395);
        assert_eq!(impact.pool_fee_pips, [10000, 20000]);
        let impact = calculate(
            &[ratio(1, 1, 0), ratio(1, 1, 0)],
            U256::from(10000),
            U256::from(10000),
            U256::from(8100),
        )
        .unwrap();
        assert_eq!(impact.price_impact_bps, 1900);
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
        for price in [ratio(0, 1, 0), ratio(1, 0, 0), ratio(1, 1, 1_000_000)] {
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
                assert_eq!(price.fee_pips, expected_fee);
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
    async fn hooks_repeated_pools_and_rpc_failures_have_explicit_reasons() {
        let rpc = Asserter::new();
        let client = client(&rpc).await;
        let mut hop = hops().remove(2);
        let UniswapPool::V4(pool) = &mut hop.pool else {
            unreachable!()
        };
        pool.key.hooks = Address::repeat_byte(9);
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
            Err(PriceImpactUnavailable::HookPricing)
        ));
        let hop = hops().remove(0);
        assert!(matches!(
            estimate(
                &client,
                &[hop.clone(), hop.clone()],
                B256::ZERO,
                U256::from(100),
                U256::from(99),
                U256::from(80)
            )
            .await,
            Err(PriceImpactUnavailable::RepeatedPool)
        ));
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
