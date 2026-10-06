//! Local relay: serves each resolved track as a plain Ogg Vorbis file on
//! `http://127.0.0.1:<port>/<token>/<track>.ogg`.
//!
//! Spotify's CDN serves the file encrypted with AES-128-CTR (the track's
//! audio key, a fixed IV, the counter running from the first byte), behind
//! a 0xa7-byte Spotify header. The relay fetches the bytes asked for from
//! the CDN, decrypts them on the fly and drops the header, so the host gets
//! the Ogg file Spotify encoded, unchanged:
//!
//! - `Content-Length` is the CDN file's size minus the header;
//! - CTR mode decrypts from any offset, so `Range: bytes=a-b` is honoured
//!   (`206`, `Content-Range`) with one CDN range request, and
//!   `Accept-Ranges: bytes` is advertised;
//! - tokens are random (122 bits) and forgotten after [`STREAM_TTL`]; an
//!   unknown token answers `410 Gone`, and the host resolves the track
//!   again;
//! - a CDN address that has expired is asked for again through the session.
//!
//! CDN addresses carry signatures: they are never logged.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use librespot_core::{FileId, Session};
use tokio::sync::OnceCell;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// The IV of every Spotify audio file.
pub const AUDIO_IV: [u8; 16] = [0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93];
/// Spotify's header in front of the Ogg stream (decrypted).
pub const HEADER: u64 = 0xa7;
/// How long a relay URL works.
pub const STREAM_TTL: Duration = Duration::from_secs(6 * 3600);
/// Streams registered at once; the oldest go first.
const MAX_STREAMS: usize = 32;

/// Decrypt `buf`, which starts at `offset` in the encrypted file.
pub fn decrypt_at(key: &[u8; 16], offset: u64, buf: &mut [u8]) {
    let mut cipher = Aes128Ctr::new(key.into(), &AUDIO_IV.into());
    cipher.seek(offset);
    cipher.apply_keystream(buf);
}

/// Where the encrypted file is, and how to find it again once the address
/// has expired.
pub struct Cdn {
    pub url: String,
    pub expires: Option<Instant>,
}

/// A registered track.
pub struct Stream {
    /// Base62 track id: the file name in the URL.
    pub track_id: String,
    pub file_id: FileId,
    pub key: [u8; 16],
    /// Size of the encrypted file, header included.
    pub size: u64,
    pub cdn: Mutex<Cdn>,
    /// Renews the CDN address; `None` in tests.
    pub session: Option<Session>,
}

impl Stream {
    /// Bytes the host sees.
    pub fn served_size(&self) -> u64 {
        self.size.saturating_sub(HEADER)
    }
}

struct Entry {
    stream: Stream,
    registered: Instant,
}

#[derive(Default)]
struct Shared {
    streams: Mutex<HashMap<String, Arc<Entry>>>,
    http: reqwest::Client,
}

/// The relay; its HTTP server starts with the first stream.
#[derive(Default, Clone)]
pub struct Relay {
    shared: Arc<Shared>,
    port: Arc<OnceCell<u16>>,
}

impl Relay {
    /// Register `stream`; returns its URL.
    pub async fn register(&self, stream: Stream) -> Result<String> {
        let port = self.port.get_or_try_init(|| self.start()).await?;
        let track_id = stream.track_id.clone();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let entry = Arc::new(Entry { stream, registered: Instant::now() });
        let mut streams = self.shared.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.retain(|_, e| e.registered.elapsed() < STREAM_TTL);
        while streams.len() >= MAX_STREAMS {
            let oldest = streams.iter().min_by_key(|(_, e)| e.registered).map(|(t, _)| t.clone());
            if let Some(t) = oldest {
                streams.remove(&t);
            }
        }
        streams.insert(token.clone(), entry);
        Ok(format!("http://127.0.0.1:{port}/{token}/{track_id}.ogg"))
    }

    async fn start(&self) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.context("relay: bind")?;
        let port = listener.local_addr()?.port();
        let app = Router::new().route("/{token}/{file}", get(serve).head(serve)).with_state(self.shared.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("relay stopped: {e}");
            }
        });
        tracing::info!("relay listening on 127.0.0.1:{port}");
        Ok(port)
    }

    /// The HTTP client the relay fetches with.
    pub fn http(&self) -> &reqwest::Client {
        &self.shared.http
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes:
/// the inclusive range, `Err` when it cannot be satisfied, `None` for the
/// whole file (no header, or one that is not a single byte range).
pub fn byte_range(headers: &HeaderMap, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let range = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) if a <= b => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => (size.saturating_sub(n), size.saturating_sub(1)),
        _ => return None,
    };
    Some(if size == 0 || range.0 >= size || range.0 > range.1 { Err(()) } else { Ok(range) })
}

async fn serve(State(shared): State<Arc<Shared>>, Path((token, _file)): Path<(String, String)>, method: Method, headers: HeaderMap) -> Response {
    let entry = {
        let streams = shared.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.get(&token).cloned()
    };
    let Some(entry) = entry.filter(|e| e.registered.elapsed() < STREAM_TTL) else {
        return (StatusCode::GONE, "stream expired").into_response();
    };
    let size = entry.stream.served_size();
    let (status, start, end) = match byte_range(&headers, size) {
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        match open(&shared, &entry, start + HEADER, end + HEADER).await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!("relay: track {}: {e:#}", entry.stream.track_id);
                return (StatusCode::BAD_GATEWAY, "CDN unavailable").into_response();
            }
        }
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/ogg")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The CDN address, renewed through the session when it has expired or
/// was refused.
async fn cdn_url(entry: &Entry, renew: bool) -> Result<String> {
    {
        let cdn = entry.stream.cdn.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = cdn.expires.is_none_or(|t| t > Instant::now() + Duration::from_secs(30));
        if fresh && !renew {
            return Ok(cdn.url.clone());
        }
    }
    let session = entry.stream.session.as_ref().ok_or_else(|| anyhow!("CDN address expired"))?;
    let (url, expires) = resolve_cdn(session, entry.stream.file_id).await?;
    let mut cdn = entry.stream.cdn.lock().unwrap_or_else(|e| e.into_inner());
    cdn.url = url.clone();
    cdn.expires = expires;
    Ok(url)
}

/// A CDN address of `file`, and when it expires.
pub async fn resolve_cdn(session: &Session, file: FileId) -> Result<(String, Option<Instant>)> {
    let cdn = librespot_core::cdn_url::CdnUrl::new(file).resolve_audio(session).await.context("CDN address")?;
    let url = cdn.try_get_urls()?.first().map(|u| u.to_string()).ok_or_else(|| anyhow!("no CDN address"))?;
    // `__token__=exp=<unix>~…` or `?…&Expires=`: librespot read it already;
    // keep a conservative lifetime of our own.
    let expires = Some(Instant::now() + Duration::from_secs(30 * 60));
    Ok((url, expires))
}

/// A body streaming the decrypted bytes `from..=to` of the encrypted file.
async fn open(shared: &Shared, entry: &Arc<Entry>, from: u64, to: u64) -> Result<Body> {
    let mut renew = false;
    let resp = loop {
        let url = cdn_url(entry, renew).await?;
        let resp = shared
            .http
            .get(&url)
            .header(header::RANGE, format!("bytes={from}-{to}"))
            .send()
            .await
            .context("CDN request")?;
        let status = resp.status().as_u16();
        if matches!(status, 403 | 404 | 410) && !renew && entry.stream.session.is_some() {
            renew = true;
            continue;
        }
        if status != 206 && !(status == 200 && from == 0 && to + 1 == entry.stream.size) {
            bail!("CDN answered HTTP {status} to a range request");
        }
        break resp;
    };
    let mut cipher = Aes128Ctr::new((&entry.stream.key).into(), &AUDIO_IV.into());
    cipher.seek(from);
    let expected = to - from + 1;
    let mut sent = 0u64;
    let track = entry.stream.track_id.clone();
    let chunks = resp.bytes_stream().map(move |chunk| match chunk {
        Ok(bytes) => {
            let take = (expected.saturating_sub(sent)).min(bytes.len() as u64) as usize;
            let mut buf = bytes[..take].to_vec();
            cipher.apply_keystream(&mut buf);
            sent += take as u64;
            Ok::<_, std::io::Error>(Bytes::from(buf))
        }
        Err(e) => {
            tracing::warn!("relay: track {track}: CDN body: {e}");
            Err(std::io::Error::other(e))
        }
    });
    Ok(Body::from_stream(chunks))
}

/// What resolving learns from the first bytes of the file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Head {
    /// Size of the encrypted file.
    pub size: u64,
    /// Spotify's normalisation data: track gain (dB) and peak, album gain
    /// and peak.
    pub normalisation: Option<[f32; 4]>,
}

/// Normalisation data sits at offset 144 of the decrypted file, in the
/// Spotify header: four little-endian `f32`.
const NORMALISATION_AT: usize = 144;

/// Read the head of the decrypted file: checks that the Ogg stream starts
/// after the header, and reads the normalisation data.
pub fn parse_head(decrypted: &[u8], size: u64) -> Result<Head> {
    let h = HEADER as usize;
    anyhow::ensure!(decrypted.len() >= h + 4, "file too short");
    anyhow::ensure!(&decrypted[h..h + 4] == b"OggS", "no Ogg stream after the header (wrong key?)");
    let f = |i: usize| f32::from_le_bytes(decrypted[NORMALISATION_AT + 4 * i..NORMALISATION_AT + 4 * i + 4].try_into().expect("4 bytes"));
    let values = [f(0), f(1), f(2), f(3)];
    let sane = values.iter().all(|v| v.is_finite()) && values[1] > 0.0 && values[1] <= 2.0 && values[0].abs() < 60.0;
    Ok(Head { size, normalisation: sane.then_some(values) })
}

/// Fetch and decrypt the first bytes of the file: its size (from
/// `Content-Range`) and head.
pub async fn fetch_head(http: &reqwest::Client, url: &str, key: &[u8; 16]) -> Result<Head> {
    let resp = http.get(url).header(header::RANGE, "bytes=0-1023").send().await.context("CDN request")?;
    let status = resp.status().as_u16();
    if !(status == 206 || status == 200) {
        return Err(anyhow::Error::from(crate::webapi::HttpError { status, retry_after: None, body: String::new() }).context("CDN"));
    }
    let size = resp
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(if status == 200 { resp.content_length() } else { None })
        .ok_or_else(|| anyhow!("CDN gave no file size"))?;
    let mut head = resp.bytes().await.context("CDN body")?.to_vec();
    head.truncate(1024);
    decrypt_at(key, 0, &mut head);
    parse_head(&head, size)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const KEY: [u8; 16] = *b"0123456789abcdef";

    /// A fake Spotify file: header with normalisation data, then Ogg pages.
    pub fn plain_file(len: usize) -> Vec<u8> {
        let mut f = vec![0u8; len];
        for (i, b) in f.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        for (i, v) in [-6.5f32, 0.95, -7.25, 0.99].iter().enumerate() {
            f[NORMALISATION_AT + 4 * i..NORMALISATION_AT + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        f[HEADER as usize..HEADER as usize + 4].copy_from_slice(b"OggS");
        f
    }

    pub fn encrypted(plain: &[u8]) -> Vec<u8> {
        let mut e = plain.to_vec();
        decrypt_at(&KEY, 0, &mut e);
        e
    }

    fn range(h: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, h.parse().unwrap());
        byte_range(&headers, size)
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range(&HeaderMap::new(), 100), None);
        assert_eq!(range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=90-500", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=100-", 100), Some(Err(())));
        assert_eq!(range("bytes=5-2", 100), None);
        assert_eq!(range("bytes=0-1,5-6", 100), None, "several ranges: the whole file");
        assert_eq!(range("items=0-1", 100), None);
    }

    #[test]
    fn ctr_decrypts_from_any_offset() {
        let plain = plain_file(5000);
        let enc = encrypted(&plain);
        assert_ne!(enc, plain);
        for (a, b) in [(0usize, 5000usize), (1, 17), (15, 16), (16, 33), (HEADER as usize, 999), (4093, 5000)] {
            let mut part = enc[a..b].to_vec();
            decrypt_at(&KEY, a as u64, &mut part);
            assert_eq!(part, plain[a..b], "{a}..{b}");
        }
    }

    #[test]
    fn head_gives_size_and_normalisation() {
        let plain = plain_file(2000);
        let head = parse_head(&plain[..1024], 2000).unwrap();
        assert_eq!(head.size, 2000);
        assert_eq!(head.normalisation, Some([-6.5, 0.95, -7.25, 0.99]));
        let mut bad = plain.clone();
        bad[HEADER as usize] = b'X';
        assert!(parse_head(&bad, 2000).is_err(), "a wrong key gives no Ogg stream");
        let mut nan = plain;
        nan[NORMALISATION_AT..NORMALISATION_AT + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(parse_head(&nan, 2000).unwrap().normalisation, None);
    }

    /// A CDN serving `file` with range support.
    pub async fn fake_cdn(file: Vec<u8>) -> String {
        use axum::extract::State as S;
        async fn cdn(S(file): S<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
            let size = file.len() as u64;
            match byte_range(&headers, size) {
                Some(Ok((a, b))) => (
                    StatusCode::PARTIAL_CONTENT,
                    [(header::CONTENT_RANGE, format!("bytes {a}-{b}/{size}"))],
                    file[a as usize..=b as usize].to_vec(),
                )
                    .into_response(),
                _ => file.to_vec().into_response(),
            }
        }
        let app = Router::new().route("/audio/{f}", get(cdn)).with_state(Arc::new(file));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("{base}/audio/f00d?__token__=exp=9999999999~hmac=x")
    }

    #[tokio::test]
    async fn relay_serves_the_ogg_file_with_ranges() {
        let plain = plain_file(70_000);
        let url = fake_cdn(encrypted(&plain)).await;
        let relay = Relay::default();
        let head = fetch_head(relay.http(), &url, &KEY).await.unwrap();
        assert_eq!(head.size, 70_000);
        assert!(head.normalisation.is_some());
        let stream = Stream {
            track_id: "abc".into(),
            file_id: FileId::from_raw(&[1; 20]),
            key: KEY,
            size: head.size,
            cdn: Mutex::new(Cdn { url, expires: None }),
            session: None,
        };
        let local = relay.register(stream).await.unwrap();
        assert!(local.starts_with("http://127.0.0.1:") && local.ends_with("/abc.ogg"));
        let ogg = &plain[HEADER as usize..];

        let http = reqwest::Client::new();
        let whole = http.get(&local).send().await.unwrap();
        assert_eq!(whole.status(), 200);
        assert_eq!(whole.headers()["accept-ranges"], "bytes");
        assert_eq!(whole.headers()["content-length"], ogg.len().to_string().as_str());
        let body = whole.bytes().await.unwrap();
        assert!(body.starts_with(b"OggS"));
        assert_eq!(&body[..], ogg);

        for (a, b) in [(0u64, 0u64), (1, 100), (12_345, 54_321), (ogg.len() as u64 - 10, ogg.len() as u64 - 1)] {
            let r = http.get(&local).header("Range", format!("bytes={a}-{b}")).send().await.unwrap();
            assert_eq!(r.status(), 206);
            assert_eq!(r.headers()["content-range"], format!("bytes {a}-{b}/{}", ogg.len()).as_str());
            assert_eq!(&r.bytes().await.unwrap()[..], &ogg[a as usize..=b as usize], "{a}-{b}");
        }
        let tail = http.get(&local).header("Range", "bytes=-5").send().await.unwrap().bytes().await.unwrap();
        assert_eq!(&tail[..], &ogg[ogg.len() - 5..]);
        let past = http.get(&local).header("Range", format!("bytes={}-", ogg.len())).send().await.unwrap();
        assert_eq!(past.status(), 416);
        let head = http.head(&local).send().await.unwrap();
        assert_eq!((head.status().as_u16(), head.headers()["content-length"].to_str().unwrap()), (200, ogg.len().to_string().as_str()));

        let gone = local.replace(local.rsplit('/').nth(1).unwrap(), "0000");
        assert_eq!(http.get(&gone).send().await.unwrap().status(), 410);
    }
}
