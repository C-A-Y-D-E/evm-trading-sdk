use alloy_primitives::{
    Address, B256, Bytes, U256, address,
    aliases::{I24, U24, U160},
    b256, keccak256,
};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, SolValue, sol};
use futures_util::{future::join_all, try_join};

use crate::{
    EvmClient,
    dex::{
        Currency, Dex, DiscoverPools, Error, ExactInput, Quote, QuoteRequest, ResolvePool, Result,
        SwapLimits, SwapTransaction, TokenApproval,
    },
};

use super::{
    DiscoveryFailure, DiscoveryOutcome, SearchCoverage, SearchScope, V3Deployment, V3Pool,
};

const ROUTER_SENDER: Address = address!("0000000000000000000000000000000000000001");
const ROUTER_SELF: Address = address!("0000000000000000000000000000000000000002");

/// Direct swaps through canonical Uniswap V3 pools, SwapRouter02, and QuoterV2.
/// Forks with different pool bytecode or router behavior need a compatible adapter.
pub struct UniswapV3<P: Provider> {
    client: EvmClient<P>,
    deployment: V3Deployment,
    wrapped_native: Address,
}

#[derive(Clone, Debug)]
pub struct V3PoolQuery {
    pub chain_id: u64,
    pub currency_a: Currency,
    pub currency_b: Currency,
    pub fee_tiers: Vec<U24>,
}

#[derive(Debug)]
pub struct V3PoolSearch {
    pub query: V3PoolQuery,
    pub block_hash: B256,
    pub outcome: DiscoveryOutcome<V3Pool>,
}

impl<P: Provider> UniswapV3<P> {
    pub async fn connect(
        client: EvmClient<P>,
        deployment: V3Deployment,
        block_hash: B256,
    ) -> Result<Self> {
        let (_, router_factory, wrapped_native, quoter_factory, quoter_wrapped_native) = try_join!(
            client.require_contract(deployment.factory, block_hash),
            client.call(deployment.swap_router_02, abi::factoryCall {}, block_hash),
            client.call(deployment.swap_router_02, abi::WETH9Call {}, block_hash),
            client.call(deployment.quoter_v2, abi::factoryCall {}, block_hash),
            client.call(deployment.quoter_v2, abi::WETH9Call {}, block_hash),
        )?;
        if router_factory != deployment.factory {
            return Err(Error::UnsupportedDeployment(router_factory));
        }
        if quoter_factory != deployment.factory {
            return Err(Error::UnsupportedDeployment(quoter_factory));
        }
        if quoter_wrapped_native != wrapped_native {
            return Err(Error::UnsupportedDeployment(deployment.quoter_v2));
        }
        client.require_contract(wrapped_native, block_hash).await?;
        Ok(Self {
            client,
            deployment,
            wrapped_native,
        })
    }

    pub fn wrapped_native(&self) -> Address {
        self.wrapped_native
    }
}

impl<P: Provider> Dex for UniswapV3<P> {
    type PoolId = Address;
    type Pool = V3Pool;
    type QuoteOptions = ();
    type SwapPlan = SwapTransaction;

    async fn load_pool(&self, address: Address, block_hash: B256) -> Result<V3Pool> {
        if address.is_zero() {
            return Err(Error::PoolNotFound);
        }
        let (factory, token0, token1, fee, tick_spacing) = try_join!(
            self.client.call(address, abi::factoryCall {}, block_hash),
            self.client.call(address, abi::token0Call {}, block_hash),
            self.client.call(address, abi::token1Call {}, block_hash),
            self.client.call(address, abi::feeCall {}, block_hash),
            self.client
                .call(address, abi::tickSpacingCall {}, block_hash),
        )?;
        if factory != self.deployment.factory {
            return Err(Error::UnsupportedDeployment(factory));
        }
        let pool = V3Pool {
            chain_id: self.client.chain_id(),
            factory,
            address,
            token0,
            token1,
            fee,
            tick_spacing,
        };
        self.validate_pool(&pool)?;
        if self.pool_address([token0, token1], fee, block_hash).await? != address {
            return Err(Error::InvalidPool(
                "factory registry points to a different pool".into(),
            ));
        }
        Ok(pool)
    }

    /// Transfer fees are unsupported. A price boundary can leave some input unspent;
    /// QuoterV2 reports output rather than the amount of input actually consumed.
    async fn quote(&self, request: QuoteRequest<V3Pool>) -> Result<Quote<V3Pool>> {
        let path = self.validate_trade(&request.pool, &request.trade)?;
        let loaded = self
            .load_pool(request.pool.address, request.block_hash)
            .await?;
        if loaded != request.pool {
            return Err(Error::InvalidPool(
                "provided identity differs from the selected pool".into(),
            ));
        }
        let result = self
            .client
            .call(
                self.deployment.quoter_v2,
                abi::quoteExactInputSingleCall {
                    params: abi::QuoteExactInputSingleParams {
                        tokenIn: path[0],
                        tokenOut: path[1],
                        amountIn: request.trade.amount_in,
                        fee: request.pool.fee,
                        sqrtPriceLimitX96: U160::ZERO,
                    },
                },
                request.block_hash,
            )
            .await
            .map_err(|error| match error {
                Error::ContractReverted {
                    reason,
                    revert_data,
                    ..
                } => Error::QuoteFailed {
                    reason,
                    revert_data,
                },
                error => error,
            })?;
        if result.amountOut.is_zero() {
            return Err(Error::QuoteFailed {
                reason: "input amount produces no output".into(),
                revert_data: Bytes::new(),
            });
        }
        Ok(Quote {
            request,
            amount_out: result.amountOut,
        })
    }

    fn build_swap(&self, quote: &Quote<V3Pool>, limits: SwapLimits) -> Result<SwapTransaction> {
        let request = &quote.request;
        let trade = &request.trade;
        let path = self.validate_trade(&request.pool, trade)?;
        let approval = match trade.currency_in {
            Currency::Native => None,
            Currency::Erc20(token) => Some(TokenApproval {
                token,
                owner: trade.sender,
                spender: self.deployment.swap_router_02,
                amount: trade.amount_in,
            }),
        };
        Ok(SwapTransaction {
            transaction: TransactionRequest {
                chain_id: Some(self.client.chain_id()),
                from: Some(trade.sender),
                to: Some(self.deployment.swap_router_02.into()),
                value: Some(if approval.is_none() {
                    trade.amount_in
                } else {
                    U256::ZERO
                }),
                input: self.swap_calldata(quote, path, limits)?.into(),
                ..Default::default()
            },
            approval,
        })
    }
}

impl<P: Provider> DiscoverPools for UniswapV3<P> {
    type Query = V3PoolQuery;
    type Search = V3PoolSearch;

    async fn find_pools(&self, query: V3PoolQuery, block_hash: B256) -> Result<V3PoolSearch> {
        self.check_chain(query.chain_id)?;
        let path = self.token_path(query.currency_a, query.currency_b)?;
        let mut coverage = SearchCoverage {
            requested: query
                .fee_tiers
                .iter()
                .copied()
                .map(SearchScope::V3FeeTier)
                .collect(),
            completed: Vec::new(),
        };
        let mut pools = Vec::new();
        let mut failures = Vec::new();
        let results = join_all(
            query
                .fee_tiers
                .iter()
                .map(|fee| self.find_pool(path, *fee, block_hash)),
        )
        .await;
        for (fee, result) in query.fee_tiers.iter().zip(results) {
            let scope = SearchScope::V3FeeTier(*fee);
            match result {
                Ok(pool) => {
                    pools.extend(pool);
                    coverage.completed.push(scope);
                }
                Err(error) => failures.push(DiscoveryFailure { scope, error }),
            }
        }
        Ok(V3PoolSearch {
            query,
            block_hash,
            outcome: DiscoveryOutcome::Searched {
                pools,
                coverage,
                failures,
            },
        })
    }
}

impl<P: Provider> ResolvePool for UniswapV3<P> {
    type Input = Address;

    async fn resolve_pool(&self, address: Address, block_hash: B256) -> Result<V3Pool> {
        self.load_pool(address, block_hash).await
    }
}

impl<P: Provider> UniswapV3<P> {
    fn check_chain(&self, chain_id: u64) -> Result<()> {
        if chain_id != self.client.chain_id() {
            return Err(Error::ChainMismatch {
                expected: chain_id,
                actual: self.client.chain_id(),
            });
        }
        Ok(())
    }

    fn token_address(&self, currency: Currency) -> Result<Address> {
        match currency {
            Currency::Native => Ok(self.wrapped_native),
            Currency::Erc20(token) if !token.is_zero() => Ok(token),
            Currency::Erc20(_) => Err(Error::InvalidTrade("ERC-20 address is zero".into())),
        }
    }

    fn token_path(&self, currency_in: Currency, currency_out: Currency) -> Result<[Address; 2]> {
        let token_in = self.token_address(currency_in)?;
        let token_out = self.token_address(currency_out)?;
        if token_in == token_out {
            return Err(Error::InvalidTrade(
                "input and output use the same underlying token".into(),
            ));
        }
        Ok([token_in, token_out])
    }

    fn validate_trade(&self, pool: &V3Pool, trade: &ExactInput) -> Result<[Address; 2]> {
        self.check_chain(trade.chain_id)?;
        self.validate_pool(pool)?;
        if trade.amount_in.is_zero() || trade.amount_in > (U256::MAX >> 1) {
            return Err(Error::InvalidTrade(
                "input amount must fit a positive int256".into(),
            ));
        }
        let path = self.token_path(trade.currency_in, trade.currency_out)?;
        if !((pool.token0 == path[0] && pool.token1 == path[1])
            || (pool.token0 == path[1] && pool.token1 == path[0]))
        {
            return Err(Error::InvalidTrade(
                "currencies do not match the selected pool".into(),
            ));
        }
        Ok(path)
    }

    fn validate_pool(&self, pool: &V3Pool) -> Result<()> {
        self.check_chain(pool.chain_id)?;
        if pool.factory != self.deployment.factory {
            return Err(Error::UnsupportedDeployment(pool.factory));
        }
        if pool.token0.is_zero() || pool.token0 >= pool.token1 || pool.tick_spacing <= I24::ZERO {
            return Err(Error::InvalidPool(
                "tokens or tick spacing are invalid".into(),
            ));
        }
        if canonical_pool_address(pool) != pool.address {
            return Err(Error::InvalidPool(
                "selected address differs from the router's pool".into(),
            ));
        }
        Ok(())
    }

    async fn pool_address(
        &self,
        path: [Address; 2],
        fee: U24,
        block_hash: B256,
    ) -> Result<Address> {
        self.client
            .call(
                self.deployment.factory,
                abi::getPoolCall {
                    tokenA: path[0],
                    tokenB: path[1],
                    fee,
                },
                block_hash,
            )
            .await
    }

    async fn find_pool(
        &self,
        path: [Address; 2],
        fee: U24,
        block_hash: B256,
    ) -> Result<Option<V3Pool>> {
        let address = self.pool_address(path, fee, block_hash).await?;
        if address.is_zero() {
            return Ok(None);
        }
        let pool = self.load_pool(address, block_hash).await?;
        if pool.fee != fee
            || [pool.token0, pool.token1] != [path[0].min(path[1]), path[0].max(path[1])]
        {
            return Err(Error::InvalidPool(
                "pool does not match the requested pair and fee".into(),
            ));
        }
        Ok(Some(pool))
    }

    fn swap_calldata(
        &self,
        quote: &Quote<V3Pool>,
        path: [Address; 2],
        limits: SwapLimits,
    ) -> Result<Bytes> {
        let request = &quote.request;
        let minimum_amount_out = limits.minimum_amount_out(quote.amount_out)?;
        let trade = &request.trade;
        let native_output = trade.currency_out == Currency::Native;
        let aliased_recipient = matches!(trade.recipient, ROUTER_SENDER | ROUTER_SELF);
        let recipient = if native_output || aliased_recipient {
            self.deployment.swap_router_02
        } else {
            trade.recipient
        };
        let swap = abi::exactInputSingleCall {
            params: abi::ExactInputSingleParams {
                tokenIn: path[0],
                tokenOut: path[1],
                fee: request.pool.fee,
                recipient,
                amountIn: trade.amount_in,
                amountOutMinimum: minimum_amount_out,
                sqrtPriceLimitX96: U160::ZERO,
            },
        };
        let mut data = vec![swap.abi_encode().into()];
        if native_output {
            data.push(
                abi::unwrapWETH9Call {
                    amountMinimum: minimum_amount_out,
                    recipient: trade.recipient,
                }
                .abi_encode()
                .into(),
            );
        } else if aliased_recipient {
            // The swap aliases addresses 1 and 2; sweeping preserves the literal recipient.
            data.push(
                abi::sweepTokenCall {
                    token: path[1],
                    amountMinimum: minimum_amount_out,
                    recipient: trade.recipient,
                }
                .abi_encode()
                .into(),
            );
        }
        if trade.currency_in == Currency::Native {
            data.push(abi::refundETHCall {}.abi_encode().into());
        }
        // SwapRouter02 has no deadline in its swap tuple; the multicall enforces it.
        Ok(abi::multicallCall {
            deadline: U256::from(limits.deadline_unix_seconds),
            data,
        }
        .abi_encode()
        .into())
    }
}

// SwapRouter02 derives its destination from this hash and the full encoded pool key.
fn canonical_pool_address(pool: &V3Pool) -> Address {
    let salt = keccak256((pool.token0, pool.token1, pool.fee).abi_encode());
    pool.factory.create2(
        salt,
        b256!("e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54"),
    )
}

sol! {
    interface abi {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function factory() external view returns (address);
        function WETH9() external view returns (address);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
        function quoteExactInputSingle(QuoteExactInputSingleParams params) external returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[]);
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function sweepToken(address token, uint256 amountMinimum, address recipient) external payable;
        function refundETH() external payable;
    }
}
