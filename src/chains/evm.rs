//! EVM history via Blockscout.
//!
//! Account model: a transaction's direction is simply whether our address is
//! the sender, and balances are readable at any moment.
//!
//! The trap here is that Blockscout splits history across **two** endpoints
//! that do not overlap:
//!
//! - `token-transfers` — ERC-20/721/1155 movements.
//! - `internal-transactions?filter=to` — plain native coin transfers.
//!
//! Most incoming value is in the second one. Fetching only the first looks
//! like it works while quietly missing most deposits, so both are read.

use super::{clean, epoch, get_json};
use crate::error::{HistoryError, HistoryResult};
use crate::model::{HistoryKind, HistoryStatus, NewEntry};
use fnf_registry::model::{ChainInfo, HistoryApi};
use serde::Deserialize;

/// Blockscout returns 50 per page. One page is the recent history a user opens
/// the activity screen for; deeper paging is added only if it proves needed.
const MAX_TOKEN_ROWS: usize = 50;
const MAX_NATIVE_ROWS: usize = 50;

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

    // Both endpoints are required, and both failures are surfaced. Swallowing
    // one would show an activity feed missing every token transfer while
    // looking entirely plausible.
    let (tokens, natives) = futures_util::future::join(
        token_transfers(client, base, &addr),
        internal_transfers(client, base, &addr),
    )
    .await;
    let tokens = tokens?;
    let natives = natives?;

    let mut out: Vec<NewEntry> = tokens
        .into_iter()
        .filter_map(|row| map_token(wallet_id, chain, address, row))
        .collect();
    out.extend(
        natives
            .into_iter()
            .filter_map(|row| map_native(wallet_id, chain, address, row)),
    );
    Ok(out)
}

async fn token_transfers(
    client: &reqwest::Client,
    base: &str,
    address: &str,
) -> HistoryResult<Vec<TokenRow>> {
    let url = format!("{base}/api/v2/addresses/{address}/token-transfers");
    let page: TokenPage = get_json(client, &url).await?;
    Ok(page.items.into_iter().take(MAX_TOKEN_ROWS).collect())
}

async fn internal_transfers(
    client: &reqwest::Client,
    base: &str,
    address: &str,
) -> HistoryResult<Vec<InternalRow>> {
    // `filter=to` restricts to incoming, so every row is inbound to us.
    let url = format!("{base}/api/v2/addresses/{address}/internal-transactions?filter=to");
    let page: InternalPage = get_json(client, &url).await?;
    Ok(page.items.into_iter().take(MAX_NATIVE_ROWS).collect())
}

/// ERC-20/721/1155 movement.
fn map_token(
    wallet_id: &str,
    chain: &ChainInfo,
    our_address: &str,
    row: TokenRow,
) -> Option<NewEntry> {
    let hash = row.transaction_hash.clone()?;
    // An unknown decimals value cannot be rendered safely, so the row is
    // dropped rather than guessed at.
    let decimals = row.token.decimals?;
    let raw = row.total.value?;
    let symbol = clean(&row.token.symbol.clone().unwrap_or_default(), 16);
    if symbol.is_empty() {
        return None;
    }

    let outgoing = row
        .from
        .hash
        .as_deref()
        .map(|h| h.eq_ignore_ascii_case(our_address))
        .unwrap_or(false);
    let counterparty = if outgoing {
        row.to.hash.clone()
    } else {
        row.from.hash.clone()
    };
    let at = epoch(&row.timestamp);

    Some(NewEntry {
        wallet_id: wallet_id.to_string(),
        // Hash + symbol, so two token transfers in one transaction stay
        // distinct rows.
        local_id: format!("{hash}:{symbol}"),
        kind: if outgoing {
            HistoryKind::Sent
        } else {
            HistoryKind::Received
        },
        status: HistoryStatus::Confirmed,
        asset_id: format!(
            "{}:{}",
            asset_prefix(chain),
            row.token
                .address
                .clone()
                .unwrap_or_default()
                .to_lowercase()
        ),
        symbol,
        chain_id: chain.id().to_string(),
        chain_name: chain.name().to_string(),
        to_address: counterparty.clone().unwrap_or_default(),
        amount_base: raw,
        fee_base: "0".to_string(),
        decimals,
        price_usd: 0.0,
        tx_hash: Some(hash),
        explorer_url: None,
        counterparty,
        error: None,
        created_at: at,
        confirmed_at: Some(at),
        received: None,
    })
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

/// Plain native-coin transfer. Always received: the endpoint filters `to`.
fn map_native(
    wallet_id: &str,
    chain: &ChainInfo,
    our_address: &str,
    row: InternalRow,
) -> Option<NewEntry> {
    let hash = row.transaction_hash.clone()?;
    let raw = row.value.clone()?;
    let symbol = clean(&chain.native_currency().symbol, 16);
    let at = epoch(&row.timestamp);

    Some(NewEntry {
        wallet_id: wallet_id.to_string(),
        local_id: format!("{hash}:{symbol}"),
        kind: HistoryKind::Received,
        status: HistoryStatus::Confirmed,
        asset_id: format!("{}:native", asset_prefix(chain)),
        symbol,
        chain_id: chain.id().to_string(),
        chain_name: chain.name().to_string(),
        to_address: our_address.to_string(),
        amount_base: raw,
        fee_base: "0".to_string(),
        decimals: chain.native_currency().decimals,
        price_usd: 0.0,
        tx_hash: Some(hash),
        explorer_url: None,
        counterparty: row.from.hash.clone(),
        error: None,
        created_at: at,
        confirmed_at: Some(at),
        received: None,
    })
}

// ---- Blockscout wire types -------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenPage {
    #[serde(default)]
    items: Vec<TokenRow>,
}

#[derive(Debug, Deserialize)]
struct TokenRow {
    #[serde(default)]
    from: AddrRef,
    #[serde(default)]
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
    /// Endpoint is queried with `filter=to`, so this is always us. Unused
    /// today, but asserting it later is cheaper than discovering the filter
    /// changed.
    #[serde(default)]
    #[allow(dead_code)]
    to: AddrRef,
    #[serde(default)]
    from: AddrRef,
    /// Base units (wei) for native transfers.
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    transaction_hash: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnf_registry::model::EvmChain;

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
                address: Some("0xToken".to_string()),
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
        assert_eq!(row.asset_id, "evm:1:0xtoken");
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
        let client = super::super::http_client().expect("client");
        let err = runtime.block_on(read(&client, "w", &ChainInfo::Evm(evm), "0xabc")).expect_err("must refuse");
        assert!(matches!(err, HistoryError::Unsupported(_)));
    }
}