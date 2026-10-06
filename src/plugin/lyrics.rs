//! `lyrics.get {ref}` (capability `lyrics`): the session's lyrics service,
//! `color-lyrics/v2/track/<id>`, as the Spotify apps read it:
//!
//! ```json
//! {"lyrics": {"syncType": "LINE_SYNCED", "lines": [
//!   {"startTimeMs": "960", "words": "…", "endTimeMs": "0"}]}}
//! ```
//!
//! `LINE_SYNCED` lines become `synced`; `UNSYNCED` ones (every time `0`)
//! become `plain`. A track without lyrics answers 404; it is remembered for
//! a while so that the host's retries stay local.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue, Method};
use librespot_core::Session;
use serde_json::{Value, json};

use super::items::Ref;
use super::rpc::{RpcError, RpcResult};

/// How long a track without lyrics is not asked about again.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
const MAX_MISSING: usize = 2000;

static MISSING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn known_missing(id: &str) -> bool {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    m.retain(|_, at| at.elapsed() < MISSING_TTL);
    m.contains_key(id)
}

fn note_missing(id: &str) {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    if m.len() >= MAX_MISSING {
        m.clear();
    }
    m.insert(id.to_string(), Instant::now());
}

fn not_found(id: &str) -> RpcError {
    RpcError::not_found(format!("no lyrics for track {id}"))
}

/// Time of a line: a number or a numeric string, in ms.
fn millis(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The protocol answer from the lyrics document, `None` when it holds none.
pub fn convert(doc: &Value) -> Option<Value> {
    let lyrics = doc.get("lyrics")?;
    let lines = lyrics.get("lines").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    // `♪` marks instrumental breaks in synced lyrics.
    let text = |l: &Value| {
        let t = l.get("words").and_then(Value::as_str).unwrap_or("").trim();
        if t == "♪" { String::new() } else { t.to_string() }
    };
    let synced_type = lyrics.get("syncType").and_then(Value::as_str).is_some_and(|t| t.contains("SYNCED") && t != "UNSYNCED");
    let timed: Vec<Value> = lines.iter().filter_map(|l| Some(json!({"time_ms": millis(l.get("startTimeMs"))?, "text": text(l)}))).collect();
    let any_time = timed.iter().any(|l| l["time_ms"].as_u64() != Some(0));
    if synced_type && any_time && timed.len() == lines.len() {
        return Some(json!({"synced": timed}));
    }
    let plain = lines.iter().map(text).collect::<Vec<_>>().join("\n");
    let plain = plain.trim();
    (!plain.is_empty()).then(|| json!({"plain": plain}))
}

pub async fn get(session: &Session, reference: &str) -> RpcResult {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} has no lyrics")));
    };
    if known_missing(&id) {
        return Err(not_found(&id));
    }
    let mut headers = HeaderMap::new();
    headers.insert("app-platform", HeaderValue::from_static("WebPlayer"));
    let endpoint = format!("/color-lyrics/v2/track/{id}?format=json&vocalRemoval=false&market=from_token");
    let raw = match session.spclient().request_as_json(&Method::GET, &endpoint, Some(headers), None).await {
        Ok(raw) => raw,
        Err(e) if e.kind == librespot_core::error::ErrorKind::NotFound => {
            note_missing(&id);
            return Err(not_found(&id));
        }
        Err(e) => return Err(anyhow::Error::from(e).context("lyrics").into()),
    };
    let doc: Value = serde_json::from_slice(&raw).map_err(|e| RpcError::not_found(format!("lyrics of {id}: {e}")))?;
    match convert(&doc) {
        Some(lyrics) => Ok(lyrics),
        None => {
            note_missing(&id);
            Err(not_found(&id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_and_plain_documents() {
        let doc = json!({"lyrics": {"syncType": "LINE_SYNCED", "lines": [
            {"startTimeMs": "960", "words": "We're no strangers to love", "endTimeMs": "0"},
            {"startTimeMs": "4800", "words": "♪"},
            {"startTimeMs": "9000", "words": " You know the rules "}
        ]}});
        assert_eq!(convert(&doc), Some(json!({"synced": [
            {"time_ms": 960, "text": "We're no strangers to love"},
            {"time_ms": 4800, "text": ""},
            {"time_ms": 9000, "text": "You know the rules"}
        ]})));
        let unsynced = json!({"lyrics": {"syncType": "UNSYNCED", "lines": [
            {"startTimeMs": "0", "words": "a"}, {"startTimeMs": "0", "words": "b"}]}});
        assert_eq!(convert(&unsynced), Some(json!({"plain": "a\nb"})));
        let zero = json!({"lyrics": {"syncType": "LINE_SYNCED", "lines": [{"startTimeMs": "0", "words": "a"}]}});
        assert_eq!(convert(&zero), Some(json!({"plain": "a"})), "no time at all: plain");
        assert_eq!(convert(&json!({"lyrics": {"syncType": "LINE_SYNCED", "lines": []}})), None);
        assert_eq!(convert(&json!({})), None);
    }
}
