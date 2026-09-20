use std::{future::Future, time::Duration};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, transport::TransportError};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use serde::{Deserialize, Serialize};

use crate::{
    EvmClient, PairSwap, PreparedSwap,
    dex::{Error, Result, SwapLimits},
    uniswap::v4::{V4PoolLookup, V4QuoteOptions},
};

#[derive(Clone, Debug)]
pub enum TradeRequest {
    Pair(PairSwap),
    V4 {
        request: PairSwap<V4PoolLookup>,
        options: V4QuoteOptions,
    },
}

type Source = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionKind {
    Approval,
    Swap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmissionState {
    Unknown,
    Submitted(B256),
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub kind: TransactionKind,
    pub transaction: TransactionRequest,
    pub submission: SubmissionState,
    pub receipt: Option<TransactionReceipt>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub transactions: Vec<TransactionRecord>,
}

impl ExecutionRecord {
    pub fn swap_hash(&self) -> Option<B256> {
        self.transactions
            .iter()
            .rev()
            .find_map(|tx| match (tx.kind, tx.submission) {
                (TransactionKind::Swap, SubmissionState::Submitted(hash)) => Some(hash),
                _ => None,
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    Pending,
    Confirmed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    ApprovalRequired,
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub status: ExecutionStatus,
    pub record: ExecutionRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStage {
    Preparation,
    Funding,
    Allowance,
    Simulation,
    Estimation,
    Wallet,
    Persistence,
    Submission,
    Confirmation,
    Refresh,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionFailure {
    #[error("execution operation failed")]
    Operation(#[source] Source),
    #[error("wallet rejected the request")]
    Rejected(#[source] TransportError),
    #[error("submission outcome is unknown; do not resend automatically")]
    UnknownSubmission(#[source] TransportError),
    #[error("input balance {available} is below required {required}")]
    InsufficientFunds { available: U256, required: U256 },
    #[error("approval simulation returned false")]
    ApprovalRejected,
    #[error("confirmed approval is not visible at the refreshed quote block")]
    AllowanceNotVisible,
    #[error("wallet is not authorized by the RPC provider")]
    WalletUnavailable,
    #[error("swap deadline has expired")]
    Expired,
    #[error("refreshed preparation changed the original trade intent")]
    IntentChanged,
    #[error("mined transaction or receipt differs from the submitted record")]
    TransactionMismatch,
    #[error("execution record does not belong to this wallet and chain")]
    RecordMismatch,
    #[error("transaction value plus estimated gas exceeds the supported amount range")]
    GasBudgetOverflow,
}

#[derive(Debug, thiserror::Error)]
#[error("{stage:?}: {failure}")]
pub struct ExecutionError {
    pub stage: ExecutionStage,
    #[source]
    pub failure: ExecutionFailure,
    pub record: ExecutionRecord,
}

/// Persist before sending and after receiving a hash. `begin` must reject an unresolved run.
/// Storage failure stops execution; callers retain the typed record in the returned error.
pub trait ExecutionJournal {
    fn begin(&mut self, record: &ExecutionRecord) -> std::result::Result<(), Source>;
    fn save(&mut self, record: &ExecutionRecord) -> std::result::Result<(), Source>;
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutionOptions {
    pub confirmation_timeout: Duration,
    pub poll_interval: Duration,
    pub check_rpc_account: bool,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            confirmation_timeout: Duration::from_secs(240),
            poll_interval: Duration::from_secs(2),
            check_rpc_account: false,
        }
    }
}

/// Execution through a wallet RPC that implements `eth_sendTransaction` (such as Frame).
/// The provider transport must not retry send requests; local signer/filler submission is not used.
pub struct Execution<S, J> {
    provider: S,
    journal: J,
    options: ExecutionOptions,
    record: ExecutionRecord,
}

impl<S: Provider, J: ExecutionJournal> Execution<S, J> {
    pub fn new(provider: S, journal: J, options: ExecutionOptions) -> Self {
        Self {
            provider,
            journal,
            options,
            record: ExecutionRecord::default(),
        }
    }

    /// Read-only preflight. ApprovalRequired means the swap has not been simulated.
    pub async fn preview<P: Provider>(
        &self,
        client: &EvmClient<P>,
        prepared: &PreparedSwap,
    ) -> Result<Readiness> {
        self.check_chain(client.chain_id()).await?;
        self.check_funding(client, prepared).await?;
        if self.needs_approval(client, prepared).await? {
            return Ok(Readiness::ApprovalRequired);
        }
        self.simulate(&prepared.plan.transaction, TransactionKind::Swap)
            .await?;
        Ok(Readiness::Ready)
    }

    pub(crate) async fn submit<P: Provider, F: Future<Output = Result<PreparedSwap>>>(
        &mut self,
        client: &EvmClient<P>,
        prepared: PreparedSwap,
        limits: SwapLimits,
        refresh: F,
    ) -> Result<ExecutionResult> {
        self.record = ExecutionRecord::default();
        self.check_funding(client, &prepared).await?;
        let mut current = prepared;
        if self.needs_approval(client, &current).await? {
            let approval = current.plan.approval.as_ref().expect("checked approval");
            let transaction = TransactionRequest {
                from: Some(approval.owner),
                to: Some(approval.token.into()),
                chain_id: Some(client.chain_id()),
                value: Some(U256::ZERO),
                input: approveCall {
                    spender: approval.spender,
                    amount: approval.amount,
                }
                .abi_encode()
                .into(),
                ..Default::default()
            };
            self.send(transaction, TransactionKind::Approval, limits)
                .await?;
            let outcome = self
                .wait(approval.owner, client.chain_id(), &self.record)
                .await?;
            self.record = outcome.record;
            self.persist(false)?;
            if outcome.status != ExecutionStatus::Confirmed {
                return Ok(self.outcome(outcome.status));
            }
            let refreshed = refresh.await.map_err(|error| {
                self.failure(
                    ExecutionStage::Refresh,
                    ExecutionFailure::Operation(Box::new(error)),
                )
            })?;
            if !same_intent(&current, &refreshed) {
                return Err(self.failure(ExecutionStage::Refresh, ExecutionFailure::IntentChanged));
            }
            current = refreshed;
            self.check_funding(client, &current).await?;
            if self.needs_approval(client, &current).await? {
                return Err(self.failure(
                    ExecutionStage::Allowance,
                    ExecutionFailure::AllowanceNotVisible,
                ));
            }
        }
        self.send(current.plan.transaction, TransactionKind::Swap, limits)
            .await?;
        Ok(self.outcome(ExecutionStatus::Pending))
    }

    pub(crate) async fn check_funding<P: Provider>(
        &self,
        client: &EvmClient<P>,
        prepared: &PreparedSwap,
    ) -> Result<()> {
        let trade = &prepared.quote.request.trade;
        let available = client
            .balance(
                trade.currency_in,
                trade.sender,
                prepared.quote.request.block_hash,
            )
            .await
            .map_err(|error| {
                self.failure(
                    ExecutionStage::Funding,
                    ExecutionFailure::Operation(Box::new(error)),
                )
            })?;
        if available < trade.amount_in {
            return Err(self.failure(
                ExecutionStage::Funding,
                ExecutionFailure::InsufficientFunds {
                    available,
                    required: trade.amount_in,
                },
            ));
        }
        Ok(())
    }

    pub(crate) async fn needs_approval<P: Provider>(
        &self,
        client: &EvmClient<P>,
        prepared: &PreparedSwap,
    ) -> Result<bool> {
        let Some(approval) = &prepared.plan.approval else {
            return Ok(false);
        };
        let allowance = client
            .allowance(
                approval.token,
                approval.owner,
                approval.spender,
                prepared.quote.request.block_hash,
            )
            .await
            .map_err(|error| {
                self.failure(
                    ExecutionStage::Allowance,
                    ExecutionFailure::Operation(Box::new(error)),
                )
            })?;
        Ok(allowance < approval.amount)
    }

    async fn send(
        &mut self,
        mut transaction: TransactionRequest,
        kind: TransactionKind,
        limits: SwapLimits,
    ) -> Result<()> {
        self.check_deadline(limits)?;
        let wallet = transaction.from.ok_or_else(|| {
            self.failure(ExecutionStage::Wallet, ExecutionFailure::RecordMismatch)
        })?;
        let chain = transaction.chain_id.ok_or_else(|| {
            self.failure(ExecutionStage::Wallet, ExecutionFailure::RecordMismatch)
        })?;
        self.check_chain(chain).await?;
        if self.options.check_rpc_account {
            let accounts = self
                .provider
                .get_accounts()
                .await
                .map_err(|error| self.rpc_error(ExecutionStage::Wallet, error))?;
            if !accounts.contains(&wallet) {
                return Err(
                    self.failure(ExecutionStage::Wallet, ExecutionFailure::WalletUnavailable)
                );
            }
        }
        // Frame consumes `data`; keep every SDK byte when serializing through Alloy.
        transaction.normalize_data();
        self.simulate(&transaction, kind).await?;
        let estimate = self
            .provider
            .estimate_gas(transaction.clone())
            .await
            .map_err(|error| self.rpc_error(ExecutionStage::Estimation, error))?;
        transaction.gas = Some(estimate.saturating_mul(120) / 100);
        self.check_gas_funding(&transaction).await?;
        transaction.nonce = Some(
            self.provider
                .get_transaction_count(wallet)
                .pending()
                .await
                .map_err(|error| self.rpc_error(ExecutionStage::Wallet, error))?,
        );
        self.check_deadline(limits)?;
        let first = self.record.transactions.is_empty();
        self.record.transactions.push(TransactionRecord {
            kind,
            transaction: transaction.clone(),
            submission: SubmissionState::Unknown,
            receipt: None,
        });
        self.persist(first)?;
        // A transport error can follow a successful send. Never retry or choose another trade.
        let result: std::result::Result<B256, TransportError> = self
            .provider
            .raw_request("eth_sendTransaction".into(), (&transaction,))
            .await;
        match result {
            Ok(hash) => {
                self.record.transactions.last_mut().unwrap().submission =
                    SubmissionState::Submitted(hash)
            }
            Err(error) => {
                if error
                    .as_error_resp()
                    .is_some_and(|response| response.code == 4001)
                {
                    self.record.transactions.last_mut().unwrap().submission =
                        SubmissionState::Rejected;
                    self.persist(false)?;
                    return Err(self.failure(
                        ExecutionStage::Submission,
                        ExecutionFailure::Rejected(error),
                    ));
                }
                return Err(self.failure(
                    ExecutionStage::Submission,
                    ExecutionFailure::UnknownSubmission(error),
                ));
            }
        }
        self.persist(false)
    }

    async fn simulate(
        &self,
        transaction: &TransactionRequest,
        kind: TransactionKind,
    ) -> Result<()> {
        let transaction = transaction.clone().normalized_data();
        let returned: Bytes = self
            .provider
            .raw_request("eth_call".into(), (&transaction, "latest"))
            .await
            .map_err(|error| self.rpc_error(ExecutionStage::Simulation, error))?;
        if kind == TransactionKind::Approval && !returned.is_empty() {
            let accepted =
                approveCall::abi_decode_returns_validate(&returned).map_err(|error| {
                    self.failure(
                        ExecutionStage::Simulation,
                        ExecutionFailure::Operation(Box::new(error)),
                    )
                })?;
            if !accepted {
                return Err(self.failure(
                    ExecutionStage::Simulation,
                    ExecutionFailure::ApprovalRejected,
                ));
            }
        }
        Ok(())
    }

    async fn check_gas_funding(&self, transaction: &TransactionRequest) -> Result<()> {
        let wallet = transaction.from.expect("validated sender");
        let price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|error| self.rpc_error(ExecutionStage::Estimation, error))?;
        let available = self
            .provider
            .get_balance(wallet)
            .pending()
            .await
            .map_err(|error| self.rpc_error(ExecutionStage::Funding, error))?;
        // This checks the current estimate; wallet-selected fees and later gas prices can differ.
        let gas_cost = U256::from(transaction.gas.unwrap_or_default()) * U256::from(price);
        let required = gas_cost
            .checked_add(transaction.value.unwrap_or_default())
            .ok_or_else(|| {
                self.failure(
                    ExecutionStage::Estimation,
                    ExecutionFailure::GasBudgetOverflow,
                )
            })?;
        if available < required {
            return Err(self.failure(
                ExecutionStage::Funding,
                ExecutionFailure::InsufficientFunds {
                    available,
                    required,
                },
            ));
        }
        Ok(())
    }

    pub(crate) async fn status(
        &self,
        wallet: Address,
        chain: u64,
        record: &ExecutionRecord,
    ) -> Result<ExecutionResult> {
        let mut updated = record.clone();
        let error = |failure| execution_error(ExecutionStage::Confirmation, failure, record);
        if record.transactions.is_empty()
            || record.transactions.iter().any(|tx| {
                tx.transaction.from != Some(wallet) || tx.transaction.chain_id != Some(chain)
            })
        {
            return Err(error(ExecutionFailure::RecordMismatch));
        }
        let actual = self
            .provider
            .get_chain_id()
            .await
            .map_err(|source| error(ExecutionFailure::Operation(Box::new(source))))?;
        if actual != chain {
            return Err(error(ExecutionFailure::RecordMismatch));
        }
        let mut status = ExecutionStatus::Confirmed;
        for tx in &mut updated.transactions {
            let hash = match tx.submission {
                SubmissionState::Unknown => {
                    status = ExecutionStatus::Pending;
                    continue;
                }
                SubmissionState::Rejected => {
                    return Ok(ExecutionResult {
                        status: ExecutionStatus::Failed,
                        record: updated,
                    });
                }
                SubmissionState::Submitted(hash) => hash,
            };
            let receipt = self
                .provider
                .get_transaction_receipt(hash)
                .await
                .map_err(|source| error(ExecutionFailure::Operation(Box::new(source))))?;
            let Some(receipt) = receipt else {
                tx.receipt = None;
                status = ExecutionStatus::Pending;
                continue;
            };
            let mined: Option<TransactionRequest> = self
                .provider
                .raw_request("eth_getTransactionByHash".into(), (hash,))
                .await
                .map_err(|source| error(ExecutionFailure::Operation(Box::new(source))))?;
            if receipt.transaction_hash != hash
                || receipt.block_hash.is_none()
                || !mined
                    .as_ref()
                    .is_some_and(|mined| same_transaction(&tx.transaction, mined))
            {
                return Err(error(ExecutionFailure::TransactionMismatch));
            }
            let success = receipt.status();
            tx.receipt = Some(receipt);
            if !success {
                return Ok(ExecutionResult {
                    status: ExecutionStatus::Failed,
                    record: updated,
                });
            }
        }
        Ok(ExecutionResult {
            status,
            record: updated,
        })
    }

    pub(crate) async fn wait(
        &self,
        wallet: Address,
        chain: u64,
        record: &ExecutionRecord,
    ) -> Result<ExecutionResult> {
        let deadline = tokio::time::Instant::now() + self.options.confirmation_timeout;
        let mut current = record.clone();
        loop {
            match tokio::time::timeout_at(deadline, self.status(wallet, chain, &current)).await {
                Ok(result) => {
                    let result = result?;
                    if result.status != ExecutionStatus::Pending {
                        return Ok(result);
                    }
                    current = result.record;
                }
                Err(_) => {
                    return Ok(ExecutionResult {
                        status: ExecutionStatus::Pending,
                        record: current,
                    });
                }
            }
            if tokio::time::timeout_at(deadline, tokio::time::sleep(self.options.poll_interval))
                .await
                .is_err()
            {
                return Ok(ExecutionResult {
                    status: ExecutionStatus::Pending,
                    record: current,
                });
            }
        }
    }

    pub(crate) fn save_result(&mut self, result: &ExecutionResult) -> Result<()> {
        self.record = result.record.clone();
        self.persist(false)
    }

    async fn check_chain(&self, expected: u64) -> Result<()> {
        let actual = self
            .provider
            .get_chain_id()
            .await
            .map_err(|error| self.rpc_error(ExecutionStage::Wallet, error))?;
        if actual != expected {
            return Err(self.failure(
                ExecutionStage::Wallet,
                ExecutionFailure::Operation(Box::new(Error::ChainMismatch { expected, actual })),
            ));
        }
        Ok(())
    }

    fn check_deadline(&self, limits: SwapLimits) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| {
                self.failure(
                    ExecutionStage::Preparation,
                    ExecutionFailure::Operation(Box::new(error)),
                )
            })?
            .as_secs();
        if now >= limits.deadline_unix_seconds {
            return Err(self.failure(ExecutionStage::Preparation, ExecutionFailure::Expired));
        }
        Ok(())
    }

    fn persist(&mut self, first: bool) -> Result<()> {
        let result = if first {
            self.journal.begin(&self.record)
        } else {
            self.journal.save(&self.record)
        };
        result.map_err(|error| {
            self.failure(
                ExecutionStage::Persistence,
                ExecutionFailure::Operation(error),
            )
        })
    }

    fn outcome(&self, status: ExecutionStatus) -> ExecutionResult {
        ExecutionResult {
            status,
            record: self.record.clone(),
        }
    }
    fn rpc_error(&self, stage: ExecutionStage, error: TransportError) -> Error {
        self.failure(stage, ExecutionFailure::Operation(Box::new(error)))
    }
    fn failure(&self, stage: ExecutionStage, failure: ExecutionFailure) -> Error {
        execution_error(stage, failure, &self.record)
    }
}

fn execution_error(
    stage: ExecutionStage,
    failure: ExecutionFailure,
    record: &ExecutionRecord,
) -> Error {
    Error::Execution(Box::new(ExecutionError {
        stage,
        failure,
        record: record.clone(),
    }))
}

fn same_transaction(expected: &TransactionRequest, actual: &TransactionRequest) -> bool {
    expected.from == actual.from
        && expected.to == actual.to
        && expected.value == actual.value
        && expected.nonce == actual.nonce
        && expected.chain_id == actual.chain_id
        && expected.input.input() == actual.input.input()
}

fn same_intent(original: &PreparedSwap, refreshed: &PreparedSwap) -> bool {
    original.quote.request.trade == refreshed.quote.request.trade
        && original
            .quote
            .request
            .pool
            .last()
            .zip(refreshed.quote.request.pool.last())
            .is_some_and(|(before, after)| {
                before.pool == after.pool
                    && before.currency_in == after.currency_in
                    && before.currency_out == after.currency_out
                    && before.hook_data == after.hook_data
            })
}

sol! { function approve(address spender, uint256 amount) external returns (bool); }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mined_identity_checks_every_saved_field() {
        let expected = TransactionRequest {
            from: Some(Address::repeat_byte(1)),
            to: Some(Address::repeat_byte(2).into()),
            nonce: Some(7),
            chain_id: Some(4663),
            value: Some(U256::from(123)),
            input: Bytes::from_static(b"calldata").into(),
            ..Default::default()
        };
        assert!(same_transaction(&expected, &expected));
        for (field, value) in [
            ("from", json!(Address::ZERO)),
            ("to", json!(Address::ZERO)),
            ("nonce", json!("0x1")),
            ("chainId", json!("0x1")),
            ("value", json!("0x1")),
            ("input", json!("0x")),
        ] {
            let mut altered = serde_json::to_value(&expected).unwrap();
            altered[field] = value;
            assert!(
                !same_transaction(&expected, &serde_json::from_value(altered).unwrap()),
                "accepted different {field}"
            );
        }
        let frame = expected.clone().normalized_data();
        let json = serde_json::to_value(&frame).unwrap();
        assert!(json.get("input").is_none());
        assert_eq!(json["data"], json!(expected.input.input().unwrap()));
        assert!(same_transaction(&expected, &frame));
    }
}
