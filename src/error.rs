use std::fmt;

/// User-safe history errors. No paths, no response bodies — those can carry
/// internal detail that has no place in a wallet UI.
#[derive(Debug)]
pub enum HistoryError {
    /// Chain has no keyless history source configured.
    Unsupported(String),
    /// Explorer was unreachable or returned junk.
    Source(String),
    Store(String),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Unsupported(id) => {
                write!(f, "{id} has no history source configured")
            }
            HistoryError::Source(msg) => write!(f, "history source unavailable: {msg}"),
            HistoryError::Store(msg) => write!(f, "history store failed: {msg}"),
        }
    }
}

impl std::error::Error for HistoryError {}

pub type HistoryResult<T> = Result<T, HistoryError>;