//! `track.resolve`: a track becomes a local relay URL serving its Ogg Vorbis
//! file, decrypted, unchanged.
//!
//! 1. The track's metadata (session): files, availability, alternatives.
//!    A track not playable here is relinked to an alternative that is (the
//!    same recording in another release), as the Spotify apps do.
//! 2. The Ogg Vorbis file at the bitrate the `quality` setting asks for,
//!    else the closest one.
//! 3. Its audio key (retried, see `audiokey.rs`), its CDN address, its
//!    first kilobyte (size, normalisation data).
//! 4. The relay serves it (`relay.rs`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use serde::Deserialize;
use serde_json::{Value, json};

use super::items::Ref;
use super::relay::{self, Cdn, Relay};
use super::rpc::RpcError;
use super::settings::Quality;
use crate::audiokey;

/// Taken off the relay URL's lifetime in `expires_at`.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// What the host's current output accepts natively (`initialize.output`,
/// `output.changed`).
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Output {
    #[serde(default)]
    pub bit_perfect: Option<bool>,
    #[serde(default)]
    pub max_rate: Option<u32>,
    #[serde(default)]
    pub rates: Vec<u32>,
}

impl Output {
    /// Whether 44.1 kHz, the only rate Spotify streams, plays without
    /// conversion. Unknown output: yes.
    pub fn accepts_44k1(&self) -> bool {
        if self.bit_perfect != Some(true) {
            return true;
        }
        if self.rates.is_empty() { self.max_rate.is_none_or(|m| m >= 44_100) } else { self.rates.contains(&44_100) }
    }
}

/// Ogg Vorbis files to try, best first.
pub fn formats(quality: Quality) -> [AudioFileFormat; 3] {
    use AudioFileFormat::*;
    match quality {
        Quality::High => [OGG_VORBIS_320, OGG_VORBIS_160, OGG_VORBIS_96],
        Quality::Normal => [OGG_VORBIS_160, OGG_VORBIS_96, OGG_VORBIS_320],
    }
}

pub fn kbps(format: AudioFileFormat) -> u32 {
    match format {
        AudioFileFormat::OGG_VORBIS_320 => 320,
        AudioFileFormat::OGG_VORBIS_160 => 160,
        _ => 96,
    }
}

/// What play reports need from a resolution.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// The track played: an alternative when the one asked for was relinked.
    pub track_id: String,
    pub duration_ms: u64,
    pub kbps: u32,
}

/// The reason Spotify gives, in words.
fn unavailability(reason: &librespot_metadata::availability::UnavailabilityReason) -> String {
    use librespot_metadata::availability::UnavailabilityReason::*;
    match reason {
        Blacklisted | NotWhitelisted => "not available in your country".into(),
        Embargo => "not released yet".into(),
        other => format!("not available ({other:?})"),
    }
}

/// The playable item for `uri`: itself, or one of its alternatives.
async fn playable_item(session: &Session, uri: SpotifyUri) -> Result<AudioItem, RpcError> {
    let item = AudioItem::get_file(session, uri).await.map_err(|e| RpcError::from(anyhow::Error::from(e).context("track metadata")))?;
    if item.availability.is_ok() && !item.files.is_empty() {
        return Ok(item);
    }
    let reason = match &item.availability {
        Err(r) => unavailability(r),
        Ok(()) => "no audio file".into(),
    };
    for alt in item.alternatives.iter().flat_map(|a| a.iter()) {
        match AudioItem::get_file(session, alt.clone()).await {
            Ok(a) if a.availability.is_ok() && !a.files.is_empty() => {
                tracing::info!("track relinked to an alternative release");
                return Ok(a);
            }
            _ => continue,
        }
    }
    Err(RpcError::unavailable(format!("{}: {reason}", item.name)))
}

pub async fn resolve(session: &Session, relay: &Relay, reference: &str, quality: Quality, output: &Output) -> Result<(Value, Resolved), RpcError> {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} is not a track")));
    };
    if !output.accepts_44k1() {
        return Err(RpcError::unavailable("the output does not play 44.1 kHz, the only rate Spotify streams, natively"));
    }
    let sid = SpotifyId::from_base62(&id).map_err(|_| RpcError::not_found(format!("{reference}: bad id")))?;
    let item = playable_item(session, SpotifyUri::Track { id: sid }).await?;
    let (format, file_id) = formats(quality)
        .into_iter()
        .find_map(|f| item.files.get(&f).map(|file| (f, *file)))
        .ok_or_else(|| RpcError::unavailable(format!("{}: no Ogg Vorbis file", item.name)))?;
    let played_id = match &item.track_id {
        SpotifyUri::Track { id } => *id,
        _ => sid,
    };
    let key = audiokey::fetch(session, played_id, file_id, audiokey::BACKOFF)
        .await
        .map_err(|e| RpcError::unavailable(e.to_string()))?;
    let (url, expires) = relay::resolve_cdn(session, file_id).await?;
    let head = relay::fetch_head(relay.http(), &url, &key.0).await.map_err(|e| RpcError::from(e.context("first bytes of the file")))?;
    let stream = relay::Stream {
        track_id: played_id.to_base62().unwrap_or_else(|_| id.clone()),
        file_id,
        key: key.0,
        size: head.size,
        cdn: std::sync::Mutex::new(Cdn { url, expires }),
        session: Some(session.clone()),
    };
    let local = relay.register(stream).await?;
    tracing::info!("resolved a track: Ogg Vorbis {} kbps, {} bytes", kbps(format), head.size);
    let resolved = Resolved {
        track_id: played_id.to_base62().unwrap_or(id),
        duration_ms: u64::from(item.duration_ms),
        kbps: kbps(format),
    };
    Ok((result_json(&local, &head, &resolved, now_unix_s()), resolved))
}

pub fn now_unix_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The `track.resolve` answer.
pub fn result_json(url: &str, head: &relay::Head, r: &Resolved, now: i64) -> Value {
    let mut out = json!({
        "url": url,
        "expires_at": now + (relay::STREAM_TTL - EXPIRY_MARGIN).as_secs() as i64,
        "duration_ms": r.duration_ms,
        "format": {"codec": "vorbis", "sample_rate": 44_100, "channels": 2},
        "live": false,
        "delivery": "proxied",
    });
    if let Some([track_gain, track_peak, album_gain, album_peak]) = head.normalisation {
        out["replaygain"] = json!({
            "track_gain": round2(track_gain), "track_peak": round4(track_peak),
            "album_gain": round2(album_gain), "album_peak": round4(album_peak),
        });
    }
    out
}

fn round2(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

fn round4(v: f32) -> f64 {
    (f64::from(v) * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrates_follow_the_setting() {
        assert_eq!(formats(Quality::High)[0], AudioFileFormat::OGG_VORBIS_320);
        assert_eq!(formats(Quality::Normal), [AudioFileFormat::OGG_VORBIS_160, AudioFileFormat::OGG_VORBIS_96, AudioFileFormat::OGG_VORBIS_320]);
        assert_eq!(kbps(AudioFileFormat::OGG_VORBIS_96), 96);
    }

    #[test]
    fn outputs_without_44k1_are_refused() {
        assert!(Output::default().accepts_44k1());
        let only_48 = Output { bit_perfect: Some(true), max_rate: Some(48_000), rates: vec![48_000] };
        assert!(!only_48.accepts_44k1());
        assert!(Output { bit_perfect: Some(false), ..only_48.clone() }.accepts_44k1(), "the host converts anyway");
        assert!(Output { bit_perfect: Some(true), max_rate: Some(192_000), rates: vec![] }.accepts_44k1());
    }

    #[test]
    fn answer_is_proxied_vorbis_with_replaygain() {
        let head = relay::Head { size: 1000, normalisation: Some([-6.5, 0.951_234, -7.25, 0.99]) };
        let r = Resolved { track_id: "a".into(), duration_ms: 213_573, kbps: 320 };
        let v = result_json("http://127.0.0.1:1/t/a.ogg", &head, &r, 1_000_000);
        assert_eq!(v["delivery"], "proxied");
        assert_eq!(v["format"], json!({"codec": "vorbis", "sample_rate": 44100, "channels": 2}));
        assert_eq!(v["duration_ms"], 213_573);
        assert_eq!(v["replaygain"]["track_gain"], -6.5);
        assert_eq!(v["replaygain"]["track_peak"], 0.9512);
        assert!(v["expires_at"].as_i64().unwrap() > 1_000_000 + 3600);
        let none = result_json("u", &relay::Head { size: 1, normalisation: None }, &r, 0);
        assert!(none.get("replaygain").is_none());
    }

    /// Against Spotify, with stored credentials: `SPCONNECT_DATA=<data dir
    /// of a signed-in plugin> SPCONNECT_TRACK=<base62 id> SPCONNECT_OUT=<file>
    /// cargo test live_resolve -- --ignored --nocapture`. Resolves the track,
    /// downloads it through the relay and writes the Ogg file.
    #[tokio::test]
    #[ignore]
    async fn live_resolve() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let account = crate::session::Account::new(env("SPCONNECT_DATA").into(), &Default::default()).unwrap();
        let session = account.session().await.unwrap();
        let relay = Relay::default();
        let (v, r) = resolve(&session, &relay, &format!("track/{}", env("SPCONNECT_TRACK")), Quality::High, &Output::default())
            .await
            .map_err(|e| e.message)
            .unwrap();
        let resp = reqwest::get(v["url"].as_str().unwrap()).await.unwrap();
        let length: usize = resp.headers()["content-length"].to_str().unwrap().parse().unwrap();
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.len(), length);
        assert!(body.starts_with(b"OggS"));
        std::fs::write(env("SPCONNECT_OUT"), &body).unwrap();
        println!("{} kbps, {} bytes, replaygain {:?}", r.kbps, body.len(), v.get("replaygain"));
    }
}
