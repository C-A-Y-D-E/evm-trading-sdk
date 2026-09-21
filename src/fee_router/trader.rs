use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes, U256, aliases::U24};
use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::sol;

use super::{FeeRouter, PriceImpact, PriceImpactUnavailable, RouteHop, price_impact};
use crate::{
    Dex, DiscoverPools, ResolvePool,
    dex::{Currency, Error, ExactInput, Quote, QuoteRequest, Result, SwapLimits, SwapTransaction},
    execution::{
        Execution, ExecutionJournal, ExecutionOptions, ExecutionRecord, ExecutionResult,
        ExecutionStatus, TradeRequest, check_status,
    },
    submit::Submitter,
    uniswap::{
        DiscoveryOutcome, UniswapDeployment, UniswapPool,
        v2::UniswapV2,
        v3::{UniswapV3, V3PoolQuery, V3PoolSearch},
        v4::{UniswapV4, V4PoolLookup, V4QuoteOptions},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeSide {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuyWith {
    Native,
    Usdc,
}

impl BuyWith {
    pub fn currency(self, chain_id: u64) -> Result<Currency> {
        match self {
            Self::Native => Ok(Currency::Native),
            Self::Usdc => {
                let address = match chain_id {
                    crate::ROBINHOOD_CHAIN_ID => super::ROBINHOOD_USDC,
                    _ => None,
                };
                address.map(Currency::Erc20).ok_or(Error::NotConfigured {
                    chain_id,
                    requirement: "verified USDC buy currency",
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairSwap<I = Address> {
    pub pair: I,
    pub token_address: Address,
    pub side: TradeSide,
    pub amount_in: U256,
}

#[derive(Debug)]
pub struct PreparedSwap {
    pub quote: Quote<Vec<RouteHop>>,
    pub plan: SwapTransaction,
    pub funding: Option<FundingReport>,
    pub price_impact: std::result::Result<PriceImpact, PriceImpactUnavailable>,
    pub router_fee_amount: U256,
    limits: SwapLimits,
}

impl PreparedSwap {
    pub fn limits(&self) -> SwapLimits {
        self.limits
    }

    pub fn minimum_amount_out(&self) -> U256 {
        self.limits
            .minimum_amount_out(self.quote.amount_out)
            .expect("validated limits")
    }
}

#[derive(Debug)]
pub struct FundingReport {
    pub discovery: V3PoolSearch,
    pub quote_failures: Vec<FundingQuoteFailure>,
}

#[derive(Debug)]
pub struct FundingQuoteFailure {
    pub pool: Address,
    pub error: Error,
}

/// Shared trading configuration. Signing credentials and journals belong to each operation.
pub struct Trader<'a, P: Provider> {
    router: &'a FeeRouter<P>,
    native_settlement: bool,
    buy_with: BuyWith,
    execution_options: ExecutionOptions,
}

impl<'a, P: Provider + Clone> Trader<'a, P> {
    pub fn new(router: &'a FeeRouter<P>) -> Self {
        Self {
            router,
            native_settlement: false,
            buy_with: BuyWith::Native,
            execution_options: ExecutionOptions::default(),
        }
    }

    pub fn with_native_settlement(mut self) -> Self {
        self.native_settlement = true;
        self
    }

    pub fn buy_with(mut self, currency: BuyWith) -> Self {
        self.buy_with = currency;
        self
    }

    pub fn with_execution_options(mut self, options: ExecutionOptions) -> Self {
        self.execution_options = options;
        self
    }

    pub fn for_wallet(&self, wallet: Address) -> WalletTrader<'a, P> {
        WalletTrader {
            router: self.router,
            wallet,
            native_settlement: self.native_settlement,
            buy_with: self.buy_with,
        }
    }

    pub async fn prepare_swap(
        &self,
        wallet: Address,
        request: PairSwap,
        limits: SwapLimits,
    ) -> Result<PreparedSwap> {
        self.for_wallet(wallet).prepare_swap(request, limits).await
    }

    pub async fn prepare_v4_swap(
        &self,
        wallet: Address,
        request: PairSwap<V4PoolLookup>,
        options: V4QuoteOptions,
        limits: SwapLimits,
    ) -> Result<PreparedSwap> {
        self.for_wallet(wallet)
            .prepare_v4_swap(request, options, limits)
            .await
    }

    /// Each call has its own record and journal. Serialize trades spending from the same wallet
    /// across the service: pending-nonce reads do not reserve nonces for concurrent calls.
    pub async fn submit<J: ExecutionJournal>(
        &self,
        request: TradeRequest,
        limits: SwapLimits,
        signer: &EthereumWallet,
        submitter: &dyn Submitter,
        journal: &mut J,
    ) -> Result<ExecutionResult> {
        self.for_wallet(signer.default_signer().address())
            .submit(
                &mut self.execution(signer, submitter, journal),
                request,
                limits,
            )
            .await
    }

    pub async fn swap<J: ExecutionJournal>(
        &self,
        request: TradeRequest,
        limits: SwapLimits,
        signer: &EthereumWallet,
        submitter: &dyn Submitter,
        journal: &mut J,
    ) -> Result<ExecutionResult> {
        self.for_wallet(signer.default_signer().address())
            .swap(
                &mut self.execution(signer, submitter, journal),
                request,
                limits,
            )
            .await
    }

    pub async fn submit_prepared<J: ExecutionJournal>(
        &self,
        prepared: PreparedSwap,
        signer: &EthereumWallet,
        submitter: &dyn Submitter,
        journal: &mut J,
    ) -> Result<ExecutionResult> {
        self.for_wallet(signer.default_signer().address())
            .submit_prepared(&mut self.execution(signer, submitter, journal), prepared)
            .await
    }

    pub async fn swap_prepared<J: ExecutionJournal>(
        &self,
        prepared: PreparedSwap,
        signer: &EthereumWallet,
        submitter: &dyn Submitter,
        journal: &mut J,
    ) -> Result<ExecutionResult> {
        self.for_wallet(signer.default_signer().address())
            .swap_prepared(&mut self.execution(signer, submitter, journal), prepared)
            .await
    }

    pub async fn status(&self, record: &ExecutionRecord) -> Result<ExecutionResult> {
        let wallet = record
            .transactions
            .first()
            .and_then(|tx| tx.transaction.from)
            .unwrap_or_default();
        check_status(
            self.router.client.provider(),
            wallet,
            self.router.client.chain_id(),
            record,
        )
        .await
    }

    fn execution<'b, J: ExecutionJournal>(
        &'b self,
        signer: &EthereumWallet,
        submitter: &'b dyn Submitter,
        journal: &'b mut J,
    ) -> Execution<'b, &'b P, &'b mut J> {
        Execution::new(
            self.router.client.provider(),
            journal,
            self.execution_options,
        )
        .with_submitter(signer.clone(), submitter)
    }
}

/// Wallet-bound convenience for unsigned preparation and wallet RPC integrations such as Frame.
pub struct WalletTrader<'a, P: Provider> {
    router: &'a FeeRouter<P>,
    wallet: Address,
    native_settlement: bool,
    buy_with: BuyWith,
}

impl<'a, P: Provider> WalletTrader<'a, P> {
    pub(super) fn new(router: &'a FeeRouter<P>, wallet: Address) -> Self {
        Self {
            router,
            wallet,
            native_settlement: false,
            buy_with: BuyWith::Native,
        }
    }

    /// Uses native ETH instead of WETH at trade endpoints; pool currencies stay unchanged.
    pub fn with_native_settlement(mut self) -> Self {
        self.native_settlement = true;
        self
    }

    /// Selects the buy input, adding a V3 funding hop when needed.
    /// Searches tiers 100/500/3000/10000 only; sells keep their existing input currency.
    pub fn buy_with(mut self, currency: BuyWith) -> Self {
        self.buy_with = currency;
        self
    }

    /// Convenience alias for `buy_with(BuyWith::Native)`.
    pub fn with_native_buy(self) -> Self {
        self.buy_with(BuyWith::Native)
    }

    /// Runs prerequisites, then submits once. Pending approval returns without submitting a swap.
    pub async fn submit<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &mut Execution<'_, S, J>,
        request: TradeRequest,
        limits: SwapLimits,
    ) -> Result<ExecutionResult>
    where
        P: Clone,
    {
        let prepared = self
            .prepare_request(request.clone(), limits)
            .await
            .map_err(preparation_error)?;
        execution
            .submit(
                &self.router.client,
                prepared,
                limits,
                self.refresh(request, limits),
            )
            .await
    }

    /// Submits, then waits up to the configured confirmation timeout. Timeout remains pending.
    pub async fn swap<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &mut Execution<'_, S, J>,
        request: TradeRequest,
        limits: SwapLimits,
    ) -> Result<ExecutionResult>
    where
        P: Clone,
    {
        let result = self.submit(execution, request, limits).await?;
        self.wait_for_swap(execution, result).await
    }

    /// Submits the previewed route and minimum output, without a second route search.
    /// After approval, requotes the same hops but retains the original calldata and fee guard.
    pub async fn submit_prepared<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &mut Execution<'_, S, J>,
        prepared: PreparedSwap,
    ) -> Result<ExecutionResult>
    where
        P: Clone,
    {
        let limits = prepared.limits;
        let expected = self
            .router
            .build_swap(&prepared.quote, limits)
            .map_err(preparation_error)?;
        if prepared.quote.request.trade.sender != self.wallet
            || prepared.quote.request.trade.recipient != self.wallet
            || prepared.plan.transaction != expected.transaction
            || prepared.plan.approval != expected.approval
        {
            return Err(preparation_error(Error::InvalidTrade(
                "prepared swap does not match this wallet and quote".into(),
            )));
        }
        let quote = prepared.quote.clone();
        let plan = prepared.plan.clone();
        execution
            .submit(&self.router.client, prepared, limits, async {
                let mut refreshed = self.refresh_route(quote, limits).await?;
                // The displayed minimum is the user's bound, even if the approval changes the quote.
                refreshed.plan = plan;
                Ok(refreshed)
            })
            .await
    }

    /// Executes the prepared route, then waits within the confirmation timeout.
    pub async fn swap_prepared<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &mut Execution<'_, S, J>,
        prepared: PreparedSwap,
    ) -> Result<ExecutionResult>
    where
        P: Clone,
    {
        let result = self.submit_prepared(execution, prepared).await?;
        self.wait_for_swap(execution, result).await
    }

    async fn wait_for_swap<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &mut Execution<'_, S, J>,
        result: ExecutionResult,
    ) -> Result<ExecutionResult> {
        if result.status != ExecutionStatus::Pending || result.record.swap_hash().is_none() {
            return Ok(result);
        }
        let result = execution
            .wait(self.wallet, self.router.client.chain_id(), &result.record)
            .await?;
        execution.save_result(&result)?;
        Ok(result)
    }

    /// Reads existing transaction state only; never prepares, signs, submits or resends.
    pub async fn status<S: Provider, J: ExecutionJournal>(
        &self,
        execution: &Execution<'_, S, J>,
        record: &ExecutionRecord,
    ) -> Result<ExecutionResult> {
        execution
            .status(self.wallet, self.router.client.chain_id(), record)
            .await
    }

    async fn prepare_request(
        &self,
        request: TradeRequest,
        limits: SwapLimits,
    ) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        match request {
            TradeRequest::Pair(request) => self.prepare_swap(request, limits).await,
            TradeRequest::V4 { request, options } => {
                self.prepare_v4_swap(request, options, limits).await
            }
        }
    }

    async fn refresh(&self, request: TradeRequest, limits: SwapLimits) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        let block = self
            .router
            .client
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|error| Error::Rpc(Box::new(error)))?
            .ok_or_else(|| Error::InvalidTrade("latest block was not returned".into()))?
            .header;
        let router = FeeRouter::connect(self.router.client.clone(), block.hash).await?;
        let trader = WalletTrader {
            router: &router,
            wallet: self.wallet,
            native_settlement: self.native_settlement,
            buy_with: self.buy_with,
        };
        trader.prepare_request(request, limits).await
    }

    async fn refresh_route(
        &self,
        mut quote: Quote<Vec<RouteHop>>,
        limits: SwapLimits,
    ) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        let client = &self.router.client;
        let block = client
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|error| Error::Rpc(Box::new(error)))?
            .ok_or_else(|| Error::InvalidTrade("latest block was not returned".into()))?
            .header
            .hash;
        let router = FeeRouter::connect(client.clone(), block).await?;
        if router.fee_bps != self.router.fee_bps {
            return Err(Error::InvalidTrade(
                "router fee changed since the preview; prepare again".into(),
            ));
        }
        let minimum = limits.minimum_amount_out(quote.amount_out)?;
        let deployment = UniswapDeployment::robinhood_mainnet();
        let mut amount = router.amount_after_fee(quote.request.trade.amount_in);
        for hop in &quote.request.pool {
            let trade = ExactInput {
                currency_in: hop.currency_in,
                currency_out: hop.currency_out,
                amount_in: amount,
                ..quote.request.trade.clone()
            };
            amount = match &hop.pool {
                UniswapPool::V2(pool) => {
                    let adapter = UniswapV2::connect(
                        client.clone(),
                        deployment
                            .v2
                            .clone()
                            .ok_or(self.not_configured("Uniswap V2"))?,
                        block,
                    )
                    .await?;
                    adapter
                        .quote(QuoteRequest {
                            pool: pool.clone(),
                            trade,
                            block_hash: block,
                            options: (),
                        })
                        .await?
                        .amount_out
                }
                UniswapPool::V3(pool) => {
                    let adapter = UniswapV3::connect(
                        client.clone(),
                        deployment
                            .v3
                            .clone()
                            .ok_or(self.not_configured("Uniswap V3"))?,
                        block,
                    )
                    .await?;
                    adapter
                        .quote(QuoteRequest {
                            pool: pool.clone(),
                            trade,
                            block_hash: block,
                            options: (),
                        })
                        .await?
                        .amount_out
                }
                UniswapPool::V4(pool) => {
                    let adapter = UniswapV4::connect(
                        client.clone(),
                        deployment
                            .v4
                            .clone()
                            .ok_or(self.not_configured("Uniswap V4"))?,
                        block,
                    )
                    .await?;
                    adapter
                        .quote(QuoteRequest {
                            pool: pool.clone(),
                            trade,
                            block_hash: block,
                            options: V4QuoteOptions {
                                hook_data: hop.hook_data.clone(),
                            },
                        })
                        .await?
                        .amount_out
                }
            };
        }
        if amount < minimum {
            return Err(Error::InvalidTrade(
                "refreshed output is below the previewed minimum; prepare again".into(),
            ));
        }
        quote.amount_out = amount;
        quote.request.block_hash = block;
        router
            .trader(self.wallet)
            .finish_preparation(quote, None, limits)
            .await
    }

    /// Resolves a V2/V3 pair and quotes it at the router's configuration block.
    /// Returns an unsigned plan; it does not submit approvals or swaps.
    pub async fn prepare_swap(&self, request: PairSwap, limits: SwapLimits) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        self.validate_request(&request, limits)?;
        let block = self.router.configuration_block;
        let client = &self.router.client;
        let deployment = UniswapDeployment::robinhood_mainnet();
        let factory = client.call(request.pair, factoryCall {}, block).await?;
        if factory == self.router.v2_factory {
            let config = deployment.v2.ok_or(self.not_configured("Uniswap V2"))?;
            let adapter = UniswapV2::connect(client.clone(), config, block).await?;
            let pool = adapter.load_pool(request.pair, block).await?;
            let hop = self.hop(UniswapPool::V2(pool.clone()), &request, Bytes::new())?;
            self.prepare_pool(&adapter, pool, request, hop, (), limits)
                .await
        } else if factory == self.router.v3_factory {
            let config = deployment.v3.ok_or(self.not_configured("Uniswap V3"))?;
            let adapter = UniswapV3::connect(client.clone(), config, block).await?;
            let pool = adapter.load_pool(request.pair, block).await?;
            let hop = self.hop(UniswapPool::V3(pool.clone()), &request, Bytes::new())?;
            self.prepare_pool(&adapter, pool, request, hop, (), limits)
                .await
        } else {
            Err(Error::UnsupportedDeployment(factory))
        }
    }

    /// V4 uses an explicit key or an ID with initialization-history ranges.
    /// Hook-sensitive execution still needs simulation in the actual router context.
    pub async fn prepare_v4_swap(
        &self,
        request: PairSwap<V4PoolLookup>,
        options: V4QuoteOptions,
        limits: SwapLimits,
    ) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        self.validate_request(&request, limits)?;
        let config = UniswapDeployment::robinhood_mainnet()
            .v4
            .ok_or(self.not_configured("Uniswap V4"))?;
        let block = self.router.configuration_block;
        let adapter = UniswapV4::connect(self.router.client.clone(), config, block).await?;
        let pool = adapter.resolve_pool(request.pair.clone(), block).await?;
        let hop = self.hop(
            UniswapPool::V4(pool.clone()),
            &request,
            options.hook_data.clone(),
        )?;
        self.prepare_pool(&adapter, pool, request, hop, options, limits)
            .await
    }

    fn validate_request<I>(&self, request: &PairSwap<I>, limits: SwapLimits) -> Result<()> {
        limits.minimum_amount_out(U256::ZERO)?;
        if request.side == TradeSide::Buy {
            self.buy_with.currency(self.router.client.chain_id())?;
        }
        if self.wallet.is_zero()
            || self.wallet == self.router.address
            || self.router.amount_after_fee(request.amount_in).is_zero()
        {
            return Err(Error::InvalidTrade(
                "a swap needs an external wallet and input after fees".into(),
            ));
        }
        Ok(())
    }

    fn hop<I>(
        &self,
        pool: UniswapPool,
        request: &PairSwap<I>,
        hook_data: Bytes,
    ) -> Result<RouteHop> {
        let tokens = match &pool {
            UniswapPool::V2(pool) => [pool.token0, pool.token1],
            UniswapPool::V3(pool) => [pool.token0, pool.token1],
            UniswapPool::V4(pool) => [pool.key.currency0, pool.key.currency1],
        };
        let [input, output] = request.side.direction(tokens, request.token_address)?;
        Ok(RouteHop {
            pool,
            currency_in: pool_currency(input),
            currency_out: pool_currency(output),
            hook_data,
        })
    }

    async fn prepare_pool<D: Dex, I>(
        &self,
        adapter: &D,
        pool: D::Pool,
        request: PairSwap<I>,
        hop: RouteHop,
        options: D::QuoteOptions,
        limits: SwapLimits,
    ) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        let currency_in = match request.side {
            TradeSide::Buy => self.buy_with.currency(self.router.client.chain_id())?,
            TradeSide::Sell => self.settlement_currency(hop.currency_in),
        };
        let trade = ExactInput {
            chain_id: self.router.client.chain_id(),
            currency_in,
            currency_out: self.settlement_currency(hop.currency_out),
            amount_in: request.amount_in,
            sender: self.wallet,
            recipient: self.wallet,
        };
        let mut net_trade = trade.clone();
        net_trade.amount_in = self.router.amount_after_fee(trade.amount_in);
        // Quote the pool's currencies; ETH/WETH conversion happens around the hop in FeeRouter.
        net_trade.currency_in = hop.currency_in;
        net_trade.currency_out = hop.currency_out;
        if self.wrapped_currency(trade.currency_in) != self.wrapped_currency(hop.currency_in) {
            return self
                .prepare_funded(
                    adapter,
                    QuoteRequest {
                        pool,
                        trade: net_trade,
                        block_hash: self.router.configuration_block,
                        options,
                    },
                    trade,
                    hop,
                    limits,
                )
                .await;
        }
        let pool_quote = adapter
            .quote(QuoteRequest {
                pool,
                trade: net_trade,
                block_hash: self.router.configuration_block,
                options,
            })
            .await?;
        let quote = Quote {
            request: QuoteRequest {
                pool: vec![hop],
                trade,
                block_hash: self.router.configuration_block,
                options: (),
            },
            amount_out: pool_quote.amount_out,
        };
        self.finish_preparation(quote, None, limits).await
    }

    async fn prepare_funded<D: Dex>(
        &self,
        target: &D,
        target_request: QuoteRequest<D::Pool, D::QuoteOptions>,
        trade: ExactInput,
        hop: RouteHop,
        limits: SwapLimits,
    ) -> Result<PreparedSwap>
    where
        P: Clone,
    {
        let block = self.router.configuration_block;
        let config = UniswapDeployment::robinhood_mainnet()
            .v3
            .ok_or(self.not_configured("Uniswap V3 funding"))?;
        let funding = UniswapV3::connect(self.router.client.clone(), config, block).await?;
        let mut funding_trade = target_request.trade.clone();
        funding_trade.currency_in = self.wrapped_currency(trade.currency_in);
        funding_trade.currency_out = self.wrapped_currency(hop.currency_in);
        let discovery = funding
            .find_pools(
                V3PoolQuery {
                    chain_id: trade.chain_id,
                    currency_a: funding_trade.currency_in,
                    currency_b: funding_trade.currency_out,
                    fee_tiers: [100, 500, 3000, 10_000].map(U24::from).to_vec(),
                },
                block,
            )
            .await?;
        let pools = match &discovery.outcome {
            DiscoveryOutcome::Searched { pools, .. } => pools.clone(),
            DiscoveryOutcome::NotConfigured => Vec::new(),
        };
        // Rank final output, but still simulate the full call: hooks and repeated pools can
        // behave differently from isolated hop quotes. This search excludes other protocols and fees.
        let candidates = futures_util::future::join_all(pools.iter().map(|pool| async {
            let first = funding
                .quote(QuoteRequest {
                    pool: pool.clone(),
                    trade: funding_trade.clone(),
                    block_hash: block,
                    options: (),
                })
                .await?;
            let mut request = target_request.clone();
            request.trade.amount_in = first.amount_out;
            let last = target.quote(request).await?;
            Ok::<_, Error>((pool.clone(), last.amount_out))
        }))
        .await;
        let mut report = FundingReport {
            discovery,
            quote_failures: Vec::new(),
        };
        let mut best = None;
        for (pool, candidate) in pools.into_iter().zip(candidates) {
            match candidate {
                Ok(candidate) => {
                    if best
                        .as_ref()
                        .is_none_or(|(_, amount)| candidate.1 > *amount)
                    {
                        best = Some(candidate);
                    }
                }
                Err(error) => report.quote_failures.push(FundingQuoteFailure {
                    pool: pool.address,
                    error,
                }),
            }
        }
        let Some((pool, amount_out)) = best else {
            return Err(Error::NoFundingRoute(Box::new(report)));
        };
        let quote = Quote {
            request: QuoteRequest {
                pool: vec![
                    RouteHop {
                        pool: UniswapPool::V3(pool),
                        currency_in: funding_trade.currency_in,
                        currency_out: funding_trade.currency_out,
                        hook_data: Bytes::new(),
                    },
                    hop,
                ],
                trade,
                block_hash: block,
                options: (),
            },
            amount_out,
        };
        self.finish_preparation(quote, Some(report), limits).await
    }

    async fn finish_preparation(
        &self,
        quote: Quote<Vec<RouteHop>>,
        funding: Option<FundingReport>,
        limits: SwapLimits,
    ) -> Result<PreparedSwap> {
        let plan = self.router.build_swap(&quote, limits)?;
        let input = quote.request.trade.amount_in;
        let price_impact = price_impact::estimate(
            &self.router.client,
            &quote.request.pool,
            quote.request.block_hash,
            input,
            self.router.amount_after_fee(input),
            quote.amount_out,
        )
        .await;
        Ok(PreparedSwap {
            quote,
            plan,
            funding,
            price_impact,
            router_fee_amount: self.router.input_fee(input),
            limits,
        })
    }

    fn wrapped_currency(&self, currency: Currency) -> Currency {
        match currency {
            Currency::Native => Currency::Erc20(self.router.wrapped_native),
            token => token,
        }
    }

    fn settlement_currency(&self, currency: Currency) -> Currency {
        if self.native_settlement && currency == Currency::Erc20(self.router.wrapped_native) {
            Currency::Native
        } else {
            currency
        }
    }

    fn not_configured(&self, requirement: &'static str) -> Error {
        Error::NotConfigured {
            chain_id: self.router.client.chain_id(),
            requirement,
        }
    }
}

impl TradeSide {
    fn direction(self, tokens: [Address; 2], target: Address) -> Result<[Address; 2]> {
        let other = if target == tokens[0] {
            tokens[1]
        } else if target == tokens[1] {
            tokens[0]
        } else {
            return Err(Error::InvalidTrade(
                "target token is not in the selected pair".into(),
            ));
        };
        Ok(match self {
            Self::Buy => [other, target],
            Self::Sell => [target, other],
        })
    }
}

fn pool_currency(address: Address) -> Currency {
    if address.is_zero() {
        Currency::Native
    } else {
        Currency::Erc20(address)
    }
}

fn preparation_error(error: Error) -> Error {
    Error::Execution(Box::new(crate::execution::ExecutionError {
        stage: crate::execution::ExecutionStage::Preparation,
        failure: crate::execution::ExecutionFailure::Operation(Box::new(error)),
        record: ExecutionRecord::default(),
    }))
}

sol! { function factory() external view returns (address); }
