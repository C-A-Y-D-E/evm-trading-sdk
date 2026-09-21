pub mod dex;
pub mod execution;
pub mod fee_router;
pub mod submit;
pub mod uniswap;

use alloy_provider::{Provider, transport::TransportError};
use alloy_rpc_types_eth::{BlockId, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use futures_util::try_join;

pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use dex::{Dex, DiscoverPools, Error, ResolvePool, Result};
pub use fee_router::{
    BuyWith, FeeRouter, FundingQuoteFailure, FundingReport, PairSwap, PreparedSwap, PriceImpact,
    PriceImpactUnavailable, RouteHop, TradeSide, Trader, WalletTrader,
};
pub use submit::{BloxrouteSubmitter, SubmitError, Submitter};

use dex::{Currency, Token};

pub const ROBINHOOD_CHAIN_ID: u64 = 4663;
pub const ROBINHOOD_TESTNET_CHAIN_ID: u64 = 46630;

#[derive(Clone)]
pub struct EvmClient<P> {
    provider: P,
    chain_id: u64,
}

impl<P: Provider> EvmClient<P> {
    pub async fn connect(provider: P, chain_id: u64) -> Result<Self> {
        let actual = provider
            .get_chain_id()
            .await
            .map_err(|error| Error::Rpc(Box::new(error)))?;
        if actual != chain_id {
            return Err(Error::ChainMismatch {
                expected: chain_id,
                actual,
            });
        }
        Ok(Self { provider, chain_id })
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Missing or malformed metadata stays optional; RPC failures remain errors.
    pub async fn token_info(&self, address: Address, block: B256) -> Result<Token> {
        self.require_contract(address, block).await?;
        let (name, symbol, decimals) = try_join!(
            self.metadata_text(address, Erc20::nameCall {}, block),
            self.metadata_text(address, Erc20::symbolCall {}, block),
            self.optional_call(address, Erc20::decimalsCall {}, block),
        )?;
        let decimals =
            decimals.and_then(|data| Erc20::decimalsCall::abi_decode_returns_validate(&data).ok());
        Ok(Token {
            chain_id: self.chain_id,
            address,
            name,
            symbol,
            decimals,
        })
    }

    pub async fn balance(&self, currency: Currency, owner: Address, block: B256) -> Result<U256> {
        match currency {
            Currency::Native => self
                .provider
                .get_balance(owner)
                .block_id(BlockId::hash_canonical(block))
                .await
                .map_err(|error| Error::Rpc(Box::new(error))),
            Currency::Erc20(address) => {
                self.call(address, Erc20::balanceOfCall { owner }, block)
                    .await
            }
        }
    }

    pub async fn allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
        block: B256,
    ) -> Result<U256> {
        self.call(token, Erc20::allowanceCall { owner, spender }, block)
            .await
    }

    pub(crate) async fn call<C: SolCall>(
        &self,
        address: Address,
        call: C,
        block: B256,
    ) -> Result<C::Return> {
        let data = self.request(address, call, block).await?;
        C::abi_decode_returns_validate(&data).map_err(|source| Error::InvalidResponse {
            address,
            method: C::SIGNATURE,
            source,
        })
    }

    pub(crate) async fn require_contract(&self, address: Address, block: B256) -> Result<()> {
        let code = self
            .provider
            .get_code_at(address)
            .block_id(BlockId::hash_canonical(block))
            .await
            .map_err(|error| Error::Rpc(Box::new(error)))?;
        if code.is_empty() {
            return Err(Error::ContractNotFound(address));
        }
        Ok(())
    }

    async fn metadata_text<C: SolCall<Return = String>>(
        &self,
        address: Address,
        call: C,
        block: B256,
    ) -> Result<Option<String>> {
        let Some(data) = self.optional_call(address, call, block).await? else {
            return Ok(None);
        };
        Ok(C::abi_decode_returns_validate(&data)
            .ok()
            .or_else(|| legacy_text(&data)))
    }

    async fn optional_call<C: SolCall>(
        &self,
        address: Address,
        call: C,
        block: B256,
    ) -> Result<Option<Bytes>> {
        match self.request(address, call, block).await {
            Ok(data) => Ok(Some(data)),
            Err(Error::ContractReverted { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn request<C: SolCall>(&self, address: Address, call: C, block: B256) -> Result<Bytes> {
        let transaction = TransactionRequest {
            to: Some(address.into()),
            input: call.abi_encode().into(),
            ..Default::default()
        };
        self.provider
            .call(transaction)
            .block(BlockId::hash_canonical(block))
            .await
            .map_err(|error| contract_call_error(address, C::SIGNATURE, error))
    }
}

pub(crate) fn contract_call_error(
    address: Address,
    method: &'static str,
    error: TransportError,
) -> Error {
    if let Some(response) = error
        .as_error_resp()
        .filter(|response| response.code == 3 || response.message.starts_with("execution reverted"))
    {
        let reason = response.message.to_string();
        let revert_data = response
            .as_revert_data()
            .or_else(|| response.try_data_as::<Bytes>().and_then(|data| data.ok()))
            .unwrap_or_default();
        return Error::ContractReverted {
            address,
            method,
            reason,
            revert_data,
            source: Box::new(error),
        };
    }
    Error::Rpc(Box::new(error))
}

// Older tokens sometimes return a padded bytes32 instead of an ABI string.
fn legacy_text(data: &[u8]) -> Option<String> {
    if data.len() != 32 {
        return None;
    }
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    if data[end..].iter().any(|byte| *byte != 0) {
        return None;
    }
    std::str::from_utf8(&data[..end]).ok().map(str::to_owned)
}

sol! {
    interface Erc20 {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

#[cfg(test)]
mod tests {
    use alloy_provider::{ProviderBuilder, mock::Asserter};
    use alloy_sol_types::SolValue;

    use super::*;

    const BLOCK: B256 = B256::repeat_byte(0x11);
    const TOKEN: Address = Address::repeat_byte(0x22);

    fn code_response(asserter: &Asserter) {
        asserter.push_success(&Bytes::from_static(&[0x60, 0x00]));
    }

    #[tokio::test]
    async fn rejects_a_provider_on_another_chain() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let result = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID).await;
        assert!(matches!(
            result,
            Err(Error::ChainMismatch {
                expected: 4663,
                actual: 1
            })
        ));
    }

    #[tokio::test]
    async fn reads_metadata_without_assuming_eighteen_decimals() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        code_response(&asserter);
        asserter.push_success(&Bytes::from("Custom Dollar".to_owned().abi_encode()));
        asserter.push_success(&Bytes::from("CUSD".to_owned().abi_encode()));
        asserter.push_success(&Bytes::from(Erc20::decimalsCall::abi_encode_returns(&6)));

        let token = client.token_info(TOKEN, BLOCK).await.unwrap();

        assert_eq!(token.name.as_deref(), Some("Custom Dollar"));
        assert_eq!(token.symbol.as_deref(), Some("CUSD"));
        assert_eq!(token.decimals, Some(6));
        assert_eq!(token.chain_id, ROBINHOOD_CHAIN_ID);
    }

    #[tokio::test]
    async fn keeps_missing_metadata_optional() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        code_response(&asserter);
        asserter.push_success(&Bytes::new());
        asserter.push_success(&Bytes::new());
        asserter.push_success(&Bytes::new());

        let token = client.token_info(TOKEN, BLOCK).await.unwrap();

        assert!(token.name.is_none());
        assert!(token.symbol.is_none());
        assert!(token.decimals.is_none());
    }

    #[tokio::test]
    async fn keeps_reverted_metadata_optional() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        code_response(&asserter);
        for (code, message) in [
            (-32000, "execution reverted: no name"),
            (3, "contract rejected call"),
            (-32603, "execution reverted"),
        ] {
            asserter.push_failure(
                serde_json::from_value(serde_json::json!({
                    "code": code,
                    "message": message,
                    "data": "0x",
                }))
                .unwrap(),
            );
        }

        let token = client.token_info(TOKEN, BLOCK).await.unwrap();

        assert!(token.name.is_none());
        assert!(token.symbol.is_none());
        assert!(token.decimals.is_none());
    }

    #[tokio::test]
    async fn retains_revert_data_without_a_revert_message() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        asserter.push_failure(
            serde_json::from_value(serde_json::json!({
                "code": 3,
                "message": "contract rejected call",
                "data": "0xdeadbeef",
            }))
            .unwrap(),
        );

        let error = client
            .allowance(
                TOKEN,
                Address::repeat_byte(3),
                Address::repeat_byte(4),
                BLOCK,
            )
            .await
            .unwrap_err();

        let Error::ContractReverted {
            address,
            method,
            reason,
            revert_data,
            ..
        } = error
        else {
            panic!("contract reverts must remain distinct from RPC failures");
        };
        assert_eq!(address, TOKEN);
        assert_eq!(method, Erc20::allowanceCall::SIGNATURE);
        assert_eq!(reason, "contract rejected call");
        assert_eq!(revert_data.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[tokio::test]
    async fn preserves_metadata_rpc_failures() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        code_response(&asserter);
        asserter.push_failure_msg("rate limit exceeded");

        assert!(matches!(
            client.token_info(TOKEN, BLOCK).await,
            Err(Error::Rpc(_))
        ));
    }

    #[tokio::test]
    async fn reads_balances_and_allowances_in_base_units() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1237");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let client = EvmClient::connect(provider, ROBINHOOD_CHAIN_ID)
            .await
            .unwrap();
        let owner = Address::repeat_byte(0x33);
        let spender = Address::repeat_byte(0x44);
        asserter.push_success(&"0x2a");
        asserter.push_success(&Bytes::from(U256::from(123).abi_encode()));
        asserter.push_success(&Bytes::from(U256::from(77).abi_encode()));

        assert_eq!(
            client
                .balance(Currency::Native, owner, BLOCK)
                .await
                .unwrap(),
            U256::from(42)
        );
        assert_eq!(
            client
                .balance(Currency::Erc20(TOKEN), owner, BLOCK)
                .await
                .unwrap(),
            U256::from(123)
        );
        assert_eq!(
            client
                .allowance(TOKEN, owner, spender, BLOCK)
                .await
                .unwrap(),
            U256::from(77)
        );
    }

    #[test]
    fn reads_legacy_padded_symbols() {
        let mut data = [0; 32];
        data[..3].copy_from_slice(b"OLD");
        assert_eq!(legacy_text(&data).as_deref(), Some("OLD"));
        data[7] = b'X';
        assert_eq!(legacy_text(&data), None);
    }
}
