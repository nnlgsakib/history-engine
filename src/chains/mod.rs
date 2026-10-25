//! Chain-specific history readers.
//!
//! Each chain family gets its own module because the semantics genuinely
//! differ, not just the URL:
//!
//! - [`evm`] — account model. Balances are readable at any time, so a
//!   transaction's direction is decided by whether our address is the sender.
//!   Token movements and plain coin transfers come from *separate* endpoints.
//! - [`bitcoin`] — UTXO model. There is no "balance change" to read; direction
//!   and amount must be derived from inputs and outputs per transaction, and a
//!   single transaction can both send and receive.
//! - [`solana`] — no explorer at all: the node itself indexes by account, and
//!   what a transaction did is read from balances before and after it.

pub(crate) mod alchemy;
pub mod bitcoin;
pub mod evm;
pub mod solana;

use crate::discovery::Endpoints;
use crate::error::HistoryResult;
use crate::model::NewEntry;
use registry::model::{ChainInfo, HistoryProvider};
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Shared HTTP client. One per discovery run: connections are reused, and
/// public explorer endpoints rate-limit per-connection. `user_agent` names the
/// wallet making the calls, so explorers can tell one wallet's traffic from
/// another's.
pub(crate) fn http_client(user_agent: &str) -> HistoryResult<reqwest::Client> {
    reqwest::Client::builder()
        // Several addresses per name, some unreachable here (IPv6 with no
        // route): each gets a slice of this, then the next is tried.
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(HTTP_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .map_err(|_| crate::error::HistoryError::Source("http client failed".into()))
}

/// Async JSON GET. Explorer responses are untrusted, so every call site parses
/// into an explicit wire type rather than `serde_json::Value`.
///
/// Parse failures keep serde's message: silently returning an empty list here
/// would show a plausible-looking but incomplete activity feed, which is worse
/// than an error.
pub(crate) async fn get_json<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> HistoryResult<T> {
    // An error status is an error: a 500 whose body is `{}` would otherwise
    // parse as an empty page and read as "no history".
    client
        .get(url)
        .send()
        .await
        .map_err(|e| crate::error::HistoryError::Source(format!("request failed: {e}")))?
        .error_for_status()
        .map_err(|e| crate::error::HistoryError::Source(format!("request failed: {e}")))?
        .json::<T>()
        .await
        .map_err(|e| crate::error::HistoryError::Source(format!("bad response: {e}")))
}

/// Async JSON-RPC POST, with the app's session as a bearer token when it has
/// one. An HTTP error status is an error, never an empty result; 401 is
/// `Unauthorized`. The URL is dropped from every error: a development
/// endpoint can carry a key in its path, and errors reach the screen.
pub(crate) async fn post_json<T: for<'de> serde::Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> HistoryResult<T> {
    let fail = |e: reqwest::Error| crate::error::HistoryError::Source(format!("request failed: {}", e.without_url()));
    let mut request = client.post(url).json(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(fail)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(crate::error::HistoryError::Unauthorized);
    }
    response
        .error_for_status()
        .map_err(fail)?
        .json::<T>()
        .await
        .map_err(|e| crate::error::HistoryError::Source(format!("bad response: {}", e.without_url())))
}

/// Explorers are inconsistent about numeric encodings — Blockscout sends
/// `decimals` as the string `"9"` while Esplora sends `value` as a number.
/// Accept both rather than dropping the whole page over one field's type.
pub(crate) fn lenient_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Num {
        Int(u64),
        Str(String),
        Null,
    }
    match <Num as serde::Deserialize>::deserialize(deserializer)? {
        Num::Int(v) => Ok(Some(v)),
        Num::Str(s) => Ok(s.trim().parse::<u64>().ok()),
        Num::Null => Ok(None),
    }
}

/// Same tolerance for `u8` fields such as token decimals.
pub(crate) fn lenient_u8<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64(deserializer)?.and_then(|v| u8::try_from(v).ok()))
}

/// Explorer strings are untrusted: length-capped, control characters stripped.
pub(crate) fn clean(value: &str, max: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Unix seconds from an ISO-8601 timestamp, as explorers and providers send
/// them: `2026-10-08T06:44:59Z`, with optional fractional seconds and an
/// optional `Z` or `±HH:MM` offset. `None` for anything else, so a caller
/// decides what a missing date means instead of getting a made-up one.
pub(crate) fn iso_secs(timestamp: &str) -> Option<i64> {
    let (date, time) = timestamp.trim().split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    let clock = time.get(..8)?;
    let mut hms = clock.splitn(3, ':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // Skip fractional seconds, then read the offset.
    let rest = time[8..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h, m) = rest[1..].split_once(':')?;
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Explorer timestamp, or now when there is none (a transaction still in the
/// mempool has no block yet).
pub(crate) fn epoch(timestamp: &Option<String>) -> i64 {
    timestamp
        .as_deref()
        .and_then(iso_secs)
        .unwrap_or_else(crate::model::now_secs)
}

/// A `0x` hex quantity (up to 256 bits) as an exact decimal string. Amounts
/// are never routed through a float or a u128: token supplies exceed both.
pub(crate) fn hex_to_dec(hex: &str) -> Option<String> {
    let digits = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    if digits.is_empty() || digits.len() > 64 {
        return None;
    }
    // Little-endian base-10 digits, multiplied by 16 per hex digit.
    let mut out: Vec<u8> = vec![0];
    for c in digits.chars() {
        let mut carry = c.to_digit(16)?;
        for d in out.iter_mut() {
            let v = u32::from(*d) * 16 + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            out.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    Some(out.iter().rev().map(|d| char::from(b'0' + d)).collect())
}

/// One place a chain's history can be read from.
pub(crate) enum Source {
    Alchemy { url: String, internal: bool },
    /// The chain's `history_api`: Blockscout, Esplora or the Solana node.
    Keyless,
}

impl Source {
    /// Name this source's cursor is kept under; `None` when it has none.
    pub(crate) fn cursor_key(&self) -> Option<&'static str> {
        match self {
            Source::Alchemy { .. } => Some(alchemy::SOURCE),
            Source::Keyless => None,
        }
    }
}

/// What one read found, and the block a later read should start from.
pub(crate) struct Read {
    pub rows: Vec<NewEntry>,
    pub cursor: Option<u64>,
}

/// A chain's sources in the order they are tried: its providers that have
/// an endpoint, then its keyless source. A new provider is one arm here.
pub(crate) fn sources(chain: &ChainInfo, endpoints: &Endpoints) -> Vec<Source> {
    let mut out: Vec<Source> = chain
        .history_providers()
        .iter()
        .filter_map(|provider| match provider {
            HistoryProvider::Alchemy { network, internal } => endpoints.alchemy.as_deref().map(|template| Source::Alchemy {
                url: alchemy::endpoint(template, network),
                internal: *internal,
            }),
        })
        .collect();
    if chain.history_api().is_some() {
        out.push(Source::Keyless);
    }
    out
}

pub(crate) async fn read_source(
    client: &reqwest::Client,
    source: &Source,
    bearer: Option<&str>,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
    since: Option<u64>,
) -> HistoryResult<Read> {
    match source {
        Source::Alchemy { url, internal } => alchemy::read(client, url, bearer, wallet_id, chain, address, *internal, since).await,
        Source::Keyless => Ok(Read { rows: read_chain(client, wallet_id, chain, address).await?, cursor: None }),
    }
}

/// Reads one chain's history from its keyless source, by chain kind.
pub async fn read_chain(
    client: &reqwest::Client,
    wallet_id: &str,
    chain: &ChainInfo,
    address: &str,
) -> HistoryResult<Vec<NewEntry>> {
    match chain {
        ChainInfo::Evm(_) => evm::read(client, wallet_id, chain, address).await,
        ChainInfo::Bitcoin(_) => bitcoin::read(client, wallet_id, chain, address).await,
        ChainInfo::Solana(_) => solana::read(client, wallet_id, chain, address).await,
    }
}

/// A local HTTP server answering each request from a closure, so readers
/// are tested end to end (paging, cursors, fallbacks) without the network.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct Server {
        pub url: String,
        /// Every request as `(path, body)`, in arrival order.
        pub seen: Arc<Mutex<Vec<(String, String)>>>,
        /// Each request's `Authorization` header, in the same order.
        pub auth: Arc<Mutex<Vec<Option<String>>>>,
    }

    /// `reply(path, body)` returns the status and JSON body to send.
    pub fn serve(reply: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let auth = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let auth_log = auth.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut length = 0;
                let mut authorization = None;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body = String::from_utf8_lossy(&body).to_string();
                let (status, json) = reply(&path, &body);
                log.lock().unwrap().push((path, body));
                auth_log.lock().unwrap().push(authorization);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Server { url, seen, auth }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values from Python's `calendar.timegm`.
    #[test]
    fn iso_timestamps_are_unix_seconds() {
        assert_eq!(iso_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_secs("2026-10-08T06:44:59Z"), Some(1_791_441_899));
        // Blockscout adds microseconds, Alchemy sends `.000Z`.
        assert_eq!(iso_secs("2026-10-08T06:44:59.000000Z"), Some(1_791_441_899));
        assert_eq!(iso_secs("2021-09-03T05:30:00.000Z"), Some(1_630_647_000));
        // Leap day, and an explicit offset.
        assert_eq!(iso_secs("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        assert_eq!(iso_secs("2026-10-08T08:44:59+02:00"), Some(1_791_441_899));
        assert_eq!(iso_secs("2026-10-08T06:44:59"), Some(1_791_441_899));
    }

    #[test]
    fn malformed_timestamps_are_refused() {
        for bad in ["", "2026-10-08", "2026-13-01T00:00:00Z", "2026-10-08T25:00:00Z", "1791441899", "x-y-zT00:00:00Z"] {
            assert_eq!(iso_secs(bad), None, "{bad}");
        }
    }

    #[test]
    fn hex_amounts_become_exact_decimal_strings() {
        assert_eq!(hex_to_dec("0x0").as_deref(), Some("0"));
        assert_eq!(hex_to_dec("0x00ff").as_deref(), Some("255"));
        assert_eq!(hex_to_dec("0x6f05b59d3b20000").as_deref(), Some("500000000000000000"));
        let max = format!("0x{}", "f".repeat(64));
        assert_eq!(
            hex_to_dec(&max).as_deref(),
            Some("115792089237316195423570985008687907853269984665640564039457584007913129639935")
        );
    }

    #[test]
    fn bad_hex_amounts_are_refused() {
        for bad in ["", "0x", "ff", "0xzz", "-0x1"] {
            assert_eq!(hex_to_dec(bad), None, "{bad}");
        }
        assert_eq!(hex_to_dec(&format!("0x1{}", "0".repeat(64))), None, "wider than 256 bits");
    }
}
