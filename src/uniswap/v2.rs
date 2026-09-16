use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

use crate::{
    EvmClient,
    dex::{
        Currency, Dex, DiscoverPools, Error, ExactInput, Quote, QuoteRequest, ResolvePool, Result,
        SwapLimits,
    },
};

use super::{
    DiscoveryFailure, DiscoveryOutcome, SearchCoverage, SearchScope, V2Deployment, V2Pool,
};

/// Single-pool swaps through canonical Uniswap V2 Factory and Router02 contracts.
/// Forks with different pair bytecode or router behavior require a compatible adapter.
pub struct UniswapV2<P: Provider> {
    client: EvmClient<P>,
    deployment: V2Deployment,
    wrapped_native: Address,
}

#[derive(Clone, Debug)]
pub struct V2PoolQuery {
    pub chain_id: u64,
    pub currency_a: Currency,
    pub currency_b: Currency,
}

#[derive(Debug)]
pub struct V2PoolSearch {
    pub query: V2PoolQuery,
    pub block_hash: B256,
    pub outcome: DiscoveryOutcome<V2Pool>,
}

pub type V2SwapPlan = crate::dex::SwapTransaction;
pub type V2Approval = crate::dex::TokenApproval;

impl<P: Provider> UniswapV2<P> {
    pub async fn connect(
        client: EvmClient<P>,
        deployment: V2Deployment,
        block_hash: B256,
    ) -> Result<Self> {
        client
            .require_contract(deployment.factory, block_hash)
            .await?;
        let factory = client
            .call(deployment.router_02, abi::factoryCall {}, block_hash)
            .await?;
        if factory != deployment.factory {
            return Err(Error::UnsupportedDeployment(factory));
        }
        let wrapped_native = client
            .call(deployment.router_02, abi::WETHCall {}, block_hash)
            .await?;
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

    fn validate_trade(&self, pool: &V2Pool, trade: &ExactInput) -> Result<[Address; 2]> {
        self.check_chain(trade.chain_id)?;
        self.validate_pool(pool)?;
        if trade.amount_in.is_zero() {
            return Err(Error::InvalidTrade("input amount is zero".into()));
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

    fn validate_pool(&self, pool: &V2Pool) -> Result<()> {
        self.check_chain(pool.chain_id)?;
        if pool.factory != self.deployment.factory {
            return Err(Error::UnsupportedDeployment(pool.factory));
        }
        if pool.token0.is_zero() || pool.token0 >= pool.token1 {
            return Err(Error::InvalidPool(
                "tokens are not a valid ordered pair".into(),
            ));
        }
        if canonical_pair_address(pool.factory, pool.token0, pool.token1) != pool.address {
            return Err(Error::InvalidPool(
                "selected address differs from the router's pair".into(),
            ));
        }
        Ok(())
    }

    async fn pair_address(&self, path: [Address; 2], block_hash: B256) -> Result<Address> {
        self.client
            .call(
                self.deployment.factory,
                abi::getPairCall {
                    tokenA: path[0],
                    tokenB: path[1],
                },
                block_hash,
            )
            .await
    }

    async fn find_pair(&self, path: [Address; 2], block_hash: B256) -> Result<Vec<V2Pool>> {
        let address = self.pair_address(path, block_hash).await?;
        if address.is_zero() {
            return Ok(Vec::new());
        }
        Ok(vec![self.load_pool(address, block_hash).await?])
    }

    fn swap_calldata(&self, trade: &ExactInput, path: [Address; 2], limits: SwapLimits) -> Bytes {
        let path = path.to_vec();
        let minimum_amount_out = limits.minimum_amount_out;
        let to = trade.recipient;
        let deadline = U256::from(limits.deadline_unix_seconds);
        let amount_in = trade.amount_in;
        match (trade.currency_in, trade.currency_out) {
            (Currency::Native, _) => abi::swapExactETHForTokensCall {
                amountOutMin: minimum_amount_out,
                path,
                to,
                deadline,
            }
            .abi_encode()
            .into(),
            (_, Currency::Native) => abi::swapExactTokensForETHCall {
                amountIn: amount_in,
                amountOutMin: minimum_amount_out,
                path,
                to,
                deadline,
            }
            .abi_encode()
            .into(),
            _ => abi::swapExactTokensForTokensCall {
                amountIn: amount_in,
                amountOutMin: minimum_amount_out,
                path,
                to,
                deadline,
            }
            .abi_encode()
            .into(),
        }
    }
}

impl<P: Provider> Dex for UniswapV2<P> {
    type PoolId = Address;
    type Pool = V2Pool;
    type QuoteOptions = ();
    type SwapPlan = V2SwapPlan;

    async fn load_pool(&self, address: Address, block_hash: B256) -> Result<V2Pool> {
        if address.is_zero() {
            return Err(Error::PoolNotFound);
        }
        let factory = self
            .client
            .call(address, abi::factoryCall {}, block_hash)
            .await?;
        if factory != self.deployment.factory {
            return Err(Error::UnsupportedDeployment(factory));
        }
        let token0 = self
            .client
            .call(address, abi::token0Call {}, block_hash)
            .await?;
        let token1 = self
            .client
            .call(address, abi::token1Call {}, block_hash)
            .await?;
        if token0.is_zero() || token0 >= token1 {
            return Err(Error::InvalidPool(
                "tokens are not a valid ordered pair".into(),
            ));
        }
        if self.pair_address([token0, token1], block_hash).await? != address {
            return Err(Error::InvalidPool(
                "factory registry points to a different pair".into(),
            ));
        }
        let pool = V2Pool {
            chain_id: self.client.chain_id(),
            factory,
            address,
            token0,
            token1,
        };
        self.validate_pool(&pool)?;
        Ok(pool)
    }

    /// Uses the configured Router02 quote for standard tokens; transfer fees are not modeled.
    async fn quote(&self, request: QuoteRequest<V2Pool>) -> Result<Quote<V2Pool>> {
        let path = self.validate_trade(&request.pool, &request.trade)?;
        let loaded = self
            .load_pool(request.pool.address, request.block_hash)
            .await?;
        if loaded != request.pool {
            return Err(Error::InvalidPool(
                "provided identity differs from the selected pool".into(),
            ));
        }
        let amounts = self
            .client
            .call(
                self.deployment.router_02,
                abi::getAmountsOutCall {
                    amountIn: request.trade.amount_in,
                    path: path.to_vec(),
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
        if amounts.len() != 2 || amounts[0] != request.trade.amount_in {
            return Err(Error::QuoteFailed {
                reason: "router returned an invalid direct-pool quote".into(),
                revert_data: Bytes::new(),
            });
        }
        if amounts[1].is_zero() {
            return Err(Error::QuoteFailed {
                reason: "input amount produces no output".into(),
                revert_data: Bytes::new(),
            });
        }
        Ok(Quote {
            request,
            amount_out: amounts[1],
        })
    }

    fn build_swap(&self, quote: &Quote<V2Pool>, limits: SwapLimits) -> Result<V2SwapPlan> {
        let request = &quote.request;
        let trade = &request.trade;
        let path = self.validate_trade(&request.pool, trade)?;
        let approval = match trade.currency_in {
            Currency::Native => None,
            Currency::Erc20(token) => Some(V2Approval {
                token,
                owner: trade.sender,
                spender: self.deployment.router_02,
                amount: trade.amount_in,
            }),
        };
        Ok(V2SwapPlan {
            transaction: TransactionRequest {
                chain_id: Some(self.client.chain_id()),
                from: Some(trade.sender),
                to: Some(self.deployment.router_02.into()),
                value: Some(if approval.is_none() {
                    trade.amount_in
                } else {
                    U256::ZERO
                }),
                input: self.swap_calldata(trade, path, limits).into(),
                ..Default::default()
            },
            approval,
        })
    }
}

impl<P: Provider> DiscoverPools for UniswapV2<P> {
    type Query = V2PoolQuery;
    type Search = V2PoolSearch;

    async fn find_pools(&self, query: V2PoolQuery, block_hash: B256) -> Result<V2PoolSearch> {
        self.check_chain(query.chain_id)?;
        let path = self.token_path(query.currency_a, query.currency_b)?;
        let mut coverage = SearchCoverage {
            requested: vec![SearchScope::V2Pair],
            completed: Vec::new(),
        };
        let (pools, failures) = match self.find_pair(path, block_hash).await {
            Ok(pools) => {
                coverage.completed.push(SearchScope::V2Pair);
                (pools, Vec::new())
            }
            Err(error) => (
                Vec::new(),
                vec![DiscoveryFailure {
                    scope: SearchScope::V2Pair,
                    error,
                }],
            ),
        };
        Ok(V2PoolSearch {
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

impl<P: Provider> ResolvePool for UniswapV2<P> {
    type Input = Address;

    async fn resolve_pool(&self, address: Address, block_hash: B256) -> Result<V2Pool> {
        self.load_pool(address, block_hash).await
    }
}

// Router02 derives its destination from this hash; verify it to preserve the selected pool.
fn canonical_pair_address(factory: Address, token0: Address, token1: Address) -> Address {
    let salt = keccak256([token0.as_slice(), token1.as_slice()].concat());
    factory.create2(
        salt,
        b256!("96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f"),
    )
}

sol! {
    interface abi {
        function factory() external view returns (address);
        function WETH() external view returns (address);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getPair(address tokenA, address tokenB) external view returns (address);
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[]);
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[]);
        function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable returns (uint256[]);
        function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external returns (uint256[]);
    }
}
