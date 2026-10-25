//! EVM history via Blockscout, the keyless source.
//!
//! Account model: a transfer's direction is whether our address sent it.
//!
//! Blockscout splits history across **three** endpoints that do not overlap:
//!
//! - `transactions` — top-level transactions. Every ordinary coin payment,
//!   in or out, is one of these.
//! - `internal-transactions` — coin moved by a contract (a withdrawal, a
//!   bridge payout). Inbound only (`filter=to`).
//! - `token-transfers` — ERC-20 movements.
//!
//! Reading only the last two looks like it works while missing every plain
//! deposit, so all three are read.

use super::{clean, epoch, get_json};
use crate::error::{HistoryError, HistoryResult};
use crate::model::{HistoryKind, HistoryStatus, NewEntry};
use registry::model::{ChainInfo, HistoryApi};
use serde::Deserialize;

/// Blockscout returns 50 per page. One page per endpoint is the recent
/// history; deeper history needs a source with a cursor (Alchemy).
const MAX_ROWS: usize = 50;

pub async fn read(
    client: &reqwest::Client,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
) -> HistoryResult<Vec<NewEntry>> {
    let HistoryApi::Blockscout { base_url } = chain
        .history_api()
        .ok_or_else(|| HistoryError::Unsupported(chain.id().to_string()))?
    else {
        return Err(HistoryError::Unsupported(chain.id().to_string()));
    };

    let base = base_url.trim_end_matches('/');
    let addr = address.to_lowercase();

    // Every endpoint is required, and every failure is surfaced. Swallowing
    // one would show a feed missing a whole kind of transfer while looking
    // entirely plausible.
    let (txs, natives, tokens) = futures_util::future::join3(
        get_json::<TxPage>(client, &format!("{base}/api/v2/addresses/{addr}/transactions")),
        get_json::<InternalPage>(client, &format!("{base}/api/v2/addresses/{addr}/internal-transactions?filter=to")),
        get_json::<TokenPage>(client, &format!("{base}/api/v2/addresses/{addr}/token-transfers")),
    )
    .await;

    let mut out: Vec<NewEntry> = txs?
        .items
        .into_iter()
        .take(MAX_ROWS)
        .filter_map(|row| map_tx(wallet_id, chain, address, row))
        .collect();
    out.extend(natives?.items.into_iter().take(MAX_ROWS).filter_map(|row| map_native(wallet_id, chain, address, row)));
    out.extend(tokens?.items.into_iter().take(MAX_ROWS).filter_map(|row| map_token(wallet_id, chain, address, row)));
    Ok(out)
}

/// A transfer's id, the same whichever source read it, so one source can
/// stand in for another without doubling rows.
///
/// ponytail: two identical transfers (same asset, direction and amount) in
/// one transaction read as one row. Rare; key on the log index if every
/// source ever exposes it the same way.
pub(crate) fn transfer_id(hash: &str, asset_id: &str, kind: HistoryKind, amount: &str) -> String {
    format!("{}:{asset_id}:{}:{amount}", hash.to_lowercase(), kind.as_str())
}

/// The start of an asset id on this chain, as the wallet writes them:
/// `evm:{chain id}`. A row filed under anything else is never found by the
/// coin's own page, which looks its history up by that id.
pub(crate) fn asset_prefix(chain: &ChainInfo) -> String {
    match chain {
        ChainInfo::Evm(evm) => format!("evm:{}", evm.chain_id),
        other => other.id().to_string(),
    }
}

/// What every EVM source knows about one transfer, before it becomes a row.
pub(crate) struct Transfer<'a> {
    pub hash: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Token contract; `None` for the chain's coin.
    pub token: Option<String>,
    pub symbol: String,
    pub decimals: u8,
    /// Base units, decimal digits.
    pub amount: String,
    pub at: i64,
    pub our_address: &'a str,
}

/// One row from a transfer. Direction is whether we sent it; a payment to
/// ourselves reads as sent.
pub(crate) fn row(wallet_id: &str, chain: &ChainInfo, t: Transfer<'_>) -> NewEntry {
    let outgoing = t.from.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(t.our_address));
    let kind = if outgoing { HistoryKind::Sent } else { HistoryKind::Received };
    let asset_id = match &t.token {
        Some(contract) => format!("{}:{}", asset_prefix(chain), contract.to_lowercase()),
        None => format!("{}:native", asset_prefix(chain)),
    };
    let (to_address, counterparty) = if outgoing {
        (t.to.clone().unwrap_or_default(), t.to)
    } else {
        (t.our_address.to_string(), t.from)
    };
    NewEntry {
        wallet_id: wallet_id.to_string(),
        local_id: transfer_id(&t.hash, &asset_id, kind, &t.amount),
        kind,
        status: HistoryStatus::Confirmed,
        asset_id,
        symbol: t.symbol,
        chain_id: chain.id().to_string(),
        chain_name: chain.name().to_string(),
        to_address: clean(&to_address, 100),
        amount_base: t.amount,
        fee_base: "0".to_string(),
        decimals: t.decimals,
        price_usd: 0.0,
        tx_hash: Some(t.hash),
        explorer_url: None,
        counterparty: counterparty.map(|c| clean(&c, 100)),
        error: None,
        created_at: t.at,
        confirmed_at: Some(t.at),
        received: None,
    }
}

/// A base-unit amount that moved something: digits only, not zero.
pub(crate) fn moved(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.bytes().any(|b| b != b'0')
}

/// Top-level transaction: a plain coin payment in or out. Only a mined,
/// successful one with a value moved anything.
fn map_tx(wallet_id: &str, chain: &ChainInfo, our_address: &str, tx: TxRow) -> Option<NewEntry> {
    if tx.status.as_deref() != Some("ok") {
        return None;
    }
    let value = tx.value.filter(|v| moved(v))?;
    let native = chain.native_currency();
    Some(row(
        wallet_id,
        chain,
        Transfer {
            hash: clean(&tx.hash?, 100),
            from: tx.from.hash,
            to: tx.to.hash,
            token: None,
            symbol: clean(&native.symbol, 16),
            decimals: native.decimals,
            amount: value,
            at: epoch(&tx.timestamp),
            our_address,
        },
    ))
}

/// Coin moved to us by a contract. Always received: the endpoint filters `to`.
fn map_native(wallet_id: &str, chain: &ChainInfo, our_address: &str, r: InternalRow) -> Option<NewEntry> {
    // A failed internal call moved nothing, whatever its value says.
    if r.success == Some(false) {
        return None;
    }
    let value = r.value.filter(|v| moved(v))?;
    let native = chain.native_currency();
    Some(row(
        wallet_id,
        chain,
        Transfer {
            hash: clean(&r.transaction_hash?, 100),
            from: r.from.hash,
            to: Some(our_address.to_string()),
            token: None,
            symbol: clean(&native.symbol, 16),
            decimals: native.decimals,
            amount: value,
            at: epoch(&r.timestamp),
            our_address,
        },
    ))
}

/// ERC-20 movement. NFTs (no decimals) are left out of Activity.
fn map_token(wallet_id: &str, chain: &ChainInfo, our_address: &str, r: TokenRow) -> Option<NewEntry> {
    // An unknown decimals value cannot be rendered safely, so the row is
    // dropped rather than guessed at.
    let decimals = r.token.decimals?;
    let amount = r.total.value.filter(|v| moved(v))?;
    let symbol = clean(&r.token.symbol.unwrap_or_default(), 16);
    if symbol.is_empty() {
        return None;
    }
    Some(row(
        wallet_id,
        chain,
        Transfer {
            hash: clean(&r.transaction_hash?, 100),
            from: r.from.hash,
            to: r.to.hash,
            token: Some(clean(&r.token.address.unwrap_or_default(), 100)),
            symbol,
            decimals,
            amount,
            at: epoch(&r.timestamp),
            our_address,
        },
    ))
}

// ---- Blockscout wire types -------------------------------------------------

#[derive(Debug, Deserialize)]
struct TxPage {
    #[serde(default)]
    items: Vec<TxRow>,
}

#[derive(Debug, Deserialize)]
struct TxRow {
    #[serde(default)]
    hash: Option<String>,
    #[serde(default, deserialize_with = "addr_or_null")]
    from: AddrRef,
    /// `null` for a contract creation.
    #[serde(default, deserialize_with = "addr_or_null")]
    to: AddrRef,
    /// Base units (wei).
    #[serde(default)]
    value: Option<String>,
    /// `ok`, `error`, or `null` while pending.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

fn addr_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<AddrRef, D::Error> {
    Ok(Option::<AddrRef>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct TokenPage {
    #[serde(default)]
    items: Vec<TokenRow>,
}

#[derive(Debug, Deserialize)]
struct TokenRow {
    #[serde(default, deserialize_with = "addr_or_null")]
    from: AddrRef,
    #[serde(default, deserialize_with = "addr_or_null")]
    to: AddrRef,
    token: Token,
    #[serde(default)]
    total: Total,
    #[serde(default)]
    transaction_hash: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AddrRef {
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Token {
    #[serde(default)]
    symbol: Option<String>,
    /// Blockscout sends this as a *string* (`"9"`), not a number.
    #[serde(default, deserialize_with = "super::lenient_u8")]
    decimals: Option<u8>,
    /// Named `address_hash` by Blockscout, not `address`.
    #[serde(default, rename = "address_hash")]
    address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Total {
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InternalPage {
    #[serde(default)]
    items: Vec<InternalRow>,
}

#[derive(Debug, Deserialize)]
struct InternalRow {
    /// Endpoint is queried with `filter=to`, so this is always us.
    #[serde(default, deserialize_with = "addr_or_null")]
    #[allow(dead_code)]
    to: AddrRef,
    #[serde(default, deserialize_with = "addr_or_null")]
    from: AddrRef,
    /// Base units (wei).
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    transaction_hash: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    success: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry::model::EvmChain;

    /// Runs a raw JSON literal through a deserializer, the way an explorer's
    /// response would.
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "super::super::lenient_u8")]
        v: Option<u8>,
    }

    fn u8_of(raw: &str) -> Option<u8> {
        let probe: Probe = serde_json::from_str(&format!("{{\"v\":{raw}}}")).unwrap();
        probe.v
    }

    fn chain() -> ChainInfo {
        ChainInfo::Evm(
            serde_json::from_str::<EvmChain>(include_str!(
                "../../../registry/data/evm/1.json"
            ))
            .expect("fixture"),
        )
    }

    fn token_row(from: Option<&str>, to: Option<&str>, symbol: &str) -> TokenRow {
        TokenRow {
            from: AddrRef {
                hash: from.map(str::to_string),
            },
            to: AddrRef {
                hash: to.map(str::to_string),
            },
            token: Token {
                symbol: Some(symbol.to_string()),
                decimals: Some(6),
                address: Some(format!("0x{symbol}")),
            },
            total: Total {
                value: Some("2500000".to_string()),
            },
            transaction_hash: Some("0xhash".to_string()),
            timestamp: Some("2026-10-08T06:44:59Z".to_string()),
        }
    }

    #[test]
    fn incoming_token_is_received() {
        let row = map_token("w", &chain(), "0xABC", token_row(Some("0xother"), Some("0xabc"), "USDC"))
            .expect("mapped");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.amount_base, "2500000");
        assert_eq!(row.decimals, 6);
        assert_eq!(row.asset_id, "evm:1:0xusdc");
        assert_eq!(row.counterparty.as_deref(), Some("0xother"));
    }

    /// Our address being the sender must produce a send, case-insensitively,
    /// because explorer hashes and stored addresses differ in casing.
    #[test]
    fn outgoing_token_is_sent() {
        let row = map_token("w", &chain(), "0xabc", token_row(Some("0xABC"), Some("0xdest"), "USDC"))
            .expect("mapped");
        assert_eq!(row.kind, HistoryKind::Sent);
        assert_eq!(row.to_address, "0xdest");
        assert_eq!(row.counterparty.as_deref(), Some("0xdest"));
    }

    /// Two token transfers in one transaction must stay separate rows, which is
    /// why the local id includes the symbol.
    #[test]
    fn same_hash_different_tokens_get_distinct_ids() {
        let a = map_token("w", &chain(), "0xabc", token_row(Some("0xo"), None, "USDC"))
            .expect("a");
        let b = map_token("w", &chain(), "0xabc", token_row(Some("0xo"), None, "DAI"))
            .expect("b");
        assert_eq!(a.tx_hash, b.tx_hash);
        assert_ne!(a.local_id, b.local_id, "must not collapse into one row");
    }

    #[test]
    fn token_without_decimals_or_symbol_is_dropped() {
        let mut no_decimals = token_row(Some("0xo"), None, "USDC");
        no_decimals.token.decimals = None;
        assert!(map_token("w", &chain(), "0xabc", no_decimals).is_none());

        let blank_symbol = token_row(Some("0xo"), None, "");
        assert!(map_token("w", &chain(), "0xabc", blank_symbol).is_none());

        let no_hash = token_row(Some("0xo"), None, "USDC");
        assert!(map_token("w", &chain(), "0xabc", TokenRow { transaction_hash: None, ..no_hash }).is_none());
    }

    #[test]
    fn native_transfer_is_received_with_chain_decimals() {
        let row = InternalRow {
            to: AddrRef { hash: Some("0xabc".into()) },
            from: AddrRef { hash: Some("0xsender".into()) },
            value: Some("1000000000000000000".into()),
            transaction_hash: Some("0xn".into()),
            timestamp: Some("2026-10-08T06:44:59Z".into()),
            success: Some(true),
        };
        let mapped = map_native("w", &chain(), "0xabc", row).expect("mapped");
        assert_eq!(mapped.kind, HistoryKind::Received);
        assert_eq!(mapped.asset_id, "evm:1:native");
        assert_eq!(mapped.decimals, 18);
        assert_eq!(mapped.counterparty.as_deref(), Some("0xsender"));
        assert_eq!(mapped.amount_base, "1000000000000000000");
    }

    #[test]
    fn native_transfer_without_value_is_dropped() {
        let row = InternalRow {
            to: AddrRef { hash: None },
            from: AddrRef { hash: None },
            value: None,
            transaction_hash: Some("0xn".into()),
            timestamp: None,
            success: Some(true),
        };
        assert!(map_native("w", &chain(), "0xabc", row).is_none());
    }

    #[test]
    fn ethereum_fixture_declares_blockscout() {
        assert!(matches!(
            chain().history_api(),
            Some(HistoryApi::Blockscout { .. })
        ));
    }

    /// Regression guard: Blockscout sends `token.decimals` as the string `"9"`
/// and the contract under `address_hash`, not `address`. Both were wrong at
/// once and the parse failure was silently swallowed, so the whole
/// token-transfer half of the feed silently disappeared.
#[test]
    fn blockscout_token_shape_is_handled() {
        let raw = r#"{
            "from": { "hash": "0xother" },
            "to": { "hash": "0xabc" },
            "token": {
                "address_hash": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "decimals": "6",
                "symbol": "USDC",
                "type": "ERC-20"
            },
            "total": { "value": "2500000", "decimals": "6" },
            "transaction_hash": "0xhash",
            "timestamp": "2026-10-08T06:44:59.000000Z"
        }"#;
        let row: TokenRow = serde_json::from_str(raw).expect("must parse real shape");
        let mapped = map_token("w", &chain(), "0xabc", row).expect("mapped");
        assert_eq!(mapped.decimals, 6, "decimals arrive as a string");
        assert_eq!(
            mapped.asset_id, "evm:1:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert_eq!(mapped.amount_base, "2500000");
    }

    #[test]
    fn numeric_and_string_decimals_both_parse() {
        assert_eq!(u8_of("6"), Some(6));
        assert_eq!(u8_of("\"18\""), Some(18));
        assert_eq!(u8_of("null"), None);
        // Beyond u8 is unusable for display, so it is refused rather than wrapped.
        assert_eq!(u8_of("999"), None);
    }

    #[test]
    fn chain_without_source_is_rejected() {
        let ChainInfo::Evm(mut evm) = chain() else {
            unreachable!()
        };
        evm.history_api = None;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let client = super::super::http_client("history-engine-tests").expect("client");
        let err = runtime.block_on(read(&client, "w", &ChainInfo::Evm(evm), "0xabc")).expect_err("must refuse");
        assert!(matches!(err, HistoryError::Unsupported(_)));
    }

    fn tx_row(from: &str, to: &str, value: &str, status: Option<&str>) -> TxRow {
        TxRow {
            hash: Some("0xtop".into()),
            from: AddrRef { hash: Some(from.into()) },
            to: AddrRef { hash: Some(to.into()) },
            value: Some(value.into()),
            status: status.map(str::to_string),
            timestamp: Some("2026-10-08T06:44:59.000000Z".into()),
        }
    }

    /// A plain coin payment is a top-level transaction, not an internal one.
    /// Reading only internal transactions missed every ordinary deposit.
    #[test]
    fn a_plain_coin_payment_in_is_received() {
        let row = map_tx("w", &chain(), "0xABC", tx_row("0xsender", "0xabc", "500", Some("ok"))).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.asset_id, "evm:1:native");
        assert_eq!(row.amount_base, "500");
        assert_eq!(row.decimals, 18);
        assert_eq!(row.counterparty.as_deref(), Some("0xsender"));
        assert_eq!(row.to_address, "0xABC");
        assert_eq!(row.created_at, 1_791_441_899, "dated by its block");
    }

    #[test]
    fn a_plain_coin_payment_out_is_sent() {
        let row = map_tx("w", &chain(), "0xabc", tx_row("0xABC", "0xdest", "500", Some("ok"))).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Sent);
        assert_eq!(row.to_address, "0xdest");
        assert_eq!(row.counterparty.as_deref(), Some("0xdest"));
    }

    /// Nothing moved: a reverted transaction, one still pending, a contract
    /// call that carried no coin.
    #[test]
    fn transactions_that_moved_no_coin_make_no_row() {
        assert!(map_tx("w", &chain(), "0xabc", tx_row("0xo", "0xabc", "500", Some("error"))).is_none());
        assert!(map_tx("w", &chain(), "0xabc", tx_row("0xo", "0xabc", "500", None)).is_none());
        assert!(map_tx("w", &chain(), "0xabc", tx_row("0xabc", "0xc", "0", Some("ok"))).is_none());
    }

    #[test]
    fn a_failed_internal_transfer_makes_no_row() {
        let row = InternalRow {
            to: AddrRef { hash: Some("0xabc".into()) },
            from: AddrRef { hash: Some("0xcontract".into()) },
            value: Some("5".into()),
            transaction_hash: Some("0xi".into()),
            timestamp: None,
            success: Some(false),
        };
        assert!(map_native("w", &chain(), "0xabc", row).is_none());
    }

    /// The id is built from what moved, so the same transfer read from two
    /// sources (one standing in for the other) is one row, not two.
    #[test]
    fn transfer_ids_come_from_what_moved() {
        let a = map_token("w", &chain(), "0xabc", token_row(Some("0xo"), Some("0xabc"), "USDC")).unwrap();
        assert_eq!(a.local_id, "0xhash:evm:1:0xusdc:received:2500000");
        assert_eq!(transfer_id("0xHASH", "evm:1:native", HistoryKind::Sent, "9"), "0xhash:evm:1:native:sent:9");
    }

    #[test]
    fn blockscout_transaction_shape_is_handled() {
        let raw = r#"{ "items": [{
            "hash": "0xtop",
            "from": { "hash": "0xSender" },
            "to": { "hash": "0xabc" },
            "value": "1000000000000000000",
            "status": "ok",
            "result": "success",
            "timestamp": "2026-10-08T06:44:59.000000Z"
        }, {
            "hash": "0xdeploy",
            "from": { "hash": "0xabc" },
            "to": null,
            "value": "0",
            "status": "ok",
            "timestamp": "2026-10-08T06:44:59.000000Z"
        }] }"#;
        let page: TxPage = serde_json::from_str(raw).expect("must parse real shape");
        let rows: Vec<_> = page.items.into_iter().filter_map(|r| map_tx("w", &chain(), "0xabc", r)).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].amount_base, "1000000000000000000");
        assert_eq!(rows[0].kind, HistoryKind::Received);
    }
}
