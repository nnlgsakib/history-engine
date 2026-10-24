//! SQLite persistence for history.
//!
//! Owns its own migrations, independent of the app's cache database. History
//! is a financial record: it must not be wiped when the cache is purged, and
//! it must never be *replaced* wholesale the way a cache snapshot is.

use crate::error::{HistoryError, HistoryResult};
use crate::model::{now_secs, HistoryEntry, HistoryKind, HistoryStatus, KindFilter, NewEntry, Received};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const SCHEMA_VERSION: i64 = 2;

/// Append-only. Never edit an applied entry — add a new one.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS history (
        local_id     TEXT    PRIMARY KEY,
        wallet_id    TEXT    NOT NULL,
        kind         TEXT    NOT NULL,
        status       TEXT    NOT NULL,
        asset_id     TEXT    NOT NULL,
        symbol       TEXT    NOT NULL,
        chain_id     TEXT    NOT NULL,
        chain_name   TEXT    NOT NULL,
        to_address   TEXT    NOT NULL,
        amount_base  TEXT    NOT NULL,
        fee_base     TEXT    NOT NULL DEFAULT '0',
        decimals     INTEGER NOT NULL,
        price_usd    REAL    NOT NULL DEFAULT 0,
        tx_hash      TEXT,
        explorer_url TEXT,
        counterparty TEXT,
        error        TEXT,
        discovered   INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        confirmed_at INTEGER
    );

    -- Wallet-scoped newest-first reads are the only access pattern.
    CREATE INDEX IF NOT EXISTS history_wallet_time ON history (wallet_id, created_at DESC);
    CREATE INDEX IF NOT EXISTS history_kind ON history (wallet_id, kind);

    -- tx_hash is unique per wallet so a discovered transfer and a local send
    -- of the same on-chain transaction collapse to one row instead of
    -- double-counting. Partial index: local rows without a hash still insert.
    CREATE UNIQUE INDEX IF NOT EXISTS history_wallet_hash
        ON history (wallet_id, tx_hash) WHERE tx_hash IS NOT NULL;

    CREATE TABLE IF NOT EXISTS history_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // What a swap returned. In one transaction, so a crash part way cannot
    // leave some columns added and the step unrecorded.
    r#"
    BEGIN;
    ALTER TABLE history ADD COLUMN recv_symbol      TEXT;
    ALTER TABLE history ADD COLUMN recv_amount_base TEXT;
    ALTER TABLE history ADD COLUMN recv_decimals    INTEGER;
    ALTER TABLE history ADD COLUMN recv_chain_id    TEXT;
    ALTER TABLE history ADD COLUMN recv_chain_name  TEXT;
    COMMIT;
    "#,
];

/// Every column a row is read with, in the order `read_row` expects.
const COLUMNS: &str = "local_id, kind, status, asset_id, symbol, chain_id, chain_name,
                       to_address, amount_base, fee_base, decimals, price_usd,
                       tx_hash, explorer_url, counterparty, error, created_at,
                       confirmed_at, discovered, recv_symbol, recv_amount_base,
                       recv_decimals, recv_chain_id, recv_chain_name";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    // A swap row has all five; anything else has none.
    let received = match (
        r.get::<_, Option<String>>(19)?,
        r.get::<_, Option<String>>(20)?,
        r.get::<_, Option<i64>>(21)?,
        r.get::<_, Option<String>>(22)?,
        r.get::<_, Option<String>>(23)?,
    ) {
        (Some(symbol), Some(amount_base), Some(decimals), Some(chain_id), Some(chain_name)) => Some(Received {
            symbol,
            amount_base,
            decimals: decimals as u8,
            chain_id,
            chain_name,
        }),
        _ => None,
    };
    Ok(HistoryEntry {
        local_id: r.get(0)?,
        kind: HistoryKind::from_str_lossy(r.get::<_, String>(1)?.as_str()),
        status: HistoryStatus::from_str_lossy(r.get::<_, String>(2)?.as_str()),
        asset_id: r.get(3)?,
        symbol: r.get(4)?,
        chain_id: r.get(5)?,
        chain_name: r.get(6)?,
        to_address: r.get(7)?,
        amount_base: r.get(8)?,
        fee_base: r.get(9)?,
        decimals: r.get::<_, i64>(10)? as u8,
        price_usd: r.get(11)?,
        tx_hash: r.get(12)?,
        explorer_url: r.get(13)?,
        counterparty: r.get(14)?,
        error: r.get(15)?,
        created_at: r.get(16)?,
        confirmed_at: r.get(17)?,
        discovered: r.get::<_, i64>(18)? != 0,
        received,
    })
}

pub struct HistoryStore {
    conn: Mutex<Connection>,
}

fn lock<T>(m: &Mutex<T>) -> HistoryResult<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| HistoryError::Store("state poisoned".into()))
}

impl HistoryStore {
    /// Opens (and migrates) the history database at `app_data/history.sqlite`.
    pub fn open(app_data: &Path) -> HistoryResult<HistoryStore> {
        std::fs::create_dir_all(app_data)
            .map_err(|_| HistoryError::Store("cannot create app data dir".into()))?;
        let conn = Connection::open(app_data.join("history.sqlite"))
            .map_err(|_| HistoryError::Store("cannot open history database".into()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> HistoryResult<HistoryStore> {
        Self::init(Connection::open_in_memory().map_err(|_| HistoryError::Store("no memory db".into()))?)
    }

    fn init(mut conn: Connection) -> HistoryResult<HistoryStore> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Self::migrate(&mut conn)?;
        Ok(HistoryStore {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn migrate(conn: &mut Connection) -> HistoryResult<()> {
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM history_meta WHERE key = 'schema_version'), 0)",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0);

        if current > SCHEMA_VERSION {
            return Err(HistoryError::Store(
                "history was written by a newer app version — reinstall required".into(),
            ));
        }

        for (index, sql) in MIGRATIONS.iter().enumerate() {
            let version = index as i64 + 1;
            if version <= current {
                continue;
            }
            conn.execute_batch(sql)
                .map_err(|_| HistoryError::Store("migration failed".into()))?;
            // Recorded only after the batch succeeds, so an interrupted
            // migration re-runs rather than being marked done.
            conn.execute(
                "INSERT INTO history_meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![version.to_string()],
            )
            .map_err(|_| HistoryError::Store("cannot record schema version".into()))?;
        }
        Ok(())
    }

    /// Records a row. Idempotent by `local_id`, and by `(wallet_id, tx_hash)`
    /// so a discovered transfer does not duplicate a local send.
    ///
    /// An existing row for the same hash is *left alone*: the local record is
    /// the authority on fee and status, since it knows what this device paid.
    pub fn record(&self, entry: &NewEntry, discovered: bool) -> HistoryResult<()> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO history
               (local_id, wallet_id, kind, status, asset_id, symbol, chain_id,
                chain_name, to_address, amount_base, fee_base, decimals,
                price_usd, tx_hash, explorer_url, counterparty, error,
                discovered, created_at, confirmed_at, recv_symbol,
                recv_amount_base, recv_decimals, recv_chain_id, recv_chain_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
            params![
                entry.local_id,
                entry.wallet_id,
                entry.kind.as_str(),
                entry.status.as_str(),
                entry.asset_id,
                entry.symbol,
                entry.chain_id,
                entry.chain_name,
                entry.to_address,
                entry.amount_base,
                entry.fee_base,
                i64::from(entry.decimals),
                entry.price_usd,
                entry.tx_hash,
                entry.explorer_url,
                entry.counterparty,
                entry.error,
                i64::from(discovered),
                entry.created_at,
                entry.confirmed_at,
                entry.received.as_ref().map(|r| r.symbol.as_str()),
                entry.received.as_ref().map(|r| r.amount_base.as_str()),
                entry.received.as_ref().map(|r| i64::from(r.decimals)),
                entry.received.as_ref().map(|r| r.chain_id.as_str()),
                entry.received.as_ref().map(|r| r.chain_name.as_str()),
            ],
        )
        .map_err(|_| HistoryError::Store("write failed".into()))?;
        Ok(())
    }

    /// Settles a pending local row once the broadcast outcome is known.
    ///
    /// Guarded on `status = 'pending'` so a late retry cannot overwrite a
    /// confirmed send with a failure.
    pub fn settle(
        &self,
        local_id: &str,
        status: HistoryStatus,
        tx_hash: Option<&str>,
        explorer_url: Option<&str>,
        error: Option<&str>,
    ) -> HistoryResult<()> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE history
                SET status = ?1,
                    tx_hash = COALESCE(?2, tx_hash),
                    explorer_url = COALESCE(?3, explorer_url),
                    error = ?4,
                    confirmed_at = ?5
              WHERE local_id = ?6 AND status = 'pending'",
            params![status.as_str(), tx_hash, explorer_url, error, now_secs(), local_id],
        )
        .map_err(|_| HistoryError::Store("settle failed".into()))?;
        Ok(())
    }

    pub fn list(
        &self,
        wallet_id: &str,
        kind: Option<KindFilter>,
        limit: usize,
    ) -> HistoryResult<Vec<HistoryEntry>> {
        let limit = limit.clamp(1, 1000);
        let conn = lock(&self.conn)?;
        let columns = COLUMNS;
        let map = read_row;

        let mut stmt = match kind {
            None => conn
                .prepare(&format!(
                    "SELECT {columns} FROM history
                      WHERE wallet_id = ?1
                      ORDER BY created_at DESC, local_id DESC
                      LIMIT ?2"
                ))
                .map_err(|_| HistoryError::Store("read failed".into()))?,
            Some(_) => conn
                .prepare(&format!(
                    "SELECT {columns} FROM history
                      WHERE wallet_id = ?1 AND kind = ?2
                      ORDER BY created_at DESC, local_id DESC
                      LIMIT ?3"
                ))
                .map_err(|_| HistoryError::Store("read failed".into()))?,
        };

        let rows = match kind {
            Some(k) => stmt.query_map(params![wallet_id, k.as_str(), limit as i64], map),
            None => stmt.query_map(params![wallet_id, limit as i64], map),
        }
        .map_err(|_| HistoryError::Store("read failed".into()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| HistoryError::Store("read failed".into()))
    }

    pub fn get(&self, local_id: &str) -> HistoryResult<Option<HistoryEntry>> {
        self.list_all_for(local_id)
            .map(|rows| rows.into_iter().next())
    }

    fn list_all_for(&self, local_id: &str) -> HistoryResult<Vec<HistoryEntry>> {
        let conn = lock(&self.conn)?;
        let mut stmt = conn
            .prepare(&format!("SELECT {COLUMNS} FROM history WHERE local_id = ?1"))
            .map_err(|_| HistoryError::Store("read failed".into()))?;
        let rows = stmt
            .query_map(params![local_id], read_row)
            .map_err(|_| HistoryError::Store("read failed".into()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| HistoryError::Store("read failed".into()))
    }

    /// Gives a pending row its transaction hash without settling it.
    ///
    /// A swap is broadcast long before its outcome is known, and the hash is
    /// what the outcome is later looked up by.
    pub fn attach_hash(&self, local_id: &str, tx_hash: &str, explorer_url: Option<&str>) -> HistoryResult<()> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE history
                SET tx_hash = ?1, explorer_url = COALESCE(?2, explorer_url)
              WHERE local_id = ?3 AND status = 'pending'",
            params![tx_hash, explorer_url, local_id],
        )
        .map_err(|_| HistoryError::Store("write failed".into()))?;
        Ok(())
    }

    /// Replaces a swap's estimated proceeds with the amount that arrived.
    /// Only while pending: a settled row is not rewritten.
    pub fn set_received_amount(&self, local_id: &str, amount_base: &str) -> HistoryResult<()> {
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE history
                SET recv_amount_base = ?1
              WHERE local_id = ?2 AND status = 'pending' AND recv_symbol IS NOT NULL",
            params![amount_base, local_id],
        )
        .map_err(|_| HistoryError::Store("write failed".into()))?;
        Ok(())
    }

    /// Per-asset history, newest first.
    pub fn list_for_asset(
        &self,
        wallet_id: &str,
        asset_id: &str,
        limit: usize,
    ) -> HistoryResult<Vec<HistoryEntry>> {
        Ok(self
            .list(wallet_id, None, 1000)?
            .into_iter()
            .filter(|e| e.asset_id == asset_id)
            .take(limit.clamp(1, 500))
            .collect())
    }

    /// Earliest stored timestamp for a wallet, used to bound discovery scans so
    /// a wallet with a long history does not walk the whole chain.
    pub fn oldest_timestamp(&self, wallet_id: &str) -> HistoryResult<Option<i64>> {
        let conn = lock(&self.conn)?;
        // MIN() returns NULL for an empty set, which is a legitimate answer,
        // not a failure.
        conn.query_row(
            "SELECT MIN(created_at) FROM history WHERE wallet_id = ?1",
            params![wallet_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|_| HistoryError::Store("read failed".into()))
    }

    /// Refiles rows of one chain from an old asset-id prefix to the current
    /// one. Returns how many rows changed; running it again changes none.
    pub fn rename_asset_prefix(&self, chain_id: &str, from: &str, to: &str) -> HistoryResult<usize> {
        if from == to {
            return Ok(0);
        }
        let conn = lock(&self.conn)?;
        conn.execute(
            "UPDATE history
                SET asset_id = ?3 || substr(asset_id, length(?2) + 1)
              WHERE chain_id = ?1 AND substr(asset_id, 1, length(?2) + 1) = ?2 || ':'",
            params![chain_id, from, to],
        )
        .map_err(|_| HistoryError::Store("asset id update failed".into()))
    }

    /// Removes a wallet's history. For wallet deletion only — never on lock.
    pub fn purge_wallet(&self, wallet_id: &str) -> HistoryResult<()> {
        let conn = lock(&self.conn)?;
        conn.execute("DELETE FROM history WHERE wallet_id = ?1", params![wallet_id])
            .map_err(|_| HistoryError::Store("purge failed".into()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(wallet: &str, local: &str, kind: HistoryKind, amount: &str) -> NewEntry {
        NewEntry {
            wallet_id: wallet.to_string(),
            local_id: local.to_string(),
            kind,
            status: HistoryStatus::Pending,
            asset_id: "evm:1:native".to_string(),
            symbol: "ETH".to_string(),
            chain_id: "1".to_string(),
            chain_name: "Ethereum".to_string(),
            to_address: "0xabc".to_string(),
            amount_base: amount.to_string(),
            fee_base: "1000".to_string(),
            decimals: 18,
            price_usd: 3000.0,
            tx_hash: None,
            explorer_url: None,
            counterparty: None,
            error: None,
            created_at: now_secs(),
            confirmed_at: None,
            received: None,
        }
    }

    fn store() -> HistoryStore {
        HistoryStore::open_in_memory().expect("store")
    }

    #[test]
    fn record_and_read_back() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1000"), false)
            .expect("record");
        let rows = s.list("w", None, 10).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].amount_base, "1000");
        assert!(!rows[0].discovered);
    }

    #[test]
    fn duplicate_local_id_is_ignored() {
        let s = store();
        let e = entry("w", "l1", HistoryKind::Sent, "1");
        s.record(&e, false).expect("first");
        s.record(&e, false).expect("retry");
        assert_eq!(s.list("w", None, 10).unwrap().len(), 1);
    }

    /// The dedupe that matters: a discovered incoming transfer and a local
    /// send of the same on-chain transaction must collapse to one row.
    #[test]
    fn discovered_and_local_rows_with_same_hash_collapse() {
        let s = store();
        let mut local = entry("w", "local1", HistoryKind::Sent, "1000");
        local.tx_hash = Some("0xdead".into());
        s.record(&local, false).expect("local");

        let mut found = entry("w", "disc1", HistoryKind::Received, "1000");
        found.tx_hash = Some("0xdead".into());
        s.record(&found, true).expect("discovered");

        let rows = s.list("w", None, 10).expect("list");
        assert_eq!(rows.len(), 1, "same tx must not appear twice");
        assert_eq!(rows[0].local_id, "local1", "local record wins");
        assert!(!rows[0].discovered);
    }

    #[test]
    fn same_hash_in_different_wallets_is_allowed() {
        let s = store();
        let mut a = entry("a", "la", HistoryKind::Sent, "1");
        a.tx_hash = Some("0xsame".into());
        s.record(&a, false).expect("a");
        let mut b = entry("b", "lb", HistoryKind::Received, "1");
        b.tx_hash = Some("0xsame".into());
        s.record(&b, true).expect("b");
        assert_eq!(s.list("a", None, 10).unwrap().len(), 1);
        assert_eq!(s.list("b", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn rows_without_hash_all_insert() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1"), false)
            .expect("1");
        s.record(&entry("w", "l2", HistoryKind::Sent, "2"), false)
            .expect("2");
        assert_eq!(s.list("w", None, 10).unwrap().len(), 2);
    }

    #[test]
    fn settle_marks_confirmed() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1000"), false)
            .expect("record");
        s.settle("l1", HistoryStatus::Confirmed, Some("0xh"), Some("https://e"), None)
            .expect("settle");
        let row = s.get("l1").unwrap().expect("row");
        assert_eq!(row.status, HistoryStatus::Confirmed);
        assert_eq!(row.tx_hash.as_deref(), Some("0xh"));
        assert!(row.confirmed_at.is_some());
    }

    #[test]
    fn settle_records_failure_reason() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1000"), false)
            .expect("record");
        s.settle("l1", HistoryStatus::Failed, None, None, Some("reverted"))
            .expect("settle");
        let row = s.get("l1").unwrap().expect("row");
        assert_eq!(row.status, HistoryStatus::Failed);
        assert_eq!(row.error.as_deref(), Some("reverted"));
    }

    /// A late retry must not turn a confirmed send into a failure.
    #[test]
    fn settle_cannot_overwrite_a_settled_row() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1"), false)
            .expect("record");
        s.settle("l1", HistoryStatus::Confirmed, Some("0xgood"), None, None)
            .expect("confirm");
        s.settle("l1", HistoryStatus::Failed, None, None, Some("late"))
            .expect("late");
        let row = s.get("l1").unwrap().expect("row");
        assert_eq!(row.status, HistoryStatus::Confirmed);
        assert_eq!(row.tx_hash.as_deref(), Some("0xgood"));
    }

    #[test]
    fn history_is_scoped_per_wallet() {
        let s = store();
        s.record(&entry("a", "la", HistoryKind::Sent, "1"), false)
            .expect("a");
        s.record(&entry("b", "lb", HistoryKind::Received, "2"), true)
            .expect("b");
        assert_eq!(s.list("a", None, 10).unwrap().len(), 1);
        assert_eq!(s.list("a", None, 10).unwrap()[0].local_id, "la");
    }

    #[test]
    fn filters_by_kind() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "1"), false)
            .expect("sent");
        s.record(&entry("w", "l2", HistoryKind::Received, "2"), true)
            .expect("received");
        s.record(&entry("w", "l3", HistoryKind::Swap, "3"), true)
            .expect("swap");
        assert_eq!(s.list("w", Some(KindFilter::Sent), 10).unwrap().len(), 1);
        assert_eq!(
            s.list("w", Some(KindFilter::Received), 10).unwrap()[0].local_id,
            "l2"
        );
        assert_eq!(s.list("w", Some(KindFilter::Swap), 10).unwrap().len(), 1);
    }

    #[test]
    fn orders_newest_first_and_respects_limit() {
        let s = store();
        for i in 0..5 {
            let mut e = entry("w", &format!("l{i}"), HistoryKind::Sent, "1");
            e.created_at = now_secs() + i;
            s.record(&e, false).expect("record");
        }
        let rows = s.list("w", None, 3).expect("list");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].local_id, "l4", "newest first");
        assert_eq!(rows[2].local_id, "l2");
    }

    #[test]
    fn filters_by_asset() {
        let s = store();
        let mut eth = entry("w", "l1", HistoryKind::Sent, "1");
        eth.asset_id = "evm:1:native".into();
        s.record(&eth, false).expect("eth");
        let mut btc = entry("w", "l2", HistoryKind::Sent, "1");
        btc.asset_id = "bitcoin:native".into();
        s.record(&btc, false).expect("btc");
        assert_eq!(s.list_for_asset("w", "evm:1:native", 10).unwrap().len(), 1);
        assert_eq!(s.list_for_asset("w", "bitcoin:native", 10).unwrap().len(), 1);
        assert!(s.list_for_asset("w", "evm:137:native", 10).unwrap().is_empty());
    }

    /// A u256-max balance must not lose precision through a float.
    #[test]
    fn amounts_keep_full_precision() {
        let s = store();
        let huge = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        s.record(&entry("w", "l1", HistoryKind::Sent, huge), false)
            .expect("record");
        assert_eq!(s.get("l1").unwrap().unwrap().amount_base, huge);
    }

    #[test]
    fn reports_oldest_timestamp_for_scan_bounds() {
        let s = store();
        assert!(s.oldest_timestamp("w").unwrap().is_none());
        let mut old = entry("w", "l1", HistoryKind::Sent, "1");
        old.created_at = 1_000;
        s.record(&old, false).expect("record");
        let mut new = entry("w", "l2", HistoryKind::Sent, "1");
        new.created_at = 2_000;
        s.record(&new, false).expect("record");
        assert_eq!(s.oldest_timestamp("w").unwrap(), Some(1_000));
    }

    #[test]
    fn purge_removes_only_that_wallet() {
        let s = store();
        s.record(&entry("a", "la", HistoryKind::Sent, "1"), false)
            .expect("a");
        s.record(&entry("b", "lb", HistoryKind::Sent, "1"), false)
            .expect("b");
        s.purge_wallet("a").expect("purge");
        assert!(s.list("a", None, 10).unwrap().is_empty());
        assert_eq!(s.list("b", None, 10).unwrap().len(), 1);
    }

    fn swap(local: &str) -> NewEntry {
        NewEntry {
            received: Some(Received {
                symbol: "USDC".into(),
                amount_base: "1259663040".into(),
                decimals: 6,
                chain_id: "arbitrum".into(),
                chain_name: "Arbitrum One".into(),
            }),
            ..entry("w1", local, HistoryKind::Swap, "500000000000000000")
        }
    }

    #[test]
    fn rows_filed_under_an_old_asset_id_are_refiled_once() {
        let store = HistoryStore::open_in_memory().unwrap();
        let row = |local: &str, chain: &str, asset: &str| NewEntry {
            chain_id: chain.into(),
            asset_id: asset.into(),
            tx_hash: Some(format!("0x{local}")),
            ..entry("w", local, HistoryKind::Received, "1")
        };
        store.record(&row("a", "ethereum", "ethereum:native"), true).unwrap();
        store.record(&row("b", "ethereum", "ethereum:0xa0b8"), true).unwrap();
        // Already right, and another chain whose id merely starts the same.
        store.record(&row("c", "ethereum", "evm:1:native"), true).unwrap();
        store.record(&row("d", "ethereum-classic", "ethereum-classic:native"), true).unwrap();

        assert_eq!(store.rename_asset_prefix("ethereum", "ethereum", "evm:1").unwrap(), 2);
        let id = |local: &str| store.get(local).unwrap().unwrap().asset_id;
        assert_eq!(id("a"), "evm:1:native");
        assert_eq!(id("b"), "evm:1:0xa0b8");
        assert_eq!(id("c"), "evm:1:native");
        assert_eq!(id("d"), "ethereum-classic:native");
        assert_eq!(store.rename_asset_prefix("ethereum", "ethereum", "evm:1").unwrap(), 0, "nothing left to do");
    }

    #[test]
    fn a_swap_keeps_what_was_received() {
        let store = HistoryStore::open_in_memory().unwrap();
        store.record(&swap("s1"), false).unwrap();
        store.record(&entry("w1", "plain", HistoryKind::Sent, "1"), false).unwrap();

        let row = store.get("s1").unwrap().expect("stored");
        assert_eq!(row.kind, HistoryKind::Swap);
        let received = row.received.expect("a swap has a received side");
        assert_eq!(received.symbol, "USDC");
        assert_eq!(received.amount_base, "1259663040");
        assert_eq!(received.decimals, 6);
        assert_eq!(received.chain_name, "Arbitrum One");
        // An ordinary send has none.
        assert_eq!(store.get("plain").unwrap().unwrap().received, None);
        // And it lists under the swap filter.
        let swaps = store.list("w1", Some(KindFilter::Swap), 10).unwrap();
        assert_eq!(swaps.len(), 1);
        assert!(swaps[0].received.is_some());
    }

    #[test]
    fn a_swap_gets_its_hash_while_still_pending() {
        let store = HistoryStore::open_in_memory().unwrap();
        store.record(&swap("s1"), false).unwrap();
        store.attach_hash("s1", "0xabc", Some("https://scan.li.fi/tx/0xabc")).unwrap();

        let row = store.get("s1").unwrap().unwrap();
        assert_eq!(row.status, HistoryStatus::Pending, "sent is not the same as finished");
        assert_eq!(row.tx_hash.as_deref(), Some("0xabc"));
        assert_eq!(row.explorer_url.as_deref(), Some("https://scan.li.fi/tx/0xabc"));

        // The real amount replaces the estimate, then the row settles.
        store.set_received_amount("s1", "1258000000").unwrap();
        store.settle("s1", HistoryStatus::Confirmed, None, None, None).unwrap();
        let row = store.get("s1").unwrap().unwrap();
        assert_eq!(row.status, HistoryStatus::Confirmed);
        assert_eq!(row.tx_hash.as_deref(), Some("0xabc"), "settling keeps the hash");
        assert_eq!(row.received.unwrap().amount_base, "1258000000");

        // A settled row is not rewritten.
        store.set_received_amount("s1", "1").unwrap();
        store.attach_hash("s1", "0xdef", None).unwrap();
        let row = store.get("s1").unwrap().unwrap();
        assert_eq!(row.received.unwrap().amount_base, "1258000000");
        assert_eq!(row.tx_hash.as_deref(), Some("0xabc"));
    }

    /// A database written before swaps had a received side must upgrade in
    /// place, keeping every row.
    #[test]
    fn history_from_before_swaps_upgrades_and_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute(
            "INSERT INTO history_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history (local_id, wallet_id, kind, status, asset_id, symbol, chain_id,
                                  chain_name, to_address, amount_base, decimals, created_at)
             VALUES ('old', 'w1', 'sent', 'confirmed', 'evm:1:native', 'ETH', 'ethereum',
                     'Ethereum', '0xabc', '1000', 18, 5)",
            [],
        )
        .unwrap();

        HistoryStore::migrate(&mut conn).expect("upgrade");
        let store = HistoryStore { conn: Mutex::new(conn) };

        let old = store.get("old").unwrap().expect("the old row survived");
        assert_eq!(old.amount_base, "1000");
        assert_eq!(old.received, None);
        store.record(&swap("new"), false).unwrap();
        assert!(store.get("new").unwrap().unwrap().received.is_some());
        assert_eq!(store.list("w1", None, 10).unwrap().len(), 2);
    }

    #[test]
    fn migration_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        HistoryStore::migrate(&mut conn).expect("first");
        HistoryStore::migrate(&mut conn).expect("second is a no-op");
        let s = HistoryStore { conn: Mutex::new(conn) };
        s.record(&entry("w", "l1", HistoryKind::Sent, "1"), false)
            .expect("usable after re-migration");
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE history_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO history_meta (key, value) VALUES ('schema_version', '9999');",
        )
        .unwrap();
        let err = HistoryStore::migrate(&mut conn).expect_err("must refuse");
        assert!(err.to_string().contains("newer app version"));
    }

    #[test]
    fn unknown_status_and_kind_read_conservatively() {
        assert_eq!(HistoryStatus::from_str_lossy("weird"), HistoryStatus::Pending);
        assert_eq!(HistoryKind::from_str_lossy("weird"), HistoryKind::Sent);
    }

    #[test]
    fn entries_serialize_for_the_frontend() {
        let s = store();
        s.record(&entry("w", "l1", HistoryKind::Sent, "5"), true)
            .expect("record");
        let json = serde_json::to_string(&s.list("w", None, 10).unwrap()).unwrap();
        assert!(json.contains("\"kind\":\"sent\""));
        assert!(json.contains("\"discovered\":true"));
    }
}