//! Play reports (`reporting` capability, setting `report_playback`).
//!
//! The Spotify apps report each play to the event service
//! (`gabo-receiver-service/v3/events`, a `PublishEventsRequest` of event
//! envelopes) as a `RawCoreStream` event: what was played, from when, for
//! how long, why it started and why it ended. The plugin sends the same
//! event when the host says a play ended:
//!
//! | Host | `RawCoreStream` |
//! |---|---|
//! | `playback.started {ref}` | `playback_start_time`; `reason_start` = `trackdone` after a play that ended, `clickrow` otherwise |
//! | `playback.ended {ref, listened_ms, reason}` | `ms_played`; `reason_end` = `trackdone` (ended), `fwdbtn` (skipped), `endplay` (stopped) |
//!
//! Spotify does not document these events; their fields follow the
//! message definitions of its desktop client. The service answers which
//! events it refused, and why: refusals are logged, transient ones kept
//! for later. (Separately, while Spotify Connect is on, the plugin shows
//! the player as the device playing; see `connect/`.)
//!
//! Unsent plays are kept in `data_dir/play-reports.json`, so that a restart
//! or a network failure does not lose them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Result, bail};
use http::{HeaderMap, HeaderValue, Method};
use librespot_core::Session;
use serde::{Deserialize, Serialize};

use super::resolve::{Resolved, now_unix_s};

/// Plays per request.
const BATCH: usize = 20;
/// Oldest plays are dropped beyond this many.
const MAX_QUEUED: usize = 500;
/// Resolutions remembered for a later start (preloads, queues).
const MAX_RESOLVED: usize = 64;
/// Plays shorter than this are not reported, as the apps do not.
const MIN_PLAYED_MS: u64 = 1_000;

/// A play to report.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Play {
    pub track_id: String,
    /// Unix ms.
    pub started_ms: i64,
    pub ms_played: u64,
    pub reason_start: String,
    pub reason_end: String,
    #[serde(default)]
    pub kbps: u32,
    /// Random, one per play.
    pub playback_id: String,
}

struct Playing {
    track_id: String,
    started_ms: i64,
    kbps: u32,
    duration_ms: u64,
    reason_start: &'static str,
}

#[derive(Default)]
struct Inner {
    resolved: HashMap<String, (Resolved, i64)>,
    playing: HashMap<String, Playing>,
    queue: Vec<Play>,
    loaded: bool,
    /// The last play ended by itself: the next one starts by `trackdone`.
    last_ended: bool,
}

#[derive(Default)]
pub struct Reporter {
    inner: Mutex<Inner>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    plays: Vec<Play>,
}

fn queue_path(data_dir: &Path) -> PathBuf {
    data_dir.join("play-reports.json")
}

fn track_id(reference: &str) -> Option<&str> {
    reference.strip_prefix("track/")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Reporter {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember what `track.resolve` handed out for `reference`.
    pub fn resolved(&self, reference: &str, r: Resolved) {
        let mut inner = self.inner();
        inner.resolved.insert(reference.to_string(), (r, now_unix_s()));
        if inner.resolved.len() > MAX_RESOLVED {
            let oldest = inner.resolved.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                inner.resolved.remove(&k);
            }
        }
    }

    /// Forget everything not yet sent (reporting turned off, signed out).
    pub fn clear(&self, data_dir: Option<&Path>) {
        let mut inner = self.inner();
        inner.playing.clear();
        inner.queue.clear();
        inner.loaded = true;
        if let Some(dir) = data_dir {
            let _ = std::fs::remove_file(queue_path(dir));
        }
    }

    /// `playback.started {ref}`.
    pub fn started(&self, reference: &str) {
        let Some(id) = track_id(reference) else { return };
        let mut inner = self.inner();
        let resolved = inner.resolved.get(reference).map(|(r, _)| r.clone());
        let reason_start = if std::mem::take(&mut inner.last_ended) { "trackdone" } else { "clickrow" };
        inner.playing.insert(
            reference.to_string(),
            Playing {
                // A relinked track is reported as the one played.
                track_id: resolved.as_ref().map_or_else(|| id.to_string(), |r| r.track_id.clone()),
                started_ms: now_ms(),
                kbps: resolved.as_ref().map_or(0, |r| r.kbps),
                duration_ms: resolved.as_ref().map_or(0, |r| r.duration_ms),
                reason_start,
            },
        );
    }

    /// `playback.ended {ref, listened_ms, reason}`: queue the play. Returns
    /// whether one was queued.
    pub fn ended(&self, reference: &str, listened_ms: u64, reason: &str, data_dir: Option<&Path>) -> bool {
        let mut inner = self.inner();
        inner.last_ended = reason == "ended";
        let Some(play) = inner.playing.remove(reference) else { return false };
        let ms_played = if play.duration_ms > 0 { listened_ms.min(play.duration_ms) } else { listened_ms };
        if ms_played < MIN_PLAYED_MS {
            return false;
        }
        let reason_end = match reason {
            "ended" => "trackdone",
            "skipped" => "fwdbtn",
            _ => "endplay",
        };
        Self::load(&mut inner, data_dir);
        inner.queue.push(Play {
            track_id: play.track_id,
            started_ms: play.started_ms,
            ms_played,
            reason_start: play.reason_start.into(),
            reason_end: reason_end.into(),
            kbps: play.kbps,
            playback_id: uuid::Uuid::new_v4().simple().to_string(),
        });
        let excess = inner.queue.len().saturating_sub(MAX_QUEUED);
        inner.queue.drain(..excess);
        Self::save(&inner, data_dir);
        true
    }

    fn load(inner: &mut Inner, data_dir: Option<&Path>) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Some(dir) = data_dir else { return };
        if let Some(saved) = std::fs::read_to_string(queue_path(dir)).ok().and_then(|raw| serde_json::from_str::<Saved>(&raw).ok()) {
            let mut plays = saved.plays;
            plays.append(&mut inner.queue);
            inner.queue = plays;
        }
    }

    fn save(inner: &Inner, data_dir: Option<&Path>) {
        let Some(dir) = data_dir else { return };
        let path = queue_path(dir);
        let result = if inner.queue.is_empty() {
            std::fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        } else {
            let raw = serde_json::to_string(&Saved { plays: inner.queue.clone() }).expect("serialises");
            std::fs::write(&path, raw)
        };
        if let Err(e) = result {
            tracing::warn!("play reports: cannot update {}: {e}", path.display());
        }
    }

    /// Send the queued plays, a batch at a time. Stops at the first
    /// failure; what is left is sent next time.
    pub async fn flush(&self, session: &Session, data_dir: Option<&Path>) {
        loop {
            let batch: Vec<Play> = {
                let mut inner = self.inner();
                Self::load(&mut inner, data_dir);
                inner.queue.iter().take(BATCH).cloned().collect()
            };
            if batch.is_empty() {
                return;
            }
            let keep = match send(session, &batch).await {
                Ok(keep) => keep,
                Err(e) => {
                    tracing::info!("play reports: {} kept for later ({e:#})", batch.len());
                    return;
                }
            };
            let mut inner = self.inner();
            // Only drop what was sent: `clear` may have run meanwhile.
            if inner.queue.starts_with(&batch) {
                inner.queue.drain(..batch.len());
                let again: Vec<Play> = keep.into_iter().map(|i| batch[i].clone()).collect();
                let stop = !again.is_empty();
                inner.queue.splice(0..0, again);
                Self::save(&inner, data_dir);
                if stop {
                    return;
                }
            } else {
                return;
            }
        }
    }
}

/// Sequence number of the events this process sends.
static SEQUENCE: AtomicI64 = AtomicI64::new(1);

/// Publish `plays`; returns the indices to send again (transient refusals).
async fn send(session: &Session, plays: &[Play]) -> Result<Vec<usize>> {
    let sequence_id = uuid::Uuid::new_v4();
    let events: Vec<Vec<u8>> = plays
        .iter()
        .map(|p| envelope(p, session.client_id().as_str(), session.device_id(), sequence_id.as_bytes(), SEQUENCE.fetch_add(1, Ordering::Relaxed)))
        .collect();
    let mut body = Vec::new();
    for e in &events {
        pb::bytes(&mut body, 1, e);
    }
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
    headers.insert(http::header::ACCEPT, HeaderValue::from_static("application/x-protobuf"));
    let answer = session.spclient().request(&Method::POST, "/gabo-receiver-service/v3/events", Some(headers), Some(&body)).await?;
    let errors = pb::event_errors(&answer)?;
    let mut again = Vec::new();
    for (index, transient, reason) in errors {
        if transient {
            again.push(index);
        } else {
            tracing::info!("play report refused by Spotify (reason {reason})");
        }
    }
    if again.len() == plays.len() && !plays.is_empty() {
        bail!("every report refused for now");
    }
    tracing::debug!("play reports sent: {}", plays.len() - again.len());
    Ok(again)
}

/// An `EventEnvelope` holding the play as a `RawCoreStream` message.
fn envelope(p: &Play, client_id: &str, device_id: &str, sequence_id: &[u8], sequence_number: i64) -> Vec<u8> {
    let mut msg = Vec::new();
    let playback_id = hex(&p.playback_id);
    pb::bytes(&mut msg, 1, &playback_id);
    pb::string(&mut msg, 5, "audio");
    pb::string(&mut msg, 6, "spconnect");
    pb::string(&mut msg, 7, env!("CARGO_PKG_VERSION"));
    pb::string(&mut msg, 9, "unknown");
    pb::string(&mut msg, 10, &p.reason_start);
    pb::string(&mut msg, 11, "unknown");
    pb::string(&mut msg, 12, &p.reason_end);
    pb::varint_field(&mut msg, 13, p.started_ms as u64);
    pb::varint_field(&mut msg, 14, p.ms_played);
    pb::varint_field(&mut msg, 15, p.ms_played);
    pb::varint_field(&mut msg, 21, 0);
    if p.kbps > 0 {
        pb::string(&mut msg, 22, &format!("OGG_VORBIS_{}", p.kbps));
    }
    let uri = format!("spotify:track:{}", p.track_id);
    pb::string(&mut msg, 24, &uri);
    pb::string(&mut msg, 25, &uri);
    pb::varint_field(&mut msg, 26, 0);
    pb::varint_field(&mut msg, 27, 0);
    pb::string(&mut msg, 28, "context");
    pb::varint_field(&mut msg, 39, 1);

    let fragment = |name: &str, data: &[u8]| {
        let mut f = Vec::new();
        pb::string(&mut f, 1, name);
        pb::bytes(&mut f, 2, data);
        f
    };
    let wrap_bytes = |v: &[u8]| {
        let mut m = Vec::new();
        pb::bytes(&mut m, 1, v);
        m
    };
    let mut time = Vec::new();
    pb::varint_field(&mut time, 1, now_ms() as u64);
    let mut sdk = Vec::new();
    pb::string(&mut sdk, 1, env!("CARGO_PKG_VERSION"));
    pb::string(&mut sdk, 2, "rust");

    let mut env = Vec::new();
    pb::string(&mut env, 2, "RawCoreStream");
    for f in [
        fragment("message", &msg),
        fragment("context_client_id", &wrap_bytes(&hex(client_id))),
        fragment("context_installation_id", &wrap_bytes(&hex(device_id))),
        fragment("context_time", &time),
        fragment("context_sdk", &sdk),
    ] {
        pb::bytes(&mut env, 3, &f);
    }
    pb::bytes(&mut env, 4, sequence_id);
    pb::varint_field(&mut env, 5, sequence_number as u64);
    env
}

/// Bytes of a hex string; the text itself when it is not hex.
fn hex(s: &str) -> Vec<u8> {
    let valid = s.len().is_multiple_of(2) && s.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return s.as_bytes().to_vec();
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

/// The little of protobuf's wire format these events need.
mod pb {
    use anyhow::{Result, bail};

    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
        varint(out, u64::from(field) << 3);
        varint(out, v);
    }

    pub fn bytes(out: &mut Vec<u8>, field: u32, v: &[u8]) {
        varint(out, (u64::from(field) << 3) | 2);
        varint(out, v.len() as u64);
        out.extend_from_slice(v);
    }

    pub fn string(out: &mut Vec<u8>, field: u32, v: &str) {
        bytes(out, field, v.as_bytes());
    }

    fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&b) = buf.get(*pos) else { bail!("truncated varint") };
            *pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        bail!("varint too long")
    }

    /// `(field, wire type, varint value or bytes)` of each field.
    pub fn fields(buf: &[u8]) -> Result<Vec<(u32, u64, &[u8])>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < buf.len() {
            let key = read_varint(buf, &mut pos)?;
            let (field, wire) = ((key >> 3) as u32, key & 7);
            match wire {
                0 => out.push((field, read_varint(buf, &mut pos)?, &buf[0..0])),
                2 => {
                    let len = read_varint(buf, &mut pos)? as usize;
                    let Some(data) = buf.get(pos..pos + len) else { bail!("truncated field") };
                    pos += len;
                    out.push((field, 0, data));
                }
                1 => pos += 8,
                5 => pos += 4,
                _ => bail!("wire type {wire}"),
            }
        }
        Ok(out)
    }

    /// `PublishEventsResponse.error`: `(index, transient, reason)`.
    pub fn event_errors(buf: &[u8]) -> Result<Vec<(usize, bool, i64)>> {
        let mut errors = Vec::new();
        for (field, _, data) in fields(buf)? {
            if field != 1 {
                continue;
            }
            let (mut index, mut transient, mut reason) = (0usize, false, 0i64);
            for (f, v, _) in fields(data)? {
                match f {
                    1 => index = v as usize,
                    2 => transient = v != 0,
                    3 => reason = v as i64,
                    _ => {}
                }
            }
            errors.push((index, transient, reason));
        }
        Ok(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(id: &str) -> Resolved {
        Resolved { track_id: id.into(), duration_ms: 185_000, kbps: 320 }
    }

    #[test]
    fn plays_are_queued_with_reasons() {
        let dir = std::env::temp_dir().join(format!("spconnect-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Reporter::default();
        r.resolved("track/a", resolved("a2"));
        r.started("track/a");
        assert!(r.ended("track/a", 999_999, "ended", Some(&dir)), "queued");
        r.started("track/b");
        assert!(r.ended("track/b", 30_000, "skipped", Some(&dir)));
        let saved: Saved = serde_json::from_str(&std::fs::read_to_string(dir.join("play-reports.json")).unwrap()).unwrap();
        let (a, b) = (&saved.plays[0], &saved.plays[1]);
        assert_eq!((a.track_id.as_str(), a.ms_played, a.kbps), ("a2", 185_000, 320), "relinked id, capped at the length");
        assert_eq!((a.reason_start.as_str(), a.reason_end.as_str()), ("clickrow", "trackdone"));
        assert_eq!((b.reason_start.as_str(), b.reason_end.as_str()), ("trackdone", "fwdbtn"));
        assert_ne!(a.playback_id, b.playback_id);

        // Too short, or never started: nothing.
        r.started("track/c");
        assert!(!r.ended("track/c", 400, "stopped", Some(&dir)));
        assert!(!r.ended("track/zz", 40_000, "stopped", Some(&dir)));
        assert!(!r.ended("album/x", 40_000, "stopped", Some(&dir)));

        // A new reporter picks the saved queue up.
        let again = Reporter::default();
        again.started("track/d");
        assert!(again.ended("track/d", 10_000, "stopped", Some(&dir)));
        assert_eq!(again.inner().queue.len(), 3);
        assert_eq!(again.inner().queue[2].reason_end, "endplay");
        again.clear(Some(&dir));
        assert!(!dir.join("play-reports.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_are_well_formed_protobuf() {
        let p = Play {
            track_id: "4uLU6hMCjMI75M1A2tKUQC".into(),
            started_ms: 1_790_000_000_000,
            ms_played: 61_000,
            reason_start: "clickrow".into(),
            reason_end: "fwdbtn".into(),
            kbps: 320,
            playback_id: "00112233445566778899aabbccddeeff".into(),
        };
        let env = envelope(&p, "65b708073fc0480ea92a077233ca87bd", "abcd", &[9; 16], 7);
        let fields = pb::fields(&env).unwrap();
        assert_eq!(fields[0], (2, 0, &b"RawCoreStream"[..]));
        let fragments: Vec<&[u8]> = fields.iter().filter(|f| f.0 == 3).map(|f| f.2).collect();
        assert_eq!(fragments.len(), 5);
        let message = pb::fields(fragments[0]).unwrap();
        assert_eq!(message[0].2, b"message");
        let raw = pb::fields(message[1].2).unwrap();
        let get = |f: u32| raw.iter().find(|x| x.0 == f).unwrap();
        assert_eq!(get(1).2.len(), 16, "playback id as bytes");
        assert_eq!(get(14).1, 61_000);
        assert_eq!(get(12).2, b"fwdbtn");
        assert_eq!(get(24).2, b"spotify:track:4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(get(22).2, b"OGG_VORBIS_320");
        assert_eq!(fields.iter().find(|f| f.0 == 5).unwrap().1, 7);

        // An answer with one transient and one final refusal.
        let mut answer = Vec::new();
        for (i, t, r) in [(0u64, 1u64, 3u64), (1, 0, 5)] {
            let mut e = Vec::new();
            pb::varint_field(&mut e, 1, i);
            pb::varint_field(&mut e, 2, t);
            pb::varint_field(&mut e, 3, r);
            pb::bytes(&mut answer, 1, &e);
        }
        assert_eq!(pb::event_errors(&answer).unwrap(), [(0, true, 3), (1, false, 5)]);
        assert_eq!(pb::event_errors(&[]).unwrap(), []);
        assert!(pb::event_errors(&[0x0a, 0x05]).is_err());
        assert_eq!(hex("0aff"), [0x0a, 0xff]);
        assert_eq!(hex("xyz"), b"xyz");
    }
}
