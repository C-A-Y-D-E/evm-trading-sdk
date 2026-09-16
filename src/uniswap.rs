pub mod v2;
pub mod v3;
pub mod v4;

use alloy_primitives::{
    Address, B256, Bytes, address,
    aliases::{I24, U24},
};

use crate::dex::{Currency, Error};

#[derive(Clone, Debug)]
pub struct UniswapDeployment {
    pub chain_id: u64,
    pub v2: Option<V2Deployment>,
    pub v3: Option<V3Deployment>,
    pub v4: Option<V4Deployment>,
}

impl UniswapDeployment {
    /// Configures the verified Robinhood mainnet deployments.
    pub fn robinhood_mainnet() -> Self {
        Self {
            chain_id: crate::ROBINHOOD_CHAIN_ID,
            v2: Some(V2Deployment {
                factory: address!("8bceaa40b9acdfaedf85adf4ff01f5ad6517937f"),
                router_02: address!("89e5db8b5aa49aa85ac63f691524311aeb649eba"),
                deployment_block: 8928,
            }),
            v3: Some(V3Deployment {
                factory: address!("1f7d7550b1b028f7571e69a784071f0205fd2efa"),
                swap_router_02: address!("caf681a66d020601342297493863e78c959e5cb2"),
                quoter_v2: address!("33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7"),
                deployment_block: 8930,
            }),
            v4: Some(V4Deployment {
                pool_manager: address!("8366a39cc670b4001a1121b8f6a443a643e40951"),
                universal_router: address!("8876789976decbfcbbbe364623c63652db8c0904"),
                quoter: address!("8dc178efb8111bb0973dd9d722ebeff267c98f94"),
                state_view: address!("f3334192d15450cdd385c8b70e03f9a6bd9e673b"),
                permit2: address!("000000000022d473030f116ddee9f6b43ac78ba3"),
                deployment_block: 9070,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct V2Deployment {
    pub factory: Address,
    pub router_02: Address,
    pub deployment_block: u64,
}

#[derive(Clone, Debug)]
pub struct V3Deployment {
    pub factory: Address,
    pub swap_router_02: Address,
    pub quoter_v2: Address,
    pub deployment_block: u64,
}

#[derive(Clone, Debug)]
pub struct V4Deployment {
    pub pool_manager: Address,
    pub universal_router: Address,
    pub quoter: Address,
    pub state_view: Address,
    pub permit2: Address,
    pub deployment_block: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolInput {
    Address(Address),
    V4Key { manager: Address, key: V4PoolKey },
    V4Id { manager: Address, id: B256 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UniswapPool {
    V2(V2Pool),
    V3(V3Pool),
    V4(V4Pool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V2Pool {
    pub chain_id: u64,
    pub factory: Address,
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V3Pool {
    pub chain_id: u64,
    pub factory: Address,
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: U24,
    pub tick_spacing: I24,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4Pool {
    pub chain_id: u64,
    pub manager: Address,
    pub key: V4PoolKey,
}

/// V4 uses the zero address for native currency in its on-chain pool key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: U24,
    pub tick_spacing: I24,
    pub hooks: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UniswapQuoteOptions {
    V2,
    V3,
    V4 { hook_data: Bytes },
}

#[derive(Clone, Debug)]
pub struct PoolQuery {
    pub chain_id: u64,
    pub currency_a: Currency,
    pub currency_b: Currency,
    pub v3_fee_tiers: Vec<U24>,
    pub v4_from_block: u64,
}

/// Results cover only the supplied fee tiers and event range, ending at the pinned block.
#[derive(Debug)]
pub struct PoolSearch {
    pub query: PoolQuery,
    pub block_hash: B256,
    pub v2: DiscoveryOutcome<V2Pool>,
    pub v3: DiscoveryOutcome<V3Pool>,
    pub v4: DiscoveryOutcome<V4Pool>,
}

#[derive(Debug)]
pub enum DiscoveryOutcome<P> {
    NotConfigured,
    Searched {
        pools: Vec<P>,
        coverage: SearchCoverage,
        failures: Vec<DiscoveryFailure>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchScope {
    V2Pair,
    V3FeeTier(U24),
    V4Blocks { from: u64, to: u64 },
}

/// Block ranges are inclusive. Completed ranges may be pages of a requested range.
#[derive(Clone, Debug)]
pub struct SearchCoverage {
    pub requested: Vec<SearchScope>,
    pub completed: Vec<SearchScope>,
}

#[derive(Debug)]
pub struct DiscoveryFailure {
    pub scope: SearchScope,
    pub error: Error,
}
