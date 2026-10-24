//! Chain-specific history readers.
//!
//! Each chain family gets its own module because the semantics genuinely
//! differ, not just the URL:
//!
//! - [`evm`] — account model. Balances are readable at any time, so a
//!   transaction's direction is decided by whether our address is the sender.
//!   Token movements and plain coin transfers come from *separate* endpoints.
//! - [`bitcoin`] — UTXO model. There is no "balance change" to read; direction
//!   and amount must be derived from inputs and outputs per transaction, and a
//!   single transaction can both send and receive.
//! - [`solana`] — no explorer at all: the node itself indexes by account, and
//!   what a transaction did is read from balances before and after it.

pub mod bitcoin;
pub mod evm;
pub mod solana;

use crate::error::{HistoryResult};
use crate::model::NewEntry;
use registry::model::ChainInfo;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Shared HTTP client. One per discovery run: connections are reused, and
/// public explorer endpoints rate-limit per-connection. `user_agent` names the
/// wallet making the calls, so explorers can tell one wallet's traffic from
/// another's.
pub(crate) fn http_client(user_agent: &str) -> HistoryResult<reqwest::Client> {
    reqwest::Client::builder()
        // Several addresses per name, some unreachable here (IPv6 with no
        // route): each gets a slice of this, then the next is tried.
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(HTTP_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .map_err(|_| crate::error::HistoryError::Source("http client failed".into()))
}

/// Async JSON GET. Explorer responses are untrusted, so every call site parses
/// into an explicit wire type rather than `serde_json::Value`.
///
/// Parse failures keep serde's message: silently returning an empty list here
/// would show a plausible-looking but incomplete activity feed, which is worse
/// than an error.
pub(crate) async fn get_json<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> HistoryResult<T> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| crate::error::HistoryError::Source(format!("request failed: {e}")))?
        .json::<T>()
        .await
        .map_err(|e| crate::error::HistoryError::Source(format!("bad response: {e}")))
}

/// Explorers are inconsistent about numeric encodings — Blockscout sends
/// `decimals` as the string `"9"` while Esplora sends `value` as a number.
/// Accept both rather than dropping the whole page over one field's type.
pub(crate) fn lenient_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Num {
        Int(u64),
        Str(String),
        Null,
    }
    match <Num as serde::Deserialize>::deserialize(deserializer)? {
        Num::Int(v) => Ok(Some(v)),
        Num::Str(s) => Ok(s.trim().parse::<u64>().ok()),
        Num::Null => Ok(None),
    }
}

/// Same tolerance for `u8` fields such as token decimals.
pub(crate) fn lenient_u8<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64(deserializer)?.and_then(|v| u8::try_from(v).ok()))
}

/// Explorer strings are untrusted: length-capped, control characters stripped.
pub(crate) fn clean(value: &str, max: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

// Coarse calendar arithmetic. Only used to give discovered rows a plausible
// timestamp for display and ordering; no arithmetic is done with it, and SQL
// ordering is by `created_at DESC`.
const SECS_PER_DAY: i64 = 86_400;
const SECS_PER_MONTH: i64 = 2_592_000;
const SECS_PER_YEAR: i64 = 31_536_000;

/// Parses an ISO-8601 date from an explorer timestamp, falling back to now.
pub(crate) fn epoch(timestamp: &Option<String>) -> i64 {
    timestamp
        .as_deref()
        .and_then(|t| t.split('T').next())
        .and_then(|d| {
            let mut parts = d.split('-');
            let y: i64 = parts.next()?.parse().ok()?;
            let m: i64 = parts.next()?.parse().ok()?;
            let day: i64 = parts.next()?.parse().ok()?;
            Some(y * SECS_PER_YEAR + m * SECS_PER_MONTH + day * SECS_PER_DAY)
        })
        .unwrap_or_else(crate::model::now_secs)
}

/// Reads one chain's history. Dispatches to the right module by chain kind.
pub async fn read_chain(
    client: &reqwest::Client,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
) -> HistoryResult<Vec<NewEntry>> {
    match chain {
        ChainInfo::Evm(_) => evm::read(client, wallet_id, chain, address).await,
        ChainInfo::Bitcoin(_) => bitcoin::read(client, wallet_id, chain, address).await,
        ChainInfo::Solana(_) => solana::read(client, wallet_id, chain, address).await,
    }
}

/// True when this chain can report incoming activity at all.
pub fn supported(chain: &ChainInfo) -> bool {
    chain.history_api().is_some()
}