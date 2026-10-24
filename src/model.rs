use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    Sent,
    Received,
    Swap,
}

impl HistoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            HistoryKind::Sent => "sent",
            HistoryKind::Received => "received",
            HistoryKind::Swap => "swap",
        }
    }

    /// Unknown values read as `Sent` — the conservative default, since
    /// guessing "received" would credit the user with money.
    pub(crate) fn from_str_lossy(value: &str) -> HistoryKind {
        match value {
            "received" => HistoryKind::Received,
            "swap" => HistoryKind::Swap,
            _ => HistoryKind::Sent,
        }
    }
}

impl Serialize for HistoryKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryStatus {
    Pending,
    Confirmed,
    Failed,
}

impl HistoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            HistoryStatus::Pending => "pending",
            HistoryStatus::Confirmed => "confirmed",
            HistoryStatus::Failed => "failed",
        }
    }

    /// Unknown values read as `Pending`. An unrecognised status must never
    /// render as a confirmed payment.
    pub(crate) fn from_str_lossy(value: &str) -> HistoryStatus {
        match value {
            "confirmed" => HistoryStatus::Confirmed,
            "failed" => HistoryStatus::Failed,
            _ => HistoryStatus::Pending,
        }
    }
}

impl Serialize for HistoryStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Kind filter for queries. `All` is expressed as `None` by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    Sent,
    Received,
    Swap,
}

impl KindFilter {
    pub fn as_str(&self) -> &'static str {
        match self {
            KindFilter::Sent => "sent",
            KindFilter::Received => "received",
            KindFilter::Swap => "swap",
        }
    }

    pub fn parse(value: &str) -> Option<KindFilter> {
        match value {
            "sent" => Some(KindFilter::Sent),
            "received" => Some(KindFilter::Received),
            "swap" => Some(KindFilter::Swap),
            _ => None,
        }
    }
}

/// The other side of a swap: what came back, and where.
///
/// Present only on swap rows. The row's own asset and amount are what was
/// paid; this is what was received for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Received {
    pub symbol: String,
    /// Base units. The estimate until the swap settles, and the real figure
    /// afterwards where the network reports one.
    pub amount_base: String,
    pub decimals: u8,
    /// Differs from the row's `chain_id` when the swap crossed networks.
    pub chain_id: String,
    pub chain_name: String,
}

/// A row to insert. `tx_hash` identifies the on-chain transaction; local
/// sends that never broadcast leave it empty.
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub wallet_id: String,
    /// Stable local id. Unique, so a retried confirm cannot double-write.
    pub local_id: String,
    pub kind: HistoryKind,
    pub status: HistoryStatus,
    pub asset_id: String,
    pub symbol: String,
    pub chain_id: String,
    pub chain_name: String,
    pub to_address: String,
    /// Base units as a string. Never a float: a float round-trip misstates a
    /// balance by a wei.
    pub amount_base: String,
    pub fee_base: String,
    pub decimals: u8,
    pub price_usd: f64,
    pub tx_hash: Option<String>,
    pub explorer_url: Option<String>,
    pub counterparty: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub confirmed_at: Option<i64>,
    /// Set for swaps only.
    pub received: Option<Received>,
}

/// A stored history row.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub local_id: String,
    pub kind: HistoryKind,
    pub status: HistoryStatus,
    pub asset_id: String,
    pub symbol: String,
    pub chain_id: String,
    pub chain_name: String,
    pub to_address: String,
    pub amount_base: String,
    pub fee_base: String,
    pub decimals: u8,
    pub price_usd: f64,
    pub tx_hash: Option<String>,
    pub explorer_url: Option<String>,
    pub counterparty: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub confirmed_at: Option<i64>,
    /// True when this row came from a chain explorer rather than this device.
    pub discovered: bool,
    /// Set for swaps only.
    pub received: Option<Received>,
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}