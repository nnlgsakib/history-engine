//! Live checks against real public explorers. Not run by `cargo test`.
//!
//! Run: cargo test -p fnf-history --test live -- --ignored --nocapture
//!
//! These hit the network. They exist so a wire-format change or a renamed
//! endpoint is caught here rather than as an empty activity list on a device.

use fnf_history::discovery::{Discovery, ScanTarget};
use fnf_registry::model::{BitcoinChain, ChainInfo, EvmChain};

const ETHEREUM: &str = include_str!("../../registry/data/evm/1.json");
const BITCOIN: &str = include_str!("../../registry/data/bitcoin.json");

/// Address with a long public history on Ethereum.
const ETH_ADDRESS: &str = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
/// First ever Bitcoin address, so history is guaranteed non-empty.
const BTC_ADDRESS: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";

fn evm() -> ChainInfo {
    ChainInfo::Evm(serde_json::from_str::<EvmChain>(ETHEREUM).expect("fixture"))
}

fn btc() -> ChainInfo {
    ChainInfo::Bitcoin(serde_json::from_str::<BitcoinChain>(BITCOIN).expect("fixture"))
}

#[test]
#[ignore]
fn ethereum_token_and_native_transfers_are_readable() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let target = ScanTarget {
            wallet_id: "test".into(),
            chain: evm(),
            address: ETH_ADDRESS.into(),
        };
        let rows = Discovery::new()
            .expect("client")
            .scan(&target)
            .await
            .expect("scan");
        println!("ethereum rows: {}", rows.len());
        assert!(!rows.is_empty(), "expected history for a known address");
        assert!(rows.iter().all(|r| r.tx_hash.is_some()));
        assert!(rows.iter().all(|r| r.decimals > 0));

        // Token transfers and native transfers are separate endpoints. If only
        // the first were read this would fail, and incoming ETH would be
        // silently missing in production.
        assert!(
            rows.iter().any(|r| r.asset_id.ends_with(":native")),
            "native transfers must be read from internal-transactions"
        );
        assert!(
            rows.iter().any(|r| !r.asset_id.ends_with(":native")),
            "token transfers must be read from token-transfers"
        );
        assert!(
            rows.iter().any(|r| r.kind == fnf_history::HistoryKind::Received),
            "expected at least one incoming transfer"
        );
    });
}

#[test]
#[ignore]
fn ethereum_amounts_keep_full_precision() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let target = ScanTarget {
            wallet_id: "test".into(),
            chain: evm(),
            address: ETH_ADDRESS.into(),
        };
        let rows = Discovery::new().expect("client").scan(&target).await.expect("scan");
        // Base-unit strings, never floats.
        for row in &rows {
            assert!(
                row.amount_base.chars().all(|c| c.is_ascii_digit()),
                "non-digit base amount {:?}",
                row.amount_base
            );
            assert!(!row.amount_base.contains('.'), "amount must not be a float");
        }
    });
}

#[test]
#[ignore]
fn bitcoin_history_has_real_amounts_and_direction() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let target = ScanTarget {
            wallet_id: "test".into(),
            chain: btc(),
            address: BTC_ADDRESS.into(),
        };
        let rows = Discovery::new().expect("client").scan(&target).await.expect("scan");
        println!("bitcoin rows: {}", rows.len());
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.asset_id == "bitcoin:native"));
        assert!(rows.iter().all(|r| r.decimals == 8));

        // Amounts are derived from inputs/outputs, so at least one row must
        // carry a non-zero value. A previous implementation reported every
        // Bitcoin transfer as zero because the endpoint had no totals.
        assert!(
            rows.iter().any(|r| r.amount_base != "0"),
            "bitcoin rows must carry derived amounts, not zeros"
        );
        assert!(
            rows.iter().all(|r| r.kind == fnf_history::HistoryKind::Sent
                || r.kind == fnf_history::HistoryKind::Received),
            "direction must be derived from inputs and outputs"
        );
    });
}

#[test]
#[ignore]
fn both_chain_kinds_read_through_the_public_entry_point() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let discovery = Discovery::new().expect("client");
        for (chain, address) in [(evm(), ETH_ADDRESS), (btc(), BTC_ADDRESS)] {
            let chain_id = chain.id().to_string();
            let target = ScanTarget {
                wallet_id: "test".into(),
                chain,
                address: address.into(),
            };
            match discovery.scan(&target).await {
                Ok(rows) => println!("{chain_id}: {} rows", rows.len()),
                Err(e) => panic!("{chain_id} failed: {e}"),
            }
        }
    });
}

#[test]
#[ignore]
fn a_chain_without_a_source_reports_unsupported() {
    let ChainInfo::Evm(mut chain) = evm() else {
        unreachable!()
    };
    chain.history_api = None;
    assert!(!Discovery::supported(&ChainInfo::Evm(chain)));
}