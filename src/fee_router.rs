use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

use crate::{
    EvmClient,
    dex::{Currency, Error, ExactInput, Result, SwapLimits, SwapTransaction, TokenApproval},
    uniswap::UniswapPool,
};

pub struct FeeRouter<P: Provider> {
    client: EvmClient<P>,
    address: Address,
    fee_recipient: Address,
    wrapped_native: Address,
    v2_factory: Address,
    v3_factory: Address,
    pool_manager: Address,
}

#[derive(Clone, Debug)]
pub struct RouteHop {
    pub pool: UniswapPool,
    pub currency_in: Currency,
    pub currency_out: Currency,
    pub hook_data: Bytes,
}

pub fn input_fee(amount: U256) -> U256 {
    amount / U256::from(100)
}
pub fn amount_after_fee(amount: U256) -> U256 {
    amount - input_fee(amount)
}

impl<P: Provider> FeeRouter<P> {
    pub async fn connect(client: EvmClient<P>, address: Address, block: B256) -> Result<Self> {
        client.require_contract(address, block).await?;
        if client.call(address, abi::FEE_BPSCall {}, block).await? != U256::from(100) {
            return Err(Error::UnsupportedDeployment(address));
        }
        Ok(Self {
            fee_recipient: client
                .call(address, abi::feeRecipientCall {}, block)
                .await?,
            wrapped_native: client
                .call(address, abi::wrappedNativeCall {}, block)
                .await?,
            v2_factory: client.call(address, abi::v2FactoryCall {}, block).await?,
            v3_factory: client.call(address, abi::v3FactoryCall {}, block).await?,
            pool_manager: client.call(address, abi::poolManagerCall {}, block).await?,
            client,
            address,
        })
    }

    pub fn fee_recipient(&self) -> Address {
        self.fee_recipient
    }

    /// Quote the first hop using amount_after_fee. The minimum is final output after all hops.
    /// This encodes a selected route; it does not discover, quote, or simulate that route.
    pub fn build_swap(
        &self,
        trade: &ExactInput,
        hops: &[RouteHop],
        limits: SwapLimits,
    ) -> Result<SwapTransaction> {
        if trade.chain_id != self.client.chain_id() {
            return Err(Error::ChainMismatch {
                expected: self.client.chain_id(),
                actual: trade.chain_id,
            });
        }
        if trade.amount_in.is_zero()
            || hops.is_empty()
            || trade.recipient.is_zero()
            || trade.recipient == self.address
        {
            return Err(Error::InvalidTrade(
                "a fee swap needs input, a route, and an external recipient".into(),
            ));
        }
        let mut currency = trade.currency_in;
        let mut encoded = Vec::with_capacity(hops.len());
        for hop in hops {
            self.check_connection(currency, hop.currency_in)?;
            encoded.push(self.encode_hop(hop)?);
            currency = hop.currency_out;
        }
        self.check_connection(currency, trade.currency_out)?;
        let call = abi::swapCall {
            request: abi::SwapRequest {
                tokenIn: address_of(trade.currency_in)?,
                tokenOut: address_of(trade.currency_out)?,
                amountIn: trade.amount_in,
                minimumAmountOut: limits.minimum_amount_out,
                recipient: trade.recipient,
                deadline: U256::from(limits.deadline_unix_seconds),
            },
            hops: encoded,
        };
        Ok(SwapTransaction {
            transaction: TransactionRequest {
                chain_id: Some(trade.chain_id),
                from: Some(trade.sender),
                to: Some(self.address.into()),
                value: Some(if trade.currency_in == Currency::Native {
                    trade.amount_in
                } else {
                    U256::ZERO
                }),
                input: call.abi_encode().into(),
                ..Default::default()
            },
            approval: match trade.currency_in {
                Currency::Native => None,
                Currency::Erc20(token) => Some(TokenApproval {
                    token,
                    owner: trade.sender,
                    spender: self.address,
                    amount: trade.amount_in,
                }),
            },
        })
    }

    fn encode_hop(&self, hop: &RouteHop) -> Result<abi::Hop> {
        let input = address_of(hop.currency_in)?;
        let output = address_of(hop.currency_out)?;
        let (chain, currencies, mut encoded) = match &hop.pool {
            UniswapPool::V2(pool) => {
                if pool.factory != self.v2_factory {
                    return Err(Error::UnsupportedDeployment(pool.factory));
                }
                (
                    pool.chain_id,
                    [pool.token0, pool.token1],
                    abi::Hop {
                        version: 0,
                        pool: pool.address,
                        ..Default::default()
                    },
                )
            }
            UniswapPool::V3(pool) => {
                if pool.factory != self.v3_factory {
                    return Err(Error::UnsupportedDeployment(pool.factory));
                }
                (
                    pool.chain_id,
                    [pool.token0, pool.token1],
                    abi::Hop {
                        version: 1,
                        pool: pool.address,
                        fee: pool.fee,
                        ..Default::default()
                    },
                )
            }
            UniswapPool::V4(pool) => {
                if pool.manager != self.pool_manager {
                    return Err(Error::UnsupportedDeployment(pool.manager));
                }
                let key = &pool.key;
                (
                    pool.chain_id,
                    [key.currency0, key.currency1],
                    abi::Hop {
                        version: 2,
                        key: abi::PoolKey {
                            currency0: key.currency0,
                            currency1: key.currency1,
                            fee: key.fee,
                            tickSpacing: key.tick_spacing,
                            hooks: key.hooks,
                        },
                        hookData: hop.hook_data.clone(),
                        ..Default::default()
                    },
                )
            }
        };
        let mut pair = [input, output];
        pair.sort();
        if chain != self.client.chain_id() || pair != currencies || input == output {
            return Err(Error::InvalidTrade(
                "route hop does not match its pool and chain".into(),
            ));
        }
        if encoded.version != 2
            && (!hop.hook_data.is_empty() || input.is_zero() || output.is_zero())
        {
            return Err(Error::InvalidTrade(
                "V2/V3 hops require ERC-20 currencies and no hook data".into(),
            ));
        }
        encoded.tokenIn = input;
        encoded.tokenOut = output;
        Ok(encoded)
    }

    fn check_connection(&self, from: Currency, to: Currency) -> Result<()> {
        address_of(from)?;
        address_of(to)?;
        let wrapped = Currency::Erc20(self.wrapped_native);
        if from == to
            || (from == Currency::Native && to == wrapped)
            || (from == wrapped && to == Currency::Native)
        {
            return Ok(());
        }
        Err(Error::InvalidTrade(
            "route currencies are disconnected".into(),
        ))
    }
}

fn address_of(currency: Currency) -> Result<Address> {
    match currency {
        Currency::Native => Ok(Address::ZERO),
        Currency::Erc20(address) if !address.is_zero() => Ok(address),
        _ => Err(Error::InvalidTrade(
            "use Currency::Native instead of ERC-20 zero".into(),
        )),
    }
}

mod abi {
    use super::*;
    sol! {
        #[derive(Default)]
        struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
        #[derive(Default)]
        struct Hop { uint8 version; address tokenIn; address tokenOut; address pool; uint24 fee; PoolKey key; bytes hookData; }
        struct SwapRequest { address tokenIn; address tokenOut; uint256 amountIn; uint256 minimumAmountOut; address recipient; uint256 deadline; }
        function swap(SwapRequest request, Hop[] hops) external payable returns (uint256 amountOut);
        function FEE_BPS() external view returns (uint256);
        function feeRecipient() external view returns (address);
        function wrappedNative() external view returns (address);
        function v2Factory() external view returns (address);
        function v3Factory() external view returns (address);
        function poolManager() external view returns (address);
    }
}
