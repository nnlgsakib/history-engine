//! Chain discovery: read what the chain did to this wallet.
//!
//! A seed yields addresses and keys, never history, so transfers in and out
//! cannot be derived locally. A provider or explorer is asked instead.
//!
//! Each chain lists its sources in the registry: providers that need an
//! endpoint (`history_providers`, e.g. Alchemy behind the wallet's proxy),
//! then its keyless `history_api`. They are tried in that order and the
//! first that answers is used. A chain with none is reported as unsupported
//! rather than silently returning nothing, so the UI can tell the user why.

use crate::chains;
use crate::error::{HistoryError, HistoryResult};
use crate::model::NewEntry;
use crate::store::HistoryStore;
use registry::model::ChainInfo;

/// One chain plus the address to scan for it.
#[derive(Debug, Clone)]
pub struct ScanTarget {
    pub wallet_id: String,
    pub chain: ChainInfo,
    /// The address to scan. Derived per chain; never the seed.
    pub address: String,
}

/// Endpoints for providers that need one. Each is a URL template containing
/// `{network}` (the provider's name for a chain, from the registry). Normally
/// the wallet's own proxy, which holds the provider's key: a key shipped in
/// an app can be copied out of it. A provider left `None` is skipped.
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    /// Alchemy Transfers API, e.g. `https://history.example.com/alchemy/{network}`.
    pub alchemy: Option<String>,
}

pub struct Discovery {
    client: reqwest::Client,
    endpoints: Endpoints,
    /// The app's session with its provider proxy, sent as a bearer token.
    bearer: std::sync::RwLock<Option<String>>,
}

impl Discovery {
    /// Keyless sources only. `user_agent` identifies the wallet to the
    /// explorers it reads, e.g. `"Nira Wallet/1.0 (+https://example.com)"`.
    pub fn new(user_agent: &str) -> HistoryResult<Discovery> {
        Self::with_endpoints(user_agent, Endpoints::default())
    }

    /// With provider endpoints. Refuses one that is not https or does not
    /// name the network.
    pub fn with_endpoints(user_agent: &str, endpoints: Endpoints) -> HistoryResult<Discovery> {
        if let Some(template) = &endpoints.alchemy {
            chains::alchemy::check_template(template)?;
        }
        Ok(Discovery { client: chains::http_client(user_agent)?, endpoints, bearer: Default::default() })
    }

    /// Sets the session sent to provider endpoints, or clears it.
    pub fn set_bearer(&self, token: Option<String>) {
        if let Ok(mut slot) = self.bearer.write() {
            *slot = token;
        }
    }

    /// False once a provider has refused the session: the app makes a new one.
    pub fn has_bearer(&self) -> bool {
        self.bearer.read().map(|b| b.is_some()).unwrap_or(false)
    }

    /// True when this chain has a source this instance can read.
    pub fn supports(&self, chain: &ChainInfo) -> bool {
        !chains::sources(chain, &self.endpoints).is_empty()
    }

    /// Reads a chain's recent history for one address, without storing it.
    pub async fn scan(&self, target: &ScanTarget) -> HistoryResult<Vec<NewEntry>> {
        Ok(self.read(target, None).await?.1.rows)
    }

    /// Reads what is new since the last sync and stores it. Returns how many
    /// rows were added. The source's cursor moves only after every row is
    /// stored, so a failure part way re-reads rather than skips.
    pub async fn sync(&self, store: &HistoryStore, target: &ScanTarget) -> HistoryResult<usize> {
        let (cursor_key, read) = self.read(target, Some(store)).await?;
        let mut added = 0;
        for row in &read.rows {
            if store.record(row, true)? {
                added += 1;
            }
        }
        if let (Some(key), Some(block)) = (cursor_key, read.cursor) {
            store.set_cursor(&target.wallet_id, target.chain.id(), key, block)?;
        }
        Ok(added)
    }

    /// The first source that answers. When all fail, the first one's error:
    /// the preferred source's failure is the one worth showing.
    async fn read(&self, target: &ScanTarget, store: Option<&HistoryStore>) -> HistoryResult<(Option<&'static str>, chains::Read)> {
        let mut first_error = None;
        for source in chains::sources(&target.chain, &self.endpoints) {
            let since = match (store, source.cursor_key()) {
                (Some(store), Some(key)) => store.cursor(&target.wallet_id, target.chain.id(), key)?,
                _ => None,
            };
            let bearer = self.bearer.read().ok().and_then(|b| b.clone());
            match chains::read_source(&self.client, &source, bearer.as_deref(), &target.wallet_id, &target.chain, &target.address, since).await {
                Ok(read) => return Ok((source.cursor_key(), read)),
                Err(e) => {
                    if matches!(e, HistoryError::Unauthorized) {
                        self.set_bearer(None);
                    }
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| HistoryError::Unsupported(target.chain.id().to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::fake;
    use crate::store::HistoryStore;
    use registry::model::{EvmChain, HistoryApi};
    use serde_json::{json, Value};

    const ME: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

    fn ethereum(blockscout: Option<&str>) -> ChainInfo {
        let mut chain: EvmChain = serde_json::from_str(include_str!("../../registry/data/evm/1.json")).unwrap();
        chain.history_api = blockscout.map(|base_url| HistoryApi::Blockscout { base_url: base_url.into() });
        ChainInfo::Evm(chain)
    }

    fn target(chain: ChainInfo) -> ScanTarget {
        ScanTarget { wallet_id: "w".into(), chain, address: ME.into() }
    }

    fn with_alchemy(url: &str) -> Discovery {
        Discovery::with_endpoints("t", Endpoints { alchemy: Some(format!("{url}/{{network}}")) }).unwrap()
    }

    fn incoming(block: u64) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "transfers": [{
            "blockNum": format!("0x{block:x}"),
            "hash": "0x01",
            "from": "0xsender",
            "to": ME,
            "asset": "ETH",
            "category": "external",
            "rawContract": { "value": "0x10", "address": null, "decimal": "0x12" },
            "metadata": { "blockTimestamp": "2026-10-08T06:44:59.000Z" }
        }] } })
        .to_string()
    }

    fn empty() -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "transfers": [] } }).to_string()
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn sync_stores_new_rows_once_and_moves_the_cursor() {
        let server = fake::serve(|_, body| {
            let q: Value = serde_json::from_str(body).unwrap();
            (200, if q["params"][0].get("toAddress").is_some() { incoming(0x20) } else { empty() })
        });
        let discovery = with_alchemy(&server.url);
        let store = HistoryStore::open_in_memory().unwrap();
        let t = target(ethereum(None));

        assert_eq!(run(discovery.sync(&store, &t)).unwrap(), 1);
        assert_eq!(store.cursor("w", "ethereum", "alchemy").unwrap(), Some(0x20));
        let rows = store.list("w", None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].discovered);

        assert_eq!(run(discovery.sync(&store, &t)).unwrap(), 0, "nothing new the second time");
        let seen = server.seen.lock().unwrap();
        let (path, body) = seen.last().unwrap();
        assert_eq!(path, "/eth-mainnet", "the chain's own Alchemy network");
        let q: Value = serde_json::from_str(body).unwrap();
        assert_eq!(q["params"][0]["fromBlock"], "0x20", "reads on from the cursor");
        assert_eq!(q["params"][0]["order"], "asc");
    }

    #[test]
    fn sync_falls_back_to_the_keyless_source_when_the_provider_fails() {
        let server = fake::serve(|path, _| {
            if path.starts_with("/eth-mainnet") {
                return (500, "{}".into());
            }
            let items = if path.contains("/transactions") {
                json!([{ "hash": "0x02", "from": { "hash": "0xsender" }, "to": { "hash": ME },
                         "value": "7", "status": "ok", "timestamp": "2026-10-08T06:44:59Z" }])
            } else {
                json!([])
            };
            (200, json!({ "items": items }).to_string())
        });
        let discovery = with_alchemy(&server.url);
        let store = HistoryStore::open_in_memory().unwrap();

        assert_eq!(run(discovery.sync(&store, &target(ethereum(Some(&server.url))))).unwrap(), 1);
        assert_eq!(store.cursor("w", "ethereum", "alchemy").unwrap(), None, "a failed read moves nothing");
        assert_eq!(store.list("w", None, 10).unwrap()[0].amount_base, "7");
    }

    #[test]
    fn when_every_source_fails_the_error_is_reported_without_the_url() {
        let server = fake::serve(|_, _| (500, "{}".into()));
        let discovery = with_alchemy(&server.url);
        let store = HistoryStore::open_in_memory().unwrap();
        let err = run(discovery.sync(&store, &target(ethereum(Some(&server.url))))).unwrap_err().to_string();
        assert!(err.contains("500"), "{err}");
        assert!(!err.contains("eth-mainnet"), "an endpoint path can hold a key: {err}");
        assert!(store.list("w", None, 10).unwrap().is_empty());
    }

    #[test]
    fn a_provider_without_an_endpoint_is_skipped() {
        let chain = ethereum(None);
        let keyless = Discovery::new("t").unwrap();
        assert!(!keyless.supports(&chain));
        let store = HistoryStore::open_in_memory().unwrap();
        assert!(matches!(run(keyless.sync(&store, &target(chain.clone()))), Err(HistoryError::Unsupported(_))));
        assert!(with_alchemy("https://history.example.com").supports(&chain));
        assert!(keyless.supports(&ethereum(Some("https://eth.blockscout.com"))));
    }

    #[test]
    fn a_bad_endpoint_template_is_refused() {
        let bad = Endpoints { alchemy: Some("http://history.example.com/{network}".into()) };
        assert!(Discovery::with_endpoints("t", bad).is_err());
    }

    /// The app's session for the provider's proxy is sent as a bearer token.
    #[test]
    fn the_session_is_sent_to_the_provider() {
        let server = fake::serve(|_, _| (200, empty()));
        let discovery = with_alchemy(&server.url);
        discovery.set_bearer(Some("session-1".into()));
        assert!(discovery.has_bearer());
        run(discovery.sync(&HistoryStore::open_in_memory().unwrap(), &target(ethereum(None)))).unwrap();
        let auth = server.auth.lock().unwrap();
        assert_eq!(auth.len(), 2, "both directions");
        assert!(auth.iter().all(|a| a.as_deref() == Some("Bearer session-1")));
    }

    /// A refused session is dropped, so the app makes a new one, and the
    /// keyless source still fills the feed meanwhile.
    #[test]
    fn a_refused_session_is_dropped_and_the_keyless_source_answers() {
        let server = fake::serve(|path, _| {
            if path.starts_with("/eth-mainnet") {
                (401, "{}".into())
            } else {
                (200, json!({ "items": [] }).to_string())
            }
        });
        let discovery = with_alchemy(&server.url);
        discovery.set_bearer(Some("old".into()));
        let store = HistoryStore::open_in_memory().unwrap();
        assert!(run(discovery.sync(&store, &target(ethereum(Some(&server.url))))).is_ok());
        assert!(!discovery.has_bearer(), "the app is told to attest again");

        discovery.set_bearer(Some("old".into()));
        let err = run(discovery.sync(&store, &target(ethereum(None)))).unwrap_err();
        assert!(matches!(err, HistoryError::Unauthorized), "{err}");
    }
}
