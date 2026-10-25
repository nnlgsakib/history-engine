//! EVM history via Alchemy's Transfers API (`alchemy_getAssetTransfers`).
//!
//! One call per direction gives every coin and ERC-20 transfer from or to
//! an address, newest first, with block timestamps. Sends and receives are
//! separate queries (`fromAddress`, `toAddress`); the direction of a row is
//! still decided by the transfer's own `from`, so a payment to ourselves that
//! both queries return becomes one row.
//!
//! The endpoint is whatever the app supplies: the wallet's proxy, which holds
//! the key. This crate never sees a key.

use super::evm::{moved, row, Transfer};
use super::{clean, hex_to_dec, iso_secs, post_json, Read};
use crate::error::{HistoryError, HistoryResult};
use crate::model::NewEntry;
use registry::model::ChainInfo;
use serde::Deserialize;
use serde_json::{json, Value};

/// Name of this source in the store's cursor table.
pub(crate) const SOURCE: &str = "alchemy";

/// Transfers per page. Alchemy's default and documented page size.
const PAGE: &str = "0x3e8";

/// ponytail: at most 5 pages (5,000 transfers) per direction per refresh.
/// A first scan keeps the newest 5,000; a later scan that hits the cap
/// resumes from where it stopped on the next refresh, so nothing is skipped.
const MAX_PAGES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    From,
    To,
}

/// Refuses an endpoint template that could leak the address list or a key:
/// https only (plain http to this machine for development), and it must
/// name the network, since one template serves every chain.
pub(crate) fn check_template(template: &str) -> HistoryResult<()> {
    let refuse = |why: &str| Err(HistoryError::Source(format!("alchemy endpoint {why}")));
    if !template.contains("{network}") {
        return refuse("must contain {network}");
    }
    let Ok(url) = reqwest::Url::parse(&endpoint(template, "eth-mainnet")) else {
        return refuse("is not a URL");
    };
    match (url.scheme(), url.host_str()) {
        ("https", Some(_)) => Ok(()),
        ("http", Some("127.0.0.1" | "localhost")) => Ok(()),
        _ => refuse("must be https"),
    }
}

/// The endpoint for one network.
pub(crate) fn endpoint(template: &str, network: &str) -> String {
    template.replace("{network}", network)
}

/// The JSON-RPC request for one page of one direction. A first scan reads
/// newest first; a later one reads forward from its cursor (inclusive, so
/// a block half read last time is read again and deduplicated by id).
pub(crate) fn params(address: &str, direction: Direction, internal: bool, since: Option<u64>, page_key: Option<&str>) -> Value {
    let mut category = vec!["external", "erc20"];
    if internal {
        category.push("internal");
    }
    let mut query = json!({
        "fromBlock": since.map_or_else(|| "0x0".to_string(), |b| format!("0x{b:x}")),
        "toBlock": "latest",
        "category": category,
        "withMetadata": true,
        "excludeZeroValue": true,
        "maxCount": PAGE,
        "order": if since.is_some() { "asc" } else { "desc" },
    });
    let side = match direction {
        Direction::From => "fromAddress",
        Direction::To => "toAddress",
    };
    query[side] = json!(address);
    if let Some(key) = page_key {
        query["pageKey"] = json!(key);
    }
    json!({ "jsonrpc": "2.0", "id": 1, "method": "alchemy_getAssetTransfers", "params": [query] })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn read(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
    internal: bool,
    since: Option<u64>,
) -> HistoryResult<Read> {
    let address = address.to_lowercase();
    let (sent, received) = futures_util::future::join(
        direction(client, url, bearer, &address, Direction::From, internal, since),
        direction(client, url, bearer, &address, Direction::To, internal, since),
    )
    .await;

    let mut rows = Vec::new();
    let mut newest: Option<u64> = None;
    // The lowest point a capped direction reached: the next read starts there.
    let mut stop: Option<u64> = None;
    for (transfers, capped) in [sent?, received?] {
        let mut highest: Option<u64> = None;
        for t in transfers {
            let block = parse_block(&t.block_num)?;
            highest = highest.max(Some(block));
            if let Some(row) = map(wallet_id, chain, &address, t)? {
                rows.push(row);
            }
        }
        newest = newest.max(highest);
        if let (true, Some(h)) = (capped, highest) {
            stop = Some(stop.map_or(h, |s| s.min(h)));
        }
    }
    let cursor = match since {
        // Newest first: anything older than the cap is left out for good.
        None => newest,
        Some(since) => Some(stop.unwrap_or_else(|| newest.unwrap_or(since).max(since))),
    };
    Ok(Read { rows, cursor })
}

/// Every page of one direction, up to the cap. Returns whether it was capped.
async fn direction(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    address: &str,
    side: Direction,
    internal: bool,
    since: Option<u64>,
) -> HistoryResult<(Vec<WireTransfer>, bool)> {
    let mut out = Vec::new();
    let mut key: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let reply: RpcResponse = post_json(client, url, bearer, &params(address, side, internal, since, key.as_deref())).await?;
        if let Some(error) = reply.error {
            return Err(HistoryError::Source(format!("alchemy: {}", clean(&error.message, 200))));
        }
        let page = reply.result.ok_or_else(|| HistoryError::Source("alchemy: empty answer".into()))?;
        out.extend(page.transfers);
        match page.page_key.filter(|k| !k.is_empty()) {
            Some(next) => key = Some(next),
            None => return Ok((out, false)),
        }
    }
    Ok((out, true))
}

fn parse_block(hex: &str) -> HistoryResult<u64> {
    hex.strip_prefix("0x")
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .ok_or_else(|| HistoryError::Source("alchemy: bad block number".into()))
}

/// One transfer as a row. `Ok(None)` for what Activity does not show (NFTs,
/// zero amounts, tokens without decimals or symbol). A missing timestamp is
/// an error: a row with a made-up date would sort wrongly forever.
fn map(wallet_id: &str, chain: &ChainInfo, our_address: &str, t: WireTransfer) -> HistoryResult<Option<NewEntry>> {
    let native = matches!(t.category.as_str(), "external" | "internal");
    if !native && t.category != "erc20" {
        return Ok(None);
    }
    let at = t
        .metadata
        .as_ref()
        .and_then(|m| m.block_timestamp.as_deref())
        .and_then(iso_secs)
        .ok_or_else(|| HistoryError::Source("alchemy: a transfer came without its date".into()))?;
    let Some(amount) = t.raw.value.as_deref().and_then(hex_to_dec).filter(|a| moved(a)) else {
        return Ok(None);
    };
    let Some(hash) = t.hash.as_deref().map(|h| clean(h, 100)).filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let (token, symbol, decimals) = if native {
        let coin = chain.native_currency();
        (None, clean(&coin.symbol, 16), coin.decimals)
    } else {
        let Some(contract) = t.raw.address.as_deref().map(|a| clean(a, 100)).filter(|a| !a.is_empty()) else {
            return Ok(None);
        };
        // Unknown decimals cannot be rendered safely: dropped, not guessed.
        let Some(decimals) = t.raw.decimal.as_deref().and_then(hex_to_dec).and_then(|d| d.parse::<u8>().ok()) else {
            return Ok(None);
        };
        let symbol = clean(t.asset.as_deref().unwrap_or_default(), 16);
        if symbol.is_empty() {
            return Ok(None);
        }
        (Some(contract), symbol, decimals)
    };
    Ok(Some(row(
        wallet_id,
        chain,
        Transfer { hash, from: t.from, to: t.to, token, symbol, decimals, amount, at, our_address },
    )))
}

// ---- Alchemy wire types ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    result: Option<TransfersResult>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct TransfersResult {
    #[serde(default)]
    transfers: Vec<WireTransfer>,
    #[serde(default, rename = "pageKey")]
    page_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireTransfer {
    #[serde(rename = "blockNum")]
    block_num: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    /// Coin or token symbol; `null` when Alchemy does not know it.
    #[serde(default)]
    asset: Option<String>,
    category: String,
    #[serde(default, rename = "rawContract")]
    raw: RawContract,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Debug, Default, Deserialize)]
struct RawContract {
    /// Hex base units; `null` for NFTs.
    #[serde(default)]
    value: Option<String>,
    /// `null` for coin transfers.
    #[serde(default)]
    address: Option<String>,
    /// Hex decimals; `null` when unknown.
    #[serde(default)]
    decimal: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    #[serde(default, rename = "blockTimestamp")]
    block_timestamp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::fake;
    use crate::model::HistoryKind;
    use registry::model::EvmChain;

    const ME: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
    const HASH: &str = "0x3847245c01829b043431067fb2bfa95f7b5bdc7e4246c843e7a573ab6f26f5ff";

    fn chain() -> ChainInfo {
        ChainInfo::Evm(serde_json::from_str::<EvmChain>(include_str!("../../../registry/data/evm/1.json")).expect("fixture"))
    }

    #[allow(clippy::too_many_arguments)]
    fn transfer(block: u64, from: &str, to: &str, category: &str, value: &str, contract: Option<&str>, decimal: Option<&str>, asset: Option<&str>) -> Value {
        json!({
            "blockNum": format!("0x{block:x}"),
            "uniqueId": format!("{HASH}:{category}"),
            "hash": HASH,
            "from": from,
            "to": to,
            "value": 0.5,
            "asset": asset,
            "category": category,
            "rawContract": { "value": value, "address": contract, "decimal": decimal },
            "metadata": { "blockTimestamp": "2026-10-08T06:44:59.000Z" }
        })
    }

    fn coin(block: u64, from: &str, to: &str) -> Value {
        transfer(block, from, to, "external", "0x6f05b59d3b20000", None, Some("0x12"), Some("ETH"))
    }

    fn mapped(t: Value) -> HistoryResult<Option<NewEntry>> {
        map("w", &chain(), ME, serde_json::from_value(t).expect("wire shape"))
    }

    fn page(transfers: Vec<Value>, page_key: Option<&str>) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "transfers": transfers, "pageKey": page_key } }).to_string()
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn client() -> reqwest::Client {
        super::super::http_client("history-engine-tests").unwrap()
    }

    #[test]
    fn a_first_scan_asks_for_one_direction_newest_first() {
        let p = params(ME, Direction::From, true, None, None);
        assert_eq!(p["method"], "alchemy_getAssetTransfers");
        let q = &p["params"][0];
        assert_eq!(q["fromAddress"], ME);
        assert!(q.get("toAddress").is_none());
        assert_eq!(q["order"], "desc");
        assert_eq!(q["fromBlock"], "0x0");
        assert_eq!(q["toBlock"], "latest");
        assert_eq!(q["withMetadata"], true);
        assert_eq!(q["excludeZeroValue"], true);
        assert_eq!(q["maxCount"], "0x3e8");
        assert_eq!(q["category"], json!(["external", "erc20", "internal"]));
        assert!(q.get("pageKey").is_none());
    }

    #[test]
    fn a_later_scan_reads_forward_from_its_cursor() {
        let p = params(ME, Direction::To, false, Some(0x10), Some("k1"));
        let q = &p["params"][0];
        assert_eq!(q["toAddress"], ME);
        assert!(q.get("fromAddress").is_none());
        assert_eq!(q["order"], "asc");
        assert_eq!(q["fromBlock"], "0x10");
        assert_eq!(q["category"], json!(["external", "erc20"]), "internal only where the network has it");
        assert_eq!(q["pageKey"], "k1");
    }

    #[test]
    fn coin_received_uses_chain_decimals_and_the_block_date() {
        let row = mapped(coin(1, "0xSENDER", ME)).unwrap().expect("row");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.asset_id, "evm:1:native");
        assert_eq!(row.symbol, "ETH");
        assert_eq!(row.decimals, 18);
        assert_eq!(row.amount_base, "500000000000000000", "from the raw hex, never the float");
        assert_eq!(row.created_at, 1_791_441_899);
        assert_eq!(row.counterparty.as_deref(), Some("0xSENDER"));
        assert_eq!(row.to_address, ME);
        assert_eq!(row.tx_hash.as_deref(), Some(HASH));
    }

    #[test]
    fn token_sent_uses_the_contract_and_its_decimals() {
        let t = transfer(1, ME, "0xdest", "erc20", "0x2625a0", Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Some("0x6"), Some("USDC"));
        let row = mapped(t).unwrap().expect("row");
        assert_eq!(row.kind, HistoryKind::Sent);
        assert_eq!(row.asset_id, "evm:1:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        assert_eq!(row.symbol, "USDC");
        assert_eq!(row.decimals, 6);
        assert_eq!(row.amount_base, "2500000");
        assert_eq!(row.to_address, "0xdest");
    }

    #[test]
    fn coin_moved_by_a_contract_is_the_chain_coin() {
        let t = transfer(1, "0xcontract", ME, "internal", "0x5", None, None, Some("ETH"));
        let row = mapped(t).unwrap().expect("row");
        assert_eq!(row.asset_id, "evm:1:native");
        assert_eq!(row.decimals, 18);
        assert_eq!(row.amount_base, "5");
    }

    #[test]
    fn what_activity_does_not_show_makes_no_row() {
        let nft = transfer(1, "0xo", ME, "erc721", "0x1", Some("0xnft"), None, Some("PUNK"));
        let zero = transfer(1, "0xo", ME, "external", "0x0", None, Some("0x12"), Some("ETH"));
        let no_decimals = transfer(1, "0xo", ME, "erc20", "0x1", Some("0xt"), None, Some("TKN"));
        let no_symbol = transfer(1, "0xo", ME, "erc20", "0x1", Some("0xt"), Some("0x6"), None);
        let no_value = transfer(1, "0xo", ME, "erc20", "0x1", Some("0xt"), Some("0x6"), Some("TKN"));
        let mut no_value = no_value;
        no_value["rawContract"]["value"] = Value::Null;
        for t in [nft, zero, no_decimals, no_symbol, no_value] {
            assert!(mapped(t.clone()).unwrap().is_none(), "{t}");
        }
    }

    #[test]
    fn a_transfer_without_a_timestamp_is_an_error() {
        let mut t = coin(1, "0xo", ME);
        t["metadata"] = Value::Null;
        assert!(mapped(t).is_err());
    }

    /// Alchemy standing in for Blockscout, or the other way round, must not
    /// double a transfer.
    #[test]
    fn the_same_transfer_has_the_same_id_as_from_blockscout() {
        let row = mapped(coin(1, "0xsender", ME)).unwrap().unwrap();
        let expected = super::super::evm::transfer_id(HASH, "evm:1:native", HistoryKind::Received, "500000000000000000");
        assert_eq!(row.local_id, expected);
    }

    #[test]
    fn endpoint_templates_are_checked() {
        assert!(check_template("https://history.example.com/{network}").is_ok());
        assert!(check_template("http://127.0.0.1:8787/{network}").is_ok());
        assert!(check_template("http://localhost:8787/{network}").is_ok());
        assert!(check_template("http://history.example.com/{network}").is_err(), "plain http off this machine");
        assert!(check_template("http://localhost.example.com/{network}").is_err(), "the host, not a prefix of it");
        assert!(check_template("https://eth-mainnet.g.alchemy.com/v2/key").is_err(), "one network only");
        assert!(check_template("").is_err());
        assert_eq!(endpoint("https://h.example/{network}", "eth-mainnet"), "https://h.example/eth-mainnet");
    }

    #[test]
    fn a_first_scan_pages_both_directions_and_sets_the_cursor() {
        let server = fake::serve(|_path, body| {
            let q: Value = serde_json::from_str(body).unwrap();
            let q = &q["params"][0];
            let reply = match (q.get("fromAddress").is_some(), q["pageKey"].as_str()) {
                (true, None) => page(vec![coin(0x20, ME, "0xa")], Some("p2")),
                (true, Some("p2")) => page(vec![transfer(0x15, ME, "0xb", "erc20", "0x1", Some("0xt"), Some("0x6"), Some("TKN"))], None),
                (false, None) => page(vec![coin(0x18, "0xc", ME)], None),
                _ => page(vec![], None),
            };
            (200, reply)
        });
        let read = run(read(&client(), &server.url, None, "w", &chain(), ME, true, None)).expect("read");
        assert_eq!(read.rows.len(), 3);
        assert_eq!(read.rows.iter().filter(|r| r.kind == HistoryKind::Sent).count(), 2);
        assert_eq!(read.cursor, Some(0x20), "the newest block seen");
        assert_eq!(server.seen.lock().unwrap().len(), 3);
    }

    /// A later scan that hits the page cap in one direction must resume from
    /// there next time, not from the other direction's newer blocks.
    #[test]
    fn a_capped_later_scan_resumes_where_it_stopped() {
        let server = fake::serve(|_path, body| {
            let q: Value = serde_json::from_str(body).unwrap();
            let q = &q["params"][0];
            if q.get("fromAddress").is_some() {
                let n: u64 = q["pageKey"].as_str().map(|k| k.parse().unwrap()).unwrap_or(0);
                (200, page(vec![coin(0x20 + n, ME, "0xa")], Some(&(n + 1).to_string())))
            } else {
                (200, page(vec![coin(0x40, "0xc", ME)], None))
            }
        });
        let read = run(read(&client(), &server.url, None, "w", &chain(), ME, false, Some(0x20))).expect("read");
        assert_eq!(read.cursor, Some(0x20 + MAX_PAGES as u64 - 1));
    }

    #[test]
    fn a_later_scan_with_nothing_new_keeps_its_cursor() {
        let server = fake::serve(|_, _| (200, page(vec![], None)));
        let read = run(read(&client(), &server.url, None, "w", &chain(), ME, false, Some(0x30))).expect("read");
        assert!(read.rows.is_empty());
        assert_eq!(read.cursor, Some(0x30));
    }

    #[test]
    fn provider_failures_are_errors_not_empty_history() {
        let rpc_error = fake::serve(|_, _| (200, json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "limit exceeded" } }).to_string()));
        let err = run(read(&client(), &rpc_error.url, None, "w", &chain(), ME, false, None)).err().expect("error");
        assert!(err.to_string().contains("limit exceeded"), "{err}");

        let throttled = fake::serve(|_, _| (429, "{}".to_string()));
        assert!(run(read(&client(), &throttled.url, None, "w", &chain(), ME, false, None)).is_err());
    }
}
