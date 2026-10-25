//! Transaction history, activity feed and chain discovery.
//!
//! Two responsibilities, deliberately separate from the app and from
//! `registry`:
//!
//! 1. **Persist** what this device did, so a send survives an app restart.
//! 2. **Discover** what the chain did to this wallet, because a seed only
//!    yields addresses and keys — never history. Local sends and discovered
//!    rows are merged and deduplicated by transaction hash.
//!
//! No signing, no key access, no fee logic. Those stay in the app's tx module.

pub mod chains;
pub mod discovery;
pub mod error;
pub mod model;
pub mod store;

pub use error::{HistoryError, HistoryResult};
pub use discovery::{Discovery, Endpoints, ScanTarget};
pub use model::{HistoryEntry, HistoryKind, HistoryStatus, KindFilter, NewEntry, Received};
pub use store::HistoryStore;