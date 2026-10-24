//! Solana history, read from the chain's own RPC.
//!
//! A Solana node indexes transactions by the accounts they touch, so there is
//! no explorer to ask. Two things shape this reader:
//!
//! - A token sent to this wallet touches the wallet's *token account*, not the
//!   wallet address. Asking only for the wallet's own signatures would miss
//!   every incoming token transfer, so each token account is asked as well.
//! - A transaction is a list of instructions, not "a transfer". What it did to
//!   this wallet is read from the balances the node reports before and after
//!   it, which is right whatever the instructions were.

use crate::error::{HistoryError, HistoryResult};
use crate::model::{HistoryKind, HistoryStatus, NewEntry};
use registry::model::{ChainInfo, SolanaChain};
use serde_json::{json, Value};
use std::collections::HashMap;

const TOKEN_PROGRAMS: [&str; 2] = [
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
];
/// How far back to look. Public endpoints allow a few requests a second, and
/// each transaction is its own request.
const WALLET_SIGNATURES: usize = 15;
const TOKEN_ACCOUNT_SIGNATURES: usize = 5;
const MAX_TRANSACTIONS: usize = 30;

/// One JSON-RPC call, tried against each endpoint until one answers.
async fn rpc(client: &reqwest::Client, endpoints: &[String], method: &str, params: Value) -> HistoryResult<Value> {
    let mut last = "chain has no RPC endpoints".to_string();
    for endpoint in endpoints {
        let sent = client
            .post(endpoint)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await;
        match sent {
            Ok(response) => match response.json::<Value>().await {
                Ok(body) => match body.get("result") {
                    Some(result) => return Ok(result.clone()),
                    None => last = format!("{method} was refused"),
                },
                Err(e) => last = format!("bad response: {e}"),
            },
            Err(e) => last = format!("request failed: {e}"),
        }
    }
    Err(HistoryError::Source(last))
}

async fn signatures(client: &reqwest::Client, chain: &SolanaChain, account: &str, limit: usize) -> HistoryResult<Vec<String>> {
    let result = rpc(client, &chain.rpc, "getSignaturesForAddress", json!([account, { "limit": limit }])).await?;
    Ok(result
        .as_array()
        .map(|list| {
            list.iter()
                // A failed transaction moved nothing but its fee.
                .filter(|s| s.get("err").is_none_or(Value::is_null))
                .filter_map(|s| s.get("signature")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// The wallet's token accounts for tokens the registry knows.
async fn token_accounts(client: &reqwest::Client, chain: &SolanaChain, owner: &str) -> Vec<String> {
    let mut accounts = Vec::new();
    for program in TOKEN_PROGRAMS {
        let params = json!([owner, { "programId": program }, { "encoding": "jsonParsed" }]);
        let Ok(result) = rpc(client, &chain.rpc, "getTokenAccountsByOwner", params).await else {
            continue;
        };
        for entry in result.get("value").and_then(Value::as_array).into_iter().flatten() {
            let mint = entry.pointer("/account/data/parsed/info/mint").and_then(Value::as_str);
            let known = mint.is_some_and(|m| chain.default_tokens.iter().any(|t| t.address == m));
            if let (true, Some(address)) = (known, entry.get("pubkey").and_then(Value::as_str)) {
                accounts.push(address.to_string());
            }
        }
    }
    accounts
}

pub async fn read(
    client: &reqwest::Client,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
) -> HistoryResult<Vec<NewEntry>> {
    let ChainInfo::Solana(sol) = chain else {
        return Err(HistoryError::Source("not a Solana chain".into()));
    };

    // The wallet's own signatures must be readable; a token account's failing
    // only narrows what is found.
    let mut found = signatures(client, sol, address, WALLET_SIGNATURES).await?;
    for account in token_accounts(client, sol, address).await {
        for signature in signatures(client, sol, &account, TOKEN_ACCOUNT_SIGNATURES).await.unwrap_or_default() {
            if !found.contains(&signature) {
                found.push(signature);
            }
        }
    }
    found.truncate(MAX_TRANSACTIONS);

    let mut entries = Vec::new();
    for signature in found {
        let params = json!([signature, { "encoding": "jsonParsed", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]);
        // One transaction that cannot be read is skipped, not fatal.
        let Ok(tx) = rpc(client, &sol.rpc, "getTransaction", params).await else {
            continue;
        };
        if let Some(entry) = entry_from(wallet_id, sol, address, &signature, &tx) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Token balances in one of the node's before/after lists, as
/// (account owner, mint) to base units.
fn token_balances(meta: &Value, list: &str) -> HashMap<(String, String), i128> {
    let mut out = HashMap::new();
    for balance in meta.get(list).and_then(Value::as_array).into_iter().flatten() {
        let (Some(owner), Some(mint), Some(amount)) = (
            balance.get("owner").and_then(Value::as_str),
            balance.get("mint").and_then(Value::as_str),
            balance.pointer("/uiTokenAmount/amount").and_then(Value::as_str).and_then(|a| a.parse::<i128>().ok()),
        ) else {
            continue;
        };
        *out.entry((owner.to_string(), mint.to_string())).or_default() += amount;
    }
    out
}

/// What one transaction did to this wallet, as a history row.
///
/// A known token moving takes precedence over SOL moving: in a token transfer
/// the SOL change is only the fee or an account deposit. Tokens the registry
/// does not know are left out, which also keeps airdropped spam out of the
/// feed. `None` when nothing of the wallet's moved.
fn entry_from(wallet_id: &str, chain: &SolanaChain, address: &str, signature: &str, tx: &Value) -> Option<NewEntry> {
    let meta = tx.get("meta")?;
    if meta.get("err").is_some_and(|e| !e.is_null()) {
        return None;
    }
    let fee = meta.get("fee").and_then(Value::as_u64).unwrap_or(0);
    let at = tx.get("blockTime").and_then(Value::as_i64).unwrap_or_else(crate::model::now_secs);

    let before = token_balances(meta, "preTokenBalances");
    let after = token_balances(meta, "postTokenBalances");
    let change = |owner: &str, mint: &str| {
        let key = (owner.to_string(), mint.to_string());
        after.get(&key).copied().unwrap_or(0) - before.get(&key).copied().unwrap_or(0)
    };

    let token = chain.default_tokens.iter().find_map(|token| {
        let delta = change(address, &token.address);
        (delta != 0).then_some((token, delta))
    });

    let (asset, symbol, decimals, delta, other) = match token {
        Some((token, delta)) => {
            // Whoever's balance of the same token moved the other way.
            let other = before
                .keys()
                .chain(after.keys())
                .filter(|(owner, mint)| owner != address && *mint == token.address)
                .find(|(owner, mint)| change(owner, mint).signum() == -delta.signum())
                .map(|(owner, _)| owner.clone());
            (token.address.clone(), token.symbol.clone(), token.decimals, delta, other)
        }
        None => {
            let keys: Vec<&str> = tx
                .pointer("/transaction/message/accountKeys")?
                .as_array()?
                .iter()
                .filter_map(|k| k.get("pubkey").and_then(Value::as_str).or_else(|| k.as_str()))
                .collect();
            let lamports = |list: &str, index: usize| -> Option<i128> {
                meta.get(list)?.as_array()?.get(index)?.as_i64().map(i128::from)
            };
            let moved = |index: usize| Some(lamports("postBalances", index)? - lamports("preBalances", index)?);
            let ours = keys.iter().position(|k| *k == address)?;
            // The first account pays the fee, which is not part of what was sent.
            let delta = moved(ours)? + if ours == 0 { i128::from(fee) } else { 0 };
            if delta == 0 {
                return None;
            }
            let other = (0..keys.len())
                .filter(|i| *i != ours)
                .filter_map(|i| Some((i, moved(i)?)))
                .filter(|(_, d)| d.signum() == -delta.signum())
                .max_by_key(|(_, d)| d.abs())
                .map(|(i, _)| keys[i].to_string());
            let native = &chain.native_currency;
            ("native".to_string(), native.symbol.clone(), native.decimals, delta, other)
        }
    };

    let received = delta > 0;
    let symbol = super::clean(&symbol, 16);
    Some(NewEntry {
        wallet_id: wallet_id.to_string(),
        local_id: format!("{signature}:{symbol}"),
        kind: if received { HistoryKind::Received } else { HistoryKind::Sent },
        status: HistoryStatus::Confirmed,
        asset_id: format!("solana:{}:{asset}", chain.id),
        symbol,
        chain_id: chain.id.clone(),
        chain_name: chain.name.clone(),
        to_address: if received { address.to_string() } else { other.clone().unwrap_or_default() },
        amount_base: delta.abs().to_string(),
        fee_base: if received { "0".to_string() } else { fee.to_string() },
        decimals,
        price_usd: 0.0,
        tx_hash: Some(signature.to_string()),
        explorer_url: chain
            .explorers
            .first()
            .filter(|e| !e.tx_url.is_empty())
            .map(|e| e.tx_url.replace("{tx}", signature)),
        counterparty: other,
        error: None,
        created_at: at,
        confirmed_at: Some(at),
        received: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both recorded from mainnet for this wallet.
    const WALLET: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";
    const SOL_SEND: &str = include_str!("testdata/solana_sol.json");
    const TOKEN_RECEIVE: &str = include_str!("testdata/solana_token.json");

    fn chain() -> SolanaChain {
        registry::chains::builtin_only()
            .into_iter()
            .find_map(|c| match c {
                ChainInfo::Solana(s) if s.id == "solana" => Some(s),
                _ => None,
            })
            .expect("solana is built in")
    }

    fn entry(body: &str) -> Option<NewEntry> {
        entry_from("w", &chain(), WALLET, "sig", &serde_json::from_str(body).expect("fixture parses"))
    }

    #[test]
    fn a_sol_send_is_the_amount_without_the_fee() {
        let entry = entry(SOL_SEND).expect("a row");
        assert_eq!(entry.kind, HistoryKind::Sent);
        assert_eq!(entry.symbol, "SOL");
        assert_eq!(entry.asset_id, "solana:solana:native");
        assert_eq!(entry.decimals, 9);
        // The wallet's balance fell by 324354000, of which 11000 was the fee.
        assert_eq!(entry.amount_base, "324343000");
        assert_eq!(entry.fee_base, "11000");
        assert!(entry.counterparty.is_some(), "the account that gained it");
        assert_eq!(entry.to_address, entry.counterparty.clone().unwrap());
        assert_eq!(entry.explorer_url.as_deref(), Some("https://solscan.io/tx/sig"));
        assert!(entry.created_at > 1_700_000_000);
    }

    #[test]
    fn a_token_received_is_found_though_the_wallet_is_not_in_the_transaction() {
        let tx: Value = serde_json::from_str(TOKEN_RECEIVE).unwrap();
        let keys = tx.pointer("/transaction/message/accountKeys").unwrap().to_string();
        assert!(!keys.contains(WALLET), "only the wallet's token account is named");

        let entry = entry(TOKEN_RECEIVE).expect("a row");
        assert_eq!(entry.kind, HistoryKind::Received);
        assert_eq!(entry.symbol, "USDC");
        assert_eq!(entry.asset_id, "solana:solana:EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(entry.decimals, 6);
        assert_eq!(entry.amount_base, "179300000");
        assert_eq!(entry.fee_base, "0", "the sender paid it");
        assert_eq!(entry.to_address, WALLET);
        assert!(entry.counterparty.as_deref().is_some_and(|c| c.starts_with("CWKMxFuf")));
    }

    #[test]
    fn what_did_not_move_the_wallet_makes_no_row() {
        // The same transfer, seen from an address that took no part.
        let stranger = "EN2sCsJ1WDV8UFqsiTXHcUPUxQ4juE71eCknHYYMifkd";
        let tx: Value = serde_json::from_str(TOKEN_RECEIVE).unwrap();
        assert!(entry_from("w", &chain(), stranger, "sig", &tx).is_none());

        // A failed transaction.
        let mut failed: Value = serde_json::from_str(SOL_SEND).unwrap();
        failed["meta"]["err"] = json!({ "InstructionError": [0, "Custom"] });
        assert!(entry_from("w", &chain(), WALLET, "sig", &failed).is_none());

        // A token the registry does not know, with no SOL of ours moving.
        let mut unknown = chain();
        unknown.default_tokens.clear();
        assert!(entry_from("w", &unknown, WALLET, "sig", &tx).is_none());

        assert!(entry_from("w", &chain(), WALLET, "sig", &json!({})).is_none());
        assert!(entry_from("w", &chain(), WALLET, "sig", &json!({ "meta": { "err": null } })).is_none());
    }

    /// Talks to mainnet: `cargo test --manifest-path history/Cargo.toml solana -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_history_is_read_from_the_chain() {
        // An active wallet that holds USDC, so both kinds of signature list are asked for.
        let wallet = "CWKMxFufwzFWvSsyVRaDWGC9YJzZzCxsPoPzT1kz3d2M";
        let client = crate::chains::http_client("history-engine-tests").unwrap();
        let rows = read(&client, "w", &ChainInfo::Solana(chain()), wallet).await.expect("readable");
        for row in rows.iter().take(6) {
            println!("live history: {:?} {} {} (10^-{}) fee {} with {:?}", row.kind, row.amount_base, row.symbol, row.decimals, row.fee_base, row.counterparty);
        }
        assert!(!rows.is_empty(), "an active wallet has history");
        for row in &rows {
            assert!(row.tx_hash.is_some() && row.amount_base != "0");
            assert!(row.asset_id.starts_with("solana:solana:"));
            assert!(row.explorer_url.as_deref().is_some_and(|u| u.starts_with("https://solscan.io/tx/")));
        }
        let mut hashes: Vec<_> = rows.iter().map(|r| r.tx_hash.clone()).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), rows.len(), "one row per transaction");
    }

    #[test]
    fn paying_only_a_fee_is_not_a_send() {
        let mut tx: Value = serde_json::from_str(SOL_SEND).unwrap();
        // The wallet's balance falls by exactly the fee.
        let pre = tx["meta"]["preBalances"][0].as_i64().unwrap();
        let fee = tx["meta"]["fee"].as_i64().unwrap();
        tx["meta"]["postBalances"][0] = json!(pre - fee);
        assert!(entry_from("w", &chain(), WALLET, "sig", &tx).is_none());
    }
}
