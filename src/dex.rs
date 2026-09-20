use std::future::Future;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::TransactionRequest;
use futures_util::future::join_all;

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

    /// Quotes independent candidates concurrently, preserving input order and individual errors.
    /// The supplied batch defines concurrency; dependent hops must still be quoted in sequence.
    fn quote_many(
        &self,
        requests: Vec<QuoteRequest<Self::Pool, Self::QuoteOptions>>,
    ) -> impl Future<Output = Vec<Result<Quote<Self::Pool, Self::QuoteOptions>>>> + Send {
        join_all(requests.into_iter().map(|request| self.quote(request)))
    }

    /// Builds a low-level direct DEX plan. Use FeeRouter::build_swap for our personal swap flow.
    /// Does not submit transactions.
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
    pub slippage_bps: u16,
    pub deadline_unix_seconds: u64,
}

impl SwapLimits {
    /// Floors the quote after slippage; 100 basis points means 1%.
    pub(crate) fn minimum_amount_out(self, quoted_amount_out: U256) -> Result<U256> {
        const BPS: u16 = 10_000;
        if self.slippage_bps > BPS {
            return Err(Error::InvalidTrade(
                "slippage exceeds 10000 basis points".into(),
            ));
        }
        let denominator = U256::from(BPS);
        let remaining = U256::from(BPS - self.slippage_bps);
        Ok(quoted_amount_out / denominator * remaining
            + quoted_amount_out % denominator * remaining / denominator)
    }
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
    #[error(transparent)]
    Execution(#[from] Box<crate::execution::ExecutionError>),
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
    #[error("no successful funding route in the searched V3 fee tiers: {0:?}")]
    NoFundingRoute(Box<crate::fee_router::FundingReport>),
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    use alloy_primitives::U512;
    use futures_util::{future::poll_fn, task::noop_waker_ref};

    use super::*;

    struct PendingDex {
        started: AtomicUsize,
    }

    impl Dex for PendingDex {
        type PoolId = u8;
        type Pool = u8;
        type QuoteOptions = ();
        type SwapPlan = ();

        async fn load_pool(&self, id: u8, _: B256) -> Result<u8> {
            Ok(id)
        }

        async fn quote(&self, request: QuoteRequest<u8>) -> Result<Quote<u8>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut yielded = false;
            poll_fn(|cx| {
                if yielded {
                    return Poll::Ready(());
                }
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            })
            .await;
            if request.pool == 2 {
                return Err(Error::PoolNotFound);
            }
            Ok(Quote {
                amount_out: U256::from(request.pool),
                request,
            })
        }

        fn build_swap(&self, _: &Quote<u8>, _: SwapLimits) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn batch_quotes_start_together_and_keep_order_and_failures() {
        let dex = PendingDex {
            started: AtomicUsize::new(0),
        };
        let requests = [1, 2, 3]
            .map(|pool| QuoteRequest {
                pool,
                trade: ExactInput {
                    chain_id: 4663,
                    currency_in: Currency::Native,
                    currency_out: Currency::Erc20(Address::repeat_byte(1)),
                    amount_in: U256::from(100),
                    sender: Address::repeat_byte(2),
                    recipient: Address::repeat_byte(2),
                },
                block_hash: B256::repeat_byte(7),
                options: (),
            })
            .to_vec();
        let mut future = pin!(dex.quote_many(requests));
        let mut context = Context::from_waker(noop_waker_ref());
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert_eq!(dex.started.load(Ordering::SeqCst), 3);
        let Poll::Ready(results) = future.as_mut().poll(&mut context) else {
            panic!("all quotes should have completed");
        };
        assert_eq!(results[0].as_ref().unwrap().request.pool, 1);
        assert!(matches!(results[1], Err(Error::PoolNotFound)));
        assert_eq!(results[2].as_ref().unwrap().amount_out, U256::from(3));
    }

    #[test]
    fn slippage_rounds_down_without_overflow() {
        for slippage_bps in [0, 1, 50, 100, 500, 9999, 10000] {
            let limits = SwapLimits {
                slippage_bps,
                deadline_unix_seconds: 0,
            };
            for amount in [U256::ZERO, U256::from(1), U256::from(10001), U256::MAX] {
                let expected = (U512::from(amount) * U512::from(10000 - slippage_bps)
                    / U512::from(10000))
                .to::<U256>();
                assert_eq!(limits.minimum_amount_out(amount).unwrap(), expected);
            }
        }
    }

    #[test]
    fn slippage_above_one_hundred_percent_is_rejected() {
        for slippage_bps in [10001, u16::MAX] {
            let limits = SwapLimits {
                slippage_bps,
                deadline_unix_seconds: 0,
            };
            assert!(matches!(
                limits.minimum_amount_out(U256::from(100)),
                Err(Error::InvalidTrade(_))
            ));
        }
    }
}
