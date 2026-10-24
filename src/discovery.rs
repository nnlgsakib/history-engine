//! Chain discovery: read what the chain did to this wallet.
//!
//! A seed yields addresses and keys, never history, so incoming transfers
//! cannot be derived locally. We ask a chain explorer instead.
//!
//! Chain-specific behaviour lives in [`chains`], dispatched by chain kind.
//! Every chain declares its source in the registry; a chain with no
//! `history_api` is reported as unsupported rather than silently returning
//! nothing, so the UI can tell the user why.

use crate::chains;
use crate::error::{HistoryResult};
use crate::model::NewEntry;
use registry::model::ChainInfo;

/// One chain plus the address to scan for it.
#[derive(Debug, Clone)]
pub struct ScanTarget {
    pub wallet_id: String,
    pub chain: ChainInfo,
    /// The address to scan. Derived per chain; never the seed.
    pub address: String,
}

pub struct Discovery {
    client: reqwest::Client,
}

impl Discovery {
    /// `user_agent` identifies the wallet to the explorers it reads, e.g.
    /// `"Nira Wallet/1.0 (+https://example.com)"`.
    pub fn new(user_agent: &str) -> HistoryResult<Discovery> {
        Ok(Discovery {
            client: chains::http_client(user_agent)?,
        })
    }

    /// Reads a chain's history for one address.
    ///
    /// Returns rows for the caller to persist. Deduplication happens at the
    /// store level (by `(wallet_id, tx_hash)`), so overlapping results are
    /// harmless.
    pub async fn scan(&self, target: &ScanTarget) -> HistoryResult<Vec<NewEntry>> {
        chains::read_chain(
            &self.client,
            &target.wallet_id,
            &target.chain,
            &target.address,
        )
        .await
    }

    /// True when this chain can report incoming activity at all.
    pub fn supported(chain: &ChainInfo) -> bool {
        chains::supported(chain)
    }
}