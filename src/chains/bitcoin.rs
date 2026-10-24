//! Bitcoin history via Esplora (mempool.space).
//!
//! UTXO model: there is no "balance at height H" to read, so both direction
//! and amount have to be derived from the inputs and outputs of each
//! transaction:
//!
//! - **sent**    — inputs we own (their `prevout` pays our address)
//! - **received** — outputs that pay our address
//!
//! One transaction can do both: a self-send that overpays the fee. The
//! received amount is authoritative for the activity feed; the fee is recorded
//! on the send side so it is not double-counted.

use super::{epoch, get_json};
use crate::error::{HistoryError, HistoryResult};
use crate::model::{HistoryKind, HistoryStatus, NewEntry};
#[cfg(test)]
use crate::model::now_secs;
use registry::model::{ChainInfo, HistoryApi};
use serde::Deserialize;

/// Esplora returns 25 per page. Older history is reachable by paging, but the
/// first page is what the activity screen needs.
const MAX_ROWS: usize = 50;

pub async fn read(
    client: &reqwest::Client,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
) -> HistoryResult<Vec<NewEntry>> {
    let HistoryApi::Esplora { base_url } = chain
        .history_api()
        .ok_or_else(|| HistoryError::Unsupported(chain.id().to_string()))?
    else {
        return Err(HistoryError::Unsupported(chain.id().to_string()));
    };

    let base = base_url.trim_end_matches('/');
    // No error remapping here: a parse failure must stay visible, otherwise a
    // format change looks like an empty wallet.
    let txs: Vec<EsploraTx> = get_json(client, &format!("{base}/address/{address}/txs")).await?;

    Ok(txs
        .iter()
        .take(MAX_ROWS)
        .filter_map(|tx| map_tx(wallet_id, chain, address, tx))
        .collect())
}

/// Derives direction and amount from one transaction's inputs and outputs.
fn map_tx(
    wallet_id: &str,
    chain: &ChainInfo,
    our_address: &str,
    tx: &EsploraTx,
) -> Option<NewEntry> {
    if tx.txid.is_empty() {
        return None;
    }
    let at = epoch(&tx.block_time.map(|t| t.to_string()));
    let status = if tx.status.confirmed {
        HistoryStatus::Confirmed
    } else {
        HistoryStatus::Pending
    };

    // Outputs paying us: the amount actually credited.
    let mut received: u64 = 0;
    for out in &tx.vout {
        if pays_us(out, our_address) {
            received = received.saturating_add(out.value.unwrap_or(0));
        }
    }
    // Inputs we own: what left the wallet, fee included.
    let mut spent: u64 = 0;
    for input in &tx.vin {
        if input
            .prevout
            .as_ref()
            .is_some_and(|p| p.scriptpubkey_address.as_deref() == Some(our_address))
        {
            spent = spent.saturating_add(
                input
                    .prevout
                    .as_ref()
                    .and_then(|p| p.value)
                    .unwrap_or(0),
            );
        }
    }

    let (kind, amount, counterparty) = if received > 0 {
        // A receive, or a self-send with change coming back. The credited
        // amount is what belongs in the activity feed.
        (
            HistoryKind::Received,
            received,
            tx.vin
                .iter()
                .find_map(|i| i.prevout.as_ref()?.scriptpubkey_address.clone()),
        )
    } else if spent > 0 {
        // A pure spend. Record the net debit so the amount and the fee agree.
        (
            HistoryKind::Sent,
            spent.saturating_sub(tx.fee.unwrap_or(0)),
            tx.vout
                .iter()
                .find_map(|o| o.scriptpubkey_address.clone()),
        )
    } else {
        // Touches our address but moves nothing we can attribute — a
        // coinbase paying a related address, say. Better to omit than to
        // invent an amount.
        return None;
    };

    let decimals = chain.native_currency().decimals;
    Some(NewEntry {
        wallet_id: wallet_id.to_string(),
        local_id: tx.txid.clone(),
        kind,
        status,
        asset_id: format!("{}:native", chain.id()),
        symbol: chain.native_currency().symbol.clone(),
        chain_id: chain.id().to_string(),
        chain_name: chain.name().to_string(),
        to_address: counterparty.clone().unwrap_or_else(|| our_address.to_string()),
        amount_base: amount.to_string(),
        fee_base: tx.fee.unwrap_or(0).to_string(),
        decimals,
        price_usd: 0.0,
        tx_hash: Some(tx.txid.clone()),
        explorer_url: None,
        counterparty,
        error: None,
        created_at: at,
        confirmed_at: if tx.status.confirmed { Some(at) } else { None },
        received: None,
    })
}

fn pays_us(out: &Vout, our_address: &str) -> bool {
    out.scriptpubkey_address.as_deref() == Some(our_address)
}

// ---- Esplora wire types ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct EsploraTx {
    #[serde(default)]
    txid: String,
    #[serde(default)]
    status: EsploraStatus,
    /// Unix seconds. Absent on mempool transactions.
    #[serde(default)]
    block_time: Option<i64>,
    /// Satoshis. Absent on transactions with an unknown fee.
    #[serde(default, deserialize_with = "super::lenient_u64")]
    fee: Option<u64>,
    #[serde(default)]
    vin: Vec<Vin>,
    #[serde(default)]
    vout: Vec<Vout>,
}

#[derive(Debug, Default, Deserialize)]
struct EsploraStatus {
    #[serde(default)]
    confirmed: bool,
}

#[derive(Debug, Deserialize)]
struct Vin {
    /// The output being spent. Absent for coinbase inputs.
    #[serde(default)]
    prevout: Option<Prevout>,
}

#[derive(Debug, Deserialize)]
struct Prevout {
    #[serde(default)]
    scriptpubkey_address: Option<String>,
    /// Satoshis.
    #[serde(default, deserialize_with = "super::lenient_u64")]
    value: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Vout {
    #[serde(default)]
    scriptpubkey_address: Option<String>,
    /// Satoshis.
    #[serde(default, deserialize_with = "super::lenient_u64")]
    value: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry::model::BitcoinChain;

    const OURS: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

    /// Runs a raw JSON literal through a deserializer, the way an explorer's
    /// response would.
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "super::super::lenient_u64")]
        v: Option<u64>,
    }

    fn u64_of(raw: &str) -> Option<u64> {
        let probe: Probe = serde_json::from_str(&format!("{{\"v\":{raw}}}")).unwrap();
        probe.v
    }

    fn chain() -> ChainInfo {
        ChainInfo::Bitcoin(
            serde_json::from_str::<BitcoinChain>(include_str!(
                "../../../registry/data/bitcoin.json"
            ))
            .expect("fixture"),
        )
    }

    fn vout(addr: &str, value: u64) -> Vout {
        Vout {
            scriptpubkey_address: Some(addr.to_string()),
            value: Some(value),
        }
    }

    fn vin(addr: &str, value: u64) -> Vin {
        Vin {
            prevout: Some(Prevout {
                scriptpubkey_address: Some(addr.to_string()),
                value: Some(value),
            }),
        }
    }

    #[test]
    fn a_receive_uses_output_value() {
        let tx = EsploraTx {
            txid: "abc".into(),
            status: EsploraStatus { confirmed: true },
            block_time: Some(1_700_000_000),
            fee: Some(1_000),
            vin: vec![vin("someone-else", 500_000)],
            vout: vec![vout(OURS, 499_000)],
        };
        let row = map_tx("w", &chain(), OURS, &tx).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.amount_base, "499000");
        assert_eq!(row.status, HistoryStatus::Confirmed);
        assert_eq!(row.asset_id, "bitcoin:native");
        assert_eq!(row.decimals, 8);
        assert_eq!(row.counterparty.as_deref(), Some("someone-else"));
    }

    /// A pure spend must record the net debit, not the gross input, or the
    /// amount and the fee would not add up.
    #[test]
    fn a_send_records_net_debit_and_fee() {
        let tx = EsploraTx {
            txid: "abc".into(),
            status: EsploraStatus { confirmed: true },
            block_time: Some(1_700_000_000),
            fee: Some(1_500),
            vin: vec![vin(OURS, 100_000)],
            vout: vec![vout("recipient", 98_500)],
        };
        let row = map_tx("w", &chain(), OURS, &tx).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Sent);
        assert_eq!(row.amount_base, "98500", "must be input minus fee");
        assert_eq!(row.fee_base, "1500");
        assert_eq!(row.counterparty.as_deref(), Some("recipient"));
    }

    /// A self-send returns change to us. The credited amount is what belongs
    /// in the feed; the gross spend would overstate it.
    #[test]
    fn a_self_send_is_reported_as_received_change() {
        let tx = EsploraTx {
            txid: "abc".into(),
            status: EsploraStatus { confirmed: true },
            block_time: Some(1_700_000_000),
            fee: Some(1_000),
            vin: vec![vin(OURS, 100_000)],
            vout: vec![vout("recipient", 50_000), vout(OURS, 49_000)],
        };
        let row = map_tx("w", &chain(), OURS, &tx).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.amount_base, "49000", "only the change is credited");
    }

    /// A mempool transaction has no block time and is not confirmed.
    #[test]
    fn unconfirmed_is_pending_and_falls_back_to_now() {
        let tx = EsploraTx {
            txid: "abc".into(),
            status: EsploraStatus::default(),
            block_time: None,
            fee: Some(100),
            vin: vec![vin(OURS, 10_000)],
            vout: vec![vout("dest", 9_900)],
        };
        let before = now_secs();
        let row = map_tx("w", &chain(), OURS, &tx).expect("mapped");
        assert_eq!(row.status, HistoryStatus::Pending);
        assert!(row.confirmed_at.is_none());
        assert!(row.created_at >= before - 1);
    }

    /// Touching our address without moving anything attributable must be
    /// omitted rather than shown with a made-up amount.
    #[test]
    fn unattributable_transaction_is_dropped() {
        let tx = EsploraTx {
            txid: "abc".into(),
            status: EsploraStatus { confirmed: true },
            block_time: Some(1_700_000_000),
            fee: None,
            vin: vec![Vin { prevout: None }],
            vout: vec![vout("someone", 5_000)],
        };
        assert!(map_tx("w", &chain(), OURS, &tx).is_none());
    }

    /// Outputs must match on the full address, not a prefix — otherwise a
    /// different address sharing our first characters would credit us.
    #[test]
    fn output_matching_is_exact() {
        assert!(pays_us(&vout(OURS, 1), OURS));
        assert!(!pays_us(&vout("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNb", 1), OURS));
        assert!(!pays_us(&vout("someone", 1), OURS));
    }

    #[test]
    fn bitcoin_fixture_declares_esplora() {
        assert!(matches!(
            chain().history_api(),
            Some(HistoryApi::Esplora { .. })
        ));
    }

    /// Regression guard for the real Esplora shape, including a transaction with
/// no `block_time` (mempool) and `fee` sent as a JSON number.
#[test]
    fn esplora_wire_shape_is_handled() {
        let raw = r#"[
          {
            "txid": "abc",
            "version": 2,
            "size": 223,
            "weight": 891,
            "sigops": null,
            "fee": 47,
            "status": { "confirmed": false },
            "vin": [
              { "txid": "prev", "vout": 0, "is_coinbase": false, "sequence": 4294967293,
                "prevout": { "scriptpubkey_address": "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa", "value": 330 } }
            ],
            "vout": [
              { "scriptpubkey_address": "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa", "value": 283 }
            ]
          }
        ]"#;
        let txs: Vec<EsploraTx> = serde_json::from_str(raw).expect("must parse real shape");
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].fee, Some(47));
        let row = map_tx("w", &chain(), OURS, &txs[0]).expect("mapped");
        assert_eq!(row.kind, HistoryKind::Received);
        assert_eq!(row.amount_base, "283");
        assert_eq!(row.status, HistoryStatus::Pending);
    }

    #[test]
    fn string_and_numeric_satoshi_values_both_parse() {
        assert_eq!(u64_of("330"), Some(330));
        assert_eq!(u64_of("\"330\""), Some(330));
        assert_eq!(u64_of("null"), None);
    }

    #[test]
    fn chain_without_source_is_rejected() {
        let ChainInfo::Bitcoin(mut btc) = chain() else {
            unreachable!()
        };
        btc.history_api = None;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let client = super::super::http_client().expect("client");
        let err = runtime
            .block_on(read(&client, "w", &ChainInfo::Bitcoin(btc), OURS))
            .expect_err("must refuse");
        assert!(matches!(err, HistoryError::Unsupported(_)));
    }
}