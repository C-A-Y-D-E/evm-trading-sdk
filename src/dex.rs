use std::future::Future;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::TransactionRequest;

pub trait Dex: Send + Sync {
    type PoolId: Send + Sync;
    type Pool: Clone + Send + Sync;
    type QuoteOptions: Clone + Send + Sync;
    type SwapPlan: Send;

    fn load_pool(
        &self,
        id: Self::PoolId,
        block_hash: B256,
    ) -> impl Future<Output = Result<Self::Pool>> + Send;

    fn quote(
        &self,
        request: QuoteRequest<Self::Pool, Self::QuoteOptions>,
    ) -> impl Future<Output = Result<Quote<Self::Pool, Self::QuoteOptions>>> + Send;

    /// Builds an unsigned plan for the quoted pool; does not submit transactions.
    fn build_swap(
        &self,
        quote: &Quote<Self::Pool, Self::QuoteOptions>,
        limits: SwapLimits,
    ) -> Result<Self::SwapPlan>;
}

pub trait DiscoverPools: Dex {
    type Query: Send + Sync;
    type Search: Send;

    /// Returns a report that retains partial results and scoped failures.
    /// An outer error means search could not begin or its pinned chain state became invalid.
    fn find_pools(
        &self,
        query: Self::Query,
        block_hash: B256,
    ) -> impl Future<Output = Result<Self::Search>> + Send;
}

pub trait ResolvePool: Dex {
    type Input: Send + Sync;

    /// Resolves identity within supported deployments, including factory or manager checks.
    fn resolve_pool(
        &self,
        input: Self::Input,
        block_hash: B256,
    ) -> impl Future<Output = Result<Self::Pool>> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Currency {
    Native,
    Erc20(Address),
}

#[derive(Clone, Debug)]
pub struct Token {
    pub chain_id: u64,
    pub address: Address,
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub decimals: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactInput {
    pub chain_id: u64,
    pub currency_in: Currency,
    pub currency_out: Currency,
    pub amount_in: U256,
    pub sender: Address,
    pub recipient: Address,
}

#[derive(Clone, Debug)]
pub struct QuoteRequest<P, O = ()> {
    pub pool: P,
    pub trade: ExactInput,
    pub block_hash: B256,
    pub options: O,
}

#[derive(Clone, Debug)]
pub struct Quote<P, O = ()> {
    pub request: QuoteRequest<P, O>,
    pub amount_out: U256,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapLimits {
    pub minimum_amount_out: U256,
    pub deadline_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct SwapTransaction {
    pub transaction: TransactionRequest,
    pub approval: Option<TokenApproval>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenApproval {
    pub token: Address,
    pub owner: Address,
    pub spender: Address,
    pub amount: U256,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("provider chain {actual} does not match requested chain {expected}")]
    ChainMismatch { expected: u64, actual: u64 },
    #[error("{requirement} is not configured on chain {chain_id}")]
    NotConfigured {
        chain_id: u64,
        requirement: &'static str,
    },
    #[error("deployment {0} is not supported")]
    UnsupportedDeployment(Address),
    #[error("no contract at {0}")]
    ContractNotFound(Address),
    #[error("invalid response from {address} for {method}")]
    InvalidResponse {
        address: Address,
        method: &'static str,
        #[source]
        source: alloy_sol_types::Error,
    },
    #[error("contract call {method} at {address} reverted: {reason}")]
    ContractReverted {
        address: Address,
        method: &'static str,
        reason: String,
        revert_data: Bytes,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("pool was not found within the requested scope")]
    PoolNotFound,
    #[error("pool identity matches multiple deployments")]
    AmbiguousPool,
    #[error("pool identity could not be resolved: {0}")]
    UnresolvedPool(String),
    #[error("pool does not match its deployment: {0}")]
    InvalidPool(String),
    #[error("invalid trade: {0}")]
    InvalidTrade(String),
    #[error("RPC request failed")]
    Rpc(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("quote failed: {reason}")]
    QuoteFailed { reason: String, revert_data: Bytes },
}
