use std::time::Duration;

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, hex};
use futures_util::future::BoxFuture;
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};

const ROBINHOOD_SUBMIT_METHOD: &str = "robinhood_tx";

/// Broadcasts an already signed transaction once. Implementations must not retry sends.
/// Errors after a request might have reached the service must remain uncertain.
pub trait Submitter: Send + Sync {
    fn check_chain(&self, chain_id: u64) -> Result<(), SubmitError>;
    fn submit<'a>(
        &'a self,
        transaction: &'a TxEnvelope,
    ) -> BoxFuture<'a, Result<B256, SubmitError>>;
}

impl<T: Submitter + ?Sized> Submitter for &T {
    fn check_chain(&self, chain_id: u64) -> Result<(), SubmitError> {
        (**self).check_chain(chain_id)
    }

    fn submit<'a>(
        &'a self,
        transaction: &'a TxEnvelope,
    ) -> BoxFuture<'a, Result<B256, SubmitError>> {
        (**self).submit(transaction)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("submitter host must be an HTTP(S) URL without embedded credentials")]
    InvalidHost,
    #[error("invalid Authorization header")]
    InvalidAuthorization,
    #[error("bloXroute Robinhood submission does not support chain {0}")]
    UnsupportedChain(u64),
    #[error("HTTP transport error")]
    Transport(#[source] reqwest::Error),
    #[error("submission returned HTTP {0}")]
    Http(u16),
    #[error("submission RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("submission returned an invalid JSON-RPC response")]
    InvalidResponse,
    #[error("submission returned hash {actual}, expected {expected}")]
    HashMismatch { expected: B256, actual: B256 },
}

impl SubmitError {
    pub(crate) fn is_rejection(&self) -> bool {
        matches!(self, Self::UnsupportedChain(_) | Self::Http(401 | 403))
    }
}

/// Robinhood Cloud API submission. Transactions are automatically eligible for BackRunMe.
/// `fast` uses the service default asynchronous node validation; it does not skip SDK simulation.
#[derive(Clone)]
pub struct BloxrouteSubmitter {
    client: Client,
    host: Url,
}

impl BloxrouteSubmitter {
    pub fn fast(host: impl AsRef<str>, auth: impl AsRef<str>) -> Result<Self, SubmitError> {
        let host = Url::parse(host.as_ref()).map_err(|_| SubmitError::InvalidHost)?;
        if !matches!(host.scheme(), "http" | "https")
            || host.host_str().is_none()
            || !host.username().is_empty()
            || host.password().is_some()
        {
            return Err(SubmitError::InvalidHost);
        }
        let mut authorization =
            HeaderValue::from_str(auth.as_ref()).map_err(|_| SubmitError::InvalidAuthorization)?;
        if authorization.is_empty() {
            return Err(SubmitError::InvalidAuthorization);
        }
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(SubmitError::Transport)?;
        Ok(Self { client, host })
    }

    pub async fn submit(&self, transaction: &TxEnvelope) -> Result<B256, SubmitError> {
        self.check_chain(transaction.chain_id().unwrap_or_default())?;
        let request = SubmitRequest {
            jsonrpc: "2.0",
            id: 1,
            method: ROBINHOOD_SUBMIT_METHOD,
            params: SubmitParams {
                transaction: hex::encode(transaction.encoded_2718()),
            },
        };
        let response = self
            .client
            .post(self.host.clone())
            .json(&request)
            .send()
            .await
            .map_err(|error| SubmitError::Transport(error.without_url()))?;
        if !response.status().is_success() {
            return Err(SubmitError::Http(response.status().as_u16()));
        }
        let response: SubmitResponse = response
            .json()
            .await
            .map_err(|error| SubmitError::Transport(error.without_url()))?;
        if response.jsonrpc != "2.0"
            || (response.id != serde_json::json!(1) && response.id != serde_json::json!("1"))
        {
            return Err(SubmitError::InvalidResponse);
        }
        match (response.result, response.error) {
            (Some(result), None) if result.tx_hash == *transaction.tx_hash() => Ok(result.tx_hash),
            (Some(result), None) => Err(SubmitError::HashMismatch {
                expected: *transaction.tx_hash(),
                actual: result.tx_hash,
            }),
            (None, Some(error)) => Err(SubmitError::Rpc {
                code: error.code,
                message: error.message,
            }),
            _ => Err(SubmitError::InvalidResponse),
        }
    }
}

impl Submitter for BloxrouteSubmitter {
    fn check_chain(&self, chain_id: u64) -> Result<(), SubmitError> {
        if chain_id != crate::ROBINHOOD_CHAIN_ID {
            return Err(SubmitError::UnsupportedChain(chain_id));
        }
        Ok(())
    }

    fn submit<'a>(
        &'a self,
        transaction: &'a TxEnvelope,
    ) -> BoxFuture<'a, Result<B256, SubmitError>> {
        Box::pin(self.submit(transaction))
    }
}

#[derive(Serialize)]
struct SubmitRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: SubmitParams,
}

#[derive(Serialize)]
struct SubmitParams {
    transaction: String,
}

#[derive(Deserialize)]
struct SubmitResponse {
    jsonrpc: String,
    id: serde_json::Value,
    result: Option<SubmitResult>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct SubmitResult {
    #[serde(rename = "txHash")]
    tx_hash: B256,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_primitives::{Signature, U256};

    #[test]
    fn constructor_validates_host_and_authorization_without_requests() {
        for host in [
            "api.blxrbdn.com",
            "wss://api.blxrbdn.com/ws",
            "https://user:password@api.blxrbdn.com",
        ] {
            assert!(matches!(
                BloxrouteSubmitter::fast(host, "auth"),
                Err(SubmitError::InvalidHost)
            ));
        }
        for auth in ["", "secret\r\nInjected: header"] {
            assert!(matches!(
                BloxrouteSubmitter::fast("https://api.blxrbdn.com", auth),
                Err(SubmitError::InvalidAuthorization)
            ));
        }
        assert!(BloxrouteSubmitter::fast("https://api.blxrbdn.com", "auth").is_ok());
    }

    #[tokio::test]
    async fn another_chain_is_rejected_before_contacting_the_host() {
        let submitter = BloxrouteSubmitter::fast("http://127.0.0.1:1", "auth").unwrap();
        let transaction = TxEip1559 {
            chain_id: 1,
            ..Default::default()
        }
        .into_signed(Signature::new(U256::from(1), U256::from(1), false))
        .into();
        assert!(matches!(
            submitter.submit(&transaction).await,
            Err(SubmitError::UnsupportedChain(1))
        ));
    }
}
