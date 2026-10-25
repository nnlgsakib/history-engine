//! Live checks against real public explorers. Not run by `cargo test`.
//!
//! Run: cargo test -p history-engine --test live -- --ignored --nocapture
//!
//! These hit the network. They exist so a wire-format change or a renamed
//! endpoint is caught here rather than as an empty activity list on a device.

use history_engine::discovery::{Discovery, ScanTarget};
use registry::model::{BitcoinChain, ChainInfo, EvmChain};

const ETHEREUM: &str = include_str!("../../registry/data/evm/1.json");
const BITCOIN: &str = include_str!("../../registry/data/bitcoin.json");

/// An ordinary active Ethereum address. Blockscout's internal-transactions
/// endpoint times out for very busy ones (vitalik.eth: over 40 s).
const ETH_ADDRESS: &str = "0xED9Ab5BC05A0152B9dC2f7902e8369Af7bC18771";
/// Address with a long public history, for providers that page.
const BUSY_ADDRESS: &str = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
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
        let rows = Discovery::new("history-engine-tests")
            .expect("client")
            .scan(&target)
            .await
            .expect("scan");
        println!("ethereum rows: {}", rows.len());
        assert!(!rows.is_empty(), "expected history for a known address");
        assert!(rows.iter().all(|r| r.tx_hash.is_some()));
        assert!(rows.iter().all(|r| r.decimals > 0));
        // Real Unix dates. Discovered rows were once dated centuries ahead.
        assert!(rows.iter().all(|r| r.created_at > 1_400_000_000 && r.created_at < 4_000_000_000));

        // Token transfers and native transfers are separate endpoints. If only
        // the first were read this would fail, and incoming ETH would be
        // silently missing in production.
        assert!(
            rows.iter().any(|r| r.asset_id.ends_with(":native")),
            "plain coin transfers must be read from transactions"
        );
        assert!(
            rows.iter().any(|r| !r.asset_id.ends_with(":native")),
            "token transfers must be read from token-transfers"
        );
        assert!(
            rows.iter().any(|r| r.kind == history_engine::HistoryKind::Received),
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
        let rows = Discovery::new("history-engine-tests").expect("client").scan(&target).await.expect("scan");
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
        let rows = Discovery::new("history-engine-tests").expect("client").scan(&target).await.expect("scan");
        println!("bitcoin rows: {}", rows.len());
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.asset_id == "bitcoin:native"));
        assert!(rows.iter().all(|r| r.decimals == 8));
        assert!(rows.iter().all(|r| r.created_at > 1_230_000_000 && r.created_at < 4_000_000_000), "dated by block");

        // Amounts are derived from inputs/outputs, so at least one row must
        // carry a non-zero value. A previous implementation reported every
        // Bitcoin transfer as zero because the endpoint had no totals.
        assert!(
            rows.iter().any(|r| r.amount_base != "0"),
            "bitcoin rows must carry derived amounts, not zeros"
        );
        assert!(
            rows.iter().all(|r| r.kind == history_engine::HistoryKind::Sent
                || r.kind == history_engine::HistoryKind::Received),
            "direction must be derived from inputs and outputs"
        );
    });
}

#[test]
#[ignore]
fn both_chain_kinds_read_through_the_public_entry_point() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let discovery = Discovery::new("history-engine-tests").expect("client");
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
    assert!(!Discovery::new("history-engine-tests").unwrap().supports(&ChainInfo::Evm(chain)));
}

/// Sends and receives through Alchemy, on every chain that declares it.
/// Needs a key, or an endpoint template naming the network (a proxy):
///
///   ALCHEMY_API_KEY=KEY cargo test --test live alchemy -- --ignored --nocapture
#[test]
#[ignore]
fn alchemy_reads_sends_and_receives_on_every_declared_chain() {
    let template = match (std::env::var("ALCHEMY_HISTORY_URL"), std::env::var("ALCHEMY_API_KEY")) {
        (Ok(template), _) => template,
        (_, Ok(key)) => format!("https://{{network}}.g.alchemy.com/v2/{key}"),
        _ => {
            println!("ALCHEMY_API_KEY not set: skipped");
            return;
        }
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let discovery = history_engine::Discovery::with_endpoints(
            "history-engine-tests",
            history_engine::Endpoints { alchemy: Some(template) },
        )
        .expect("endpoint");
        for chain in registry::chains::builtin_only() {
            let ChainInfo::Evm(mut evm) = chain else { continue };
            // Testnets: the busy address below has no history there.
            if evm.history_providers.is_empty() || evm.testnet {
                continue;
            }
            // Alchemy alone: a Blockscout fallback would hide a broken provider.
            evm.history_api = None;
            let name = evm.name.clone();
            let chain_id = evm.chain_id;
            let store = history_engine::HistoryStore::open(&std::env::temp_dir().join(format!("history-live-{}", evm.id))).unwrap();
            let target = ScanTarget { wallet_id: "live".into(), chain: ChainInfo::Evm(evm), address: BUSY_ADDRESS.into() };
            let rows = discovery.scan(&target).await.unwrap_or_else(|e| panic!("{name}: {e}"));
            let sent = rows.iter().filter(|r| r.kind == history_engine::HistoryKind::Sent).count();
            let received = rows.len() - sent;
            println!("{name}: {sent} sent, {received} received");
            // vitalik.eth has received on Ethereum for years; on a newly added
            // chain it may have nothing, so elsewhere the answer and its dates
            // are what is checked.
            if chain_id == 1 {
                assert!(received > 0, "{name}: a public address this busy has receives");
            }
            assert!(rows.iter().all(|r| r.created_at > 1_400_000_000 && r.created_at < 4_000_000_000), "{name}: real dates");
            // A second sync starts from the cursor and adds little or nothing.
            discovery.sync(&store, &target).await.unwrap();
            discovery.sync(&store, &target).await.unwrap();
        }
    });
}