use std::ops::RangeInclusive;

use alloy_primitives::{
    Address, B256, Bytes, U256,
    aliases::{I24, U48, U160},
    keccak256,
};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockId, Filter, Log, TransactionRequest};
use alloy_sol_types::{SolCall, SolEvent, SolValue, sol};

use super::{
    DiscoveryFailure, DiscoveryOutcome, SearchCoverage, SearchScope, V4Deployment, V4Pool,
    V4PoolKey,
};
use crate::{
    EvmClient, contract_call_error,
    dex::{
        Currency, Dex, DiscoverPools, Error, ExactInput, Quote, QuoteRequest, ResolvePool, Result,
        SwapLimits, TokenApproval,
    },
};

const V4_SWAP: u8 = 0x10;
const SWEEP: u8 = 0x04;
const SWAP_EXACT_IN_SINGLE: u8 = 0x06;
const SETTLE_ALL: u8 = 0x0c;
const TAKE: u8 = 0x0e;

/// Targets the Universal Router V4 ABI with minHopPriceX36, verified on Robinhood.
pub struct UniswapV4<P: Provider> {
    client: EvmClient<P>,
    deployment: V4Deployment,
}

#[derive(Clone, Debug, Default)]
pub struct V4QuoteOptions {
    pub hook_data: Bytes,
}

#[derive(Clone, Debug)]
pub enum V4PoolLookup {
    Key(V4PoolKey),
    Id {
        id: B256,
        ranges: Vec<RangeInclusive<u64>>,
    },
}

#[derive(Clone, Debug)]
pub struct V4PoolQuery {
    pub chain_id: u64,
    pub currency_a: Currency,
    pub currency_b: Currency,
    pub ranges: Vec<RangeInclusive<u64>>,
}

#[derive(Debug)]
pub struct V4PoolSearch {
    pub query: V4PoolQuery,
    pub block_hash: B256,
    pub outcome: DiscoveryOutcome<V4Pool>,
}

#[derive(Clone, Debug)]
pub struct Permit2Approval {
    pub permit2: Address,
    pub token: Address,
    pub owner: Address,
    pub spender: Address,
    pub amount: U160,
    pub expiration: U48,
}

#[derive(Clone, Debug)]
pub struct V4SwapPlan {
    pub transaction: TransactionRequest,
    pub token_approval: Option<TokenApproval>,
    pub permit2_approval: Option<Permit2Approval>,
}

impl V4PoolKey {
    pub fn id(&self) -> B256 {
        keccak256(abi::PoolKey::from(self).abi_encode())
    }
}

impl<P: Provider> UniswapV4<P> {
    pub async fn connect(
        client: EvmClient<P>,
        deployment: V4Deployment,
        block: B256,
    ) -> Result<Self> {
        for address in [deployment.pool_manager, deployment.permit2] {
            client.require_contract(address, block).await?;
        }
        for address in [
            deployment.universal_router,
            deployment.quoter,
            deployment.state_view,
        ] {
            let manager = client.call(address, abi::poolManagerCall {}, block).await?;
            if manager != deployment.pool_manager {
                return Err(Error::UnsupportedDeployment(address));
            }
        }
        Ok(Self { client, deployment })
    }

    pub async fn permit2_allowance(
        &self,
        token: Address,
        owner: Address,
        block: B256,
    ) -> Result<(U160, U48, U48)> {
        let allowance = self
            .client
            .call(
                self.deployment.permit2,
                abi::allowanceCall {
                    owner,
                    token,
                    spender: self.deployment.universal_router,
                },
                block,
            )
            .await?;
        Ok((allowance.amount, allowance.expiration, allowance.nonce))
    }

    async fn search(
        &self,
        filter: Filter,
        ranges: &[RangeInclusive<u64>],
        block: B256,
    ) -> Result<DiscoveryOutcome<V4Pool>> {
        let header = self
            .client
            .provider()
            .get_block_by_hash(block)
            .await
            .map_err(|error| Error::Rpc(Box::new(error)))?
            .ok_or_else(|| Error::UnresolvedPool("pinned block is unavailable".into()))?
            .header;
        if header.hash != block {
            return Err(Error::UnresolvedPool(
                "RPC returned a different block".into(),
            ));
        }
        let mut coverage = SearchCoverage {
            requested: Vec::new(),
            completed: Vec::new(),
        };
        let mut pools = Vec::new();
        let mut failures = Vec::new();
        for range in ranges {
            let scope = SearchScope::V4Blocks {
                from: *range.start(),
                to: *range.end(),
            };
            coverage.requested.push(scope.clone());
            if range.is_empty() || *range.end() > header.number {
                failures.push(DiscoveryFailure {
                    scope,
                    error: Error::UnresolvedPool(
                        "event range is reversed or exceeds the pinned block".into(),
                    ),
                });
                continue;
            }
            let page = self
                .client
                .provider()
                .get_logs(
                    &filter
                        .clone()
                        .from_block(*range.start())
                        .to_block(*range.end()),
                )
                .await;
            match page {
                Err(error) => failures.push(DiscoveryFailure {
                    scope,
                    error: Error::Rpc(Box::new(error)),
                }),
                Ok(logs) => {
                    let mut complete = true;
                    for log in logs {
                        if !filter.matches_topics(log.topics()) {
                            complete = false;
                            failures.push(DiscoveryFailure {
                                scope: scope.clone(),
                                error: Error::UnresolvedPool(
                                    "RPC returned a log outside the requested topics".into(),
                                ),
                            });
                            continue;
                        }
                        match self.pool_from_log(log, range, block).await {
                            Ok(pool) if !pools.contains(&pool) => pools.push(pool),
                            Ok(_) => (),
                            Err(error) => {
                                complete = false;
                                failures.push(DiscoveryFailure {
                                    scope: scope.clone(),
                                    error,
                                });
                            }
                        }
                    }
                    if complete {
                        coverage.completed.push(scope);
                    }
                }
            }
        }
        // Range log queries cannot carry EIP-1898; discard the report if its anchor was reorganized.
        self.client
            .require_contract(self.deployment.pool_manager, block)
            .await?;
        Ok(DiscoveryOutcome::Searched {
            pools,
            coverage,
            failures,
        })
    }

    async fn pool_from_log(
        &self,
        log: Log,
        range: &RangeInclusive<u64>,
        block: B256,
    ) -> Result<V4Pool> {
        if log.address() != self.deployment.pool_manager
            || log.removed
            || !log
                .block_number
                .is_some_and(|number| range.contains(&number))
        {
            return Err(Error::UnresolvedPool(
                "initialization log has invalid chain context".into(),
            ));
        }
        let event = log
            .log_decode_validate::<abi::Initialize>()
            .map_err(|error| Error::UnresolvedPool(format!("invalid initialization log: {error}")))?
            .inner
            .data;
        let key = V4PoolKey {
            currency0: event.currency0,
            currency1: event.currency1,
            fee: event.fee,
            tick_spacing: event.tickSpacing,
            hooks: event.hooks,
        };
        if key.id() != event.id {
            return Err(Error::InvalidPool(
                "initialization key does not match its ID".into(),
            ));
        }
        self.load_pool(key, block).await
    }

    fn filter(&self) -> Filter {
        Filter::new()
            .address(self.deployment.pool_manager)
            .event_signature(abi::Initialize::SIGNATURE_HASH)
    }

    fn validate_trade(
        &self,
        pool: &V4Pool,
        trade: &ExactInput,
    ) -> Result<(Address, Address, bool)> {
        if trade.chain_id != self.client.chain_id() || pool.chain_id != self.client.chain_id() {
            return Err(Error::InvalidTrade(
                "pool and trade must use the connected chain".into(),
            ));
        }
        if pool.manager != self.deployment.pool_manager {
            return Err(Error::UnsupportedDeployment(pool.manager));
        }
        validate_key(&pool.key)?;
        let input = currency_address(trade.currency_in)?;
        let output = currency_address(trade.currency_out)?;
        let forward = input == pool.key.currency0 && output == pool.key.currency1;
        if !(forward || input == pool.key.currency1 && output == pool.key.currency0) {
            return Err(Error::InvalidTrade(
                "currencies do not match the selected V4 pool".into(),
            ));
        }
        if trade.amount_in.is_zero() || trade.amount_in > U256::from(u128::MAX) {
            return Err(Error::InvalidTrade(
                "V4 input must fit a positive uint128".into(),
            ));
        }
        Ok((input, output, forward))
    }
}

impl<P: Provider> Dex for UniswapV4<P> {
    type PoolId = V4PoolKey;
    type Pool = V4Pool;
    type QuoteOptions = V4QuoteOptions;
    type SwapPlan = V4SwapPlan;

    async fn load_pool(&self, key: V4PoolKey, block: B256) -> Result<V4Pool> {
        validate_key(&key)?;
        let state = self
            .client
            .call(
                self.deployment.state_view,
                abi::getSlot0Call { poolId: key.id() },
                block,
            )
            .await?;
        if state.sqrtPriceX96.is_zero() {
            return Err(Error::PoolNotFound);
        }
        Ok(V4Pool {
            chain_id: self.client.chain_id(),
            manager: self.deployment.pool_manager,
            key,
        })
    }

    /// Hooks see the quoter during quotes and the router during execution; simulate the final transaction too.
    async fn quote(
        &self,
        request: QuoteRequest<V4Pool, V4QuoteOptions>,
    ) -> Result<Quote<V4Pool, V4QuoteOptions>> {
        let (_, _, zero_for_one) = self.validate_trade(&request.pool, &request.trade)?;
        self.load_pool(request.pool.key.clone(), request.block_hash)
            .await?;
        let call = abi::quoteExactInputSingleCall {
            params: abi::QuoteParams {
                poolKey: (&request.pool.key).into(),
                zeroForOne: zero_for_one,
                exactAmount: request.trade.amount_in.to::<u128>(),
                hookData: request.options.hook_data.clone(),
            },
        };
        let transaction = TransactionRequest {
            from: Some(request.trade.sender),
            to: Some(self.deployment.quoter.into()),
            input: call.abi_encode().into(),
            ..Default::default()
        };
        let data = self
            .client
            .provider()
            .call(transaction)
            .block(BlockId::hash_canonical(request.block_hash))
            .await
            .map_err(|error| {
                contract_call_error(
                    self.deployment.quoter,
                    abi::quoteExactInputSingleCall::SIGNATURE,
                    error,
                )
            })
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
        let result = abi::quoteExactInputSingleCall::abi_decode_returns_validate(&data).map_err(
            |source| Error::InvalidResponse {
                address: self.deployment.quoter,
                method: abi::quoteExactInputSingleCall::SIGNATURE,
                source,
            },
        )?;
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

    fn build_swap(
        &self,
        quote: &Quote<V4Pool, V4QuoteOptions>,
        limits: SwapLimits,
    ) -> Result<V4SwapPlan> {
        let request = &quote.request;
        let trade = &request.trade;
        let (input, output, zero_for_one) = self.validate_trade(&request.pool, trade)?;
        if limits.minimum_amount_out > U256::from(u128::MAX)
            || limits.deadline_unix_seconds > U48::MAX.to::<u64>()
        {
            return Err(Error::InvalidTrade(
                "minimum output or Permit2 expiration exceeds its ABI width".into(),
            ));
        }
        // Universal Router reserves these literal recipients; never reinterpret the caller's destination.
        if [Address::with_last_byte(1), Address::with_last_byte(2)].contains(&trade.recipient) {
            return Err(Error::InvalidTrade(
                "Universal Router reserves recipient addresses 1 and 2".into(),
            ));
        }
        let actions = Bytes::from(vec![SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE]);
        let params: Vec<Bytes> = vec![
            abi::SwapParams {
                poolKey: (&request.pool.key).into(),
                zeroForOne: zero_for_one,
                amountIn: trade.amount_in.to::<u128>(),
                amountOutMinimum: limits.minimum_amount_out.to::<u128>(),
                minHopPriceX36: U256::ZERO,
                hookData: request.options.hook_data.clone(),
            }
            .abi_encode()
            .into(),
            (input, trade.amount_in).abi_encode().into(),
            (output, trade.recipient, U256::ZERO).abi_encode().into(),
        ];
        let mut commands = vec![V4_SWAP];
        let mut inputs: Vec<Bytes> = vec![(actions, params).abi_encode().into()];
        if input.is_zero() {
            commands.push(SWEEP);
            inputs.push(
                (Address::ZERO, Address::with_last_byte(1), U256::ZERO)
                    .abi_encode()
                    .into(),
            );
        }
        let calldata = abi::executeCall {
            commands: commands.into(),
            inputs,
            deadline: U256::from(limits.deadline_unix_seconds),
        }
        .abi_encode();
        let token_approval = (!input.is_zero()).then_some(TokenApproval {
            token: input,
            owner: trade.sender,
            spender: self.deployment.permit2,
            amount: trade.amount_in,
        });
        let permit2_approval = (!input.is_zero()).then_some(Permit2Approval {
            permit2: self.deployment.permit2,
            token: input,
            owner: trade.sender,
            spender: self.deployment.universal_router,
            amount: U160::from(trade.amount_in.to::<u128>()),
            expiration: U48::from(limits.deadline_unix_seconds),
        });
        Ok(V4SwapPlan {
            transaction: TransactionRequest {
                chain_id: Some(trade.chain_id),
                from: Some(trade.sender),
                to: Some(self.deployment.universal_router.into()),
                value: Some(if input.is_zero() {
                    trade.amount_in
                } else {
                    U256::ZERO
                }),
                input: calldata.into(),
                ..Default::default()
            },
            token_approval,
            permit2_approval,
        })
    }
}

impl<P: Provider> DiscoverPools for UniswapV4<P> {
    type Query = V4PoolQuery;
    type Search = V4PoolSearch;

    async fn find_pools(&self, query: V4PoolQuery, block: B256) -> Result<V4PoolSearch> {
        if query.chain_id != self.client.chain_id() {
            return Err(Error::ChainMismatch {
                expected: self.client.chain_id(),
                actual: query.chain_id,
            });
        }
        let mut currencies = [
            currency_address(query.currency_a)?,
            currency_address(query.currency_b)?,
        ];
        currencies.sort();
        if currencies[0] == currencies[1] {
            return Err(Error::InvalidTrade("pool currencies must differ".into()));
        }
        let filter = self
            .filter()
            .topic2(currencies[0].into_word())
            .topic3(currencies[1].into_word());
        let outcome = self.search(filter, &query.ranges, block).await?;
        Ok(V4PoolSearch {
            query,
            block_hash: block,
            outcome,
        })
    }
}

impl<P: Provider> ResolvePool for UniswapV4<P> {
    type Input = V4PoolLookup;

    async fn resolve_pool(&self, input: V4PoolLookup, block: B256) -> Result<V4Pool> {
        match input {
            V4PoolLookup::Key(key) => self.load_pool(key, block).await,
            V4PoolLookup::Id { id, ranges } => {
                let outcome = self
                    .search(self.filter().topic1(id), &ranges, block)
                    .await?;
                if let DiscoveryOutcome::Searched {
                    pools, failures, ..
                } = outcome
                {
                    if let Some(pool) = pools.into_iter().find(|pool| pool.key.id() == id) {
                        return Ok(pool);
                    }
                    return Err(Error::UnresolvedPool(format!(
                        "ID was not resolved in the requested ranges; failures: {failures:?}"
                    )));
                }
                Err(Error::UnresolvedPool("V4 is not configured".into()))
            }
        }
    }
}

fn currency_address(currency: Currency) -> Result<Address> {
    match currency {
        Currency::Native => Ok(Address::ZERO),
        Currency::Erc20(address) if !address.is_zero() => Ok(address),
        _ => Err(Error::InvalidTrade(
            "use Currency::Native for the V4 zero address".into(),
        )),
    }
}

fn validate_key(key: &V4PoolKey) -> Result<()> {
    let fee = key.fee.to::<u32>();
    if key.currency0 >= key.currency1
        || key.tick_spacing <= I24::ZERO
        || key.tick_spacing > I24::try_from(32767).unwrap()
        || (fee > 1_000_000 && fee != 0x800000)
        || (fee == 0x800000 && key.hooks.is_zero())
    {
        return Err(Error::InvalidPool(
            "invalid V4 currency order, fee, hook, or tick spacing".into(),
        ));
    }
    Ok(())
}

impl From<&V4PoolKey> for abi::PoolKey {
    fn from(key: &V4PoolKey) -> Self {
        Self {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tick_spacing,
            hooks: key.hooks,
        }
    }
}

mod abi {
    use super::*;
    sol! {
        struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
        struct QuoteParams { PoolKey poolKey; bool zeroForOne; uint128 exactAmount; bytes hookData; }
        struct SwapParams { PoolKey poolKey; bool zeroForOne; uint128 amountIn; uint128 amountOutMinimum; uint256 minHopPriceX36; bytes hookData; }
        function poolManager() external view returns (address);
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function quoteExactInputSingle(QuoteParams params) external returns (uint256 amountOut, uint256 gasEstimate);
        function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
        function allowance(address owner, address token, address spender) external view returns (uint160 amount, uint48 expiration, uint48 nonce);
        event Initialize(bytes32 indexed id, address indexed currency0, address indexed currency1, uint24 fee, int24 tickSpacing, address hooks, uint160 sqrtPriceX96, int24 tick);
    }
}
