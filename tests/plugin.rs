//! Fake plugin host: runs `spconnect plugin` against a mock Web API and a
//! mock accounts service, and checks the JSON-RPC exchanges.
//!
//! The plugin runs with `--no-session`: no access point, so nothing that
//! needs a live Spotify session (audio, Connect, lyrics) is reached here.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use base64::Engine;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const CODE: &str = "c0de";
const USER: &str = "user1secret";
const FIRST_TOKEN: &str = "access-one";
const REFRESHED_TOKEN: &str = "access-two";
const REFRESH: &str = "refresh-r1";

const T1: &str = "4uLU6hMCjMI75M1A2tKUQC";
const T2: &str = "7GhIk7Il098yCjg4BQjzvb";
const ALBUM: &str = "6N9PS4QXF1D0OWPk0Sxtb4";
const ARTIST: &str = "0gxyHStUsqpMadRV0Di1Qt";
const MINE: &str = "3cEYpjA9oz9GiPac4AsH4n";
const EDITORIAL: &str = "37i9dQZF1DXcBWIGoYBM5M";

#[derive(Default)]
struct Mock {
    /// `(method, path, query, body)` of every call.
    calls: Vec<(String, String, HashMap<String, String>, String)>,
    /// Access tokens the API refuses.
    revoked: HashSet<String>,
    /// `code_challenge` of the last authorisation URL, set by the test.
    challenge: Option<String>,
    /// `product` of `/me`; `premium` when unset.
    product: Option<String>,
}

type Shared = Arc<Mutex<Mock>>;

fn track_json(id: &str, name: &str) -> Value {
    json!({
        "id": id, "name": name, "type": "track", "duration_ms": 213_573, "track_number": 1, "disc_number": 1,
        "artists": [{"id": ARTIST, "name": "Rick Astley"}],
        "album": {"id": ALBUM, "name": "Whenever You Need Somebody", "release_date": "1987-11-12",
                  "images": [{"url": "https://i.scdn.co/image/cover", "width": 640}],
                  "artists": [{"id": ARTIST, "name": "Rick Astley"}]}
    })
}

fn album_json() -> Value {
    json!({
        "id": ALBUM, "name": "Whenever You Need Somebody", "release_date": "1987-11-12", "total_tracks": 2,
        "artists": [{"id": ARTIST, "name": "Rick Astley"}], "label": "RCA",
        "images": [{"url": "https://i.scdn.co/image/cover", "width": 640}],
        "copyrights": [{"text": "1987 RCA", "type": "P"}],
        "tracks": {"offset": 0, "total": 2, "next": null, "items": [
            {"id": T1, "name": "Never Gonna Give You Up", "track_number": 1, "duration_ms": 213_573, "artists": [{"id": ARTIST, "name": "Rick Astley"}]},
            {"id": T2, "name": "Whenever You Need Somebody", "track_number": 2, "duration_ms": 234_000, "artists": [{"id": ARTIST, "name": "Rick Astley"}]}
        ]}
    })
}

fn playlist_json(id: &str, owner: &str) -> Value {
    json!({"id": id, "name": if owner == USER { "Soir" } else { "Today's Top Hits" },
           "owner": {"id": owner, "display_name": if owner == USER { "Alice" } else { "Spotify" }},
           "images": [{"url": "https://i.scdn.co/image/pl", "width": 300}], "snapshot_id": "snap1",
           "tracks": {"total": 2}})
}

fn paging(items: Vec<Value>, q: &HashMap<String, String>) -> Value {
    let offset: usize = q.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
    let limit: usize = q.get("limit").and_then(|o| o.parse().ok()).unwrap_or(20);
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
    let next = if offset + page.len() < total { json!("https://api/next") } else { Value::Null };
    json!({"items": page, "total": total, "offset": offset, "limit": limit, "next": next})
}

async fn mock(
    State(mock): State<Shared>,
    method: Method,
    UrlPath(path): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    mock.lock().unwrap().calls.push((method.to_string(), path.clone(), q.clone(), body.clone()));
    if path == "api/token" {
        let form: HashMap<String, String> = url_form(&body);
        return match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let challenge = mock.lock().unwrap().challenge.clone();
                let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
                if form.get("code").map(String::as_str) != Some(CODE) || challenge.as_deref() != Some(computed.as_str()) {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "Invalid authorization code"})))
                        .into_response();
                }
                Json(json!({"access_token": FIRST_TOKEN, "token_type": "Bearer", "expires_in": 3600, "refresh_token": REFRESH, "scope": "streaming"}))
                    .into_response()
            }
            Some("refresh_token") if form.get("refresh_token").map(String::as_str) == Some(REFRESH) => {
                Json(json!({"access_token": REFRESHED_TOKEN, "token_type": "Bearer", "expires_in": 3600})).into_response()
            }
            _ => (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "Refresh token revoked"}))).into_response(),
        };
    }
    let Some(api) = path.strip_prefix("v1") else { return StatusCode::NOT_FOUND.into_response() };
    let token = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let ok_token = (token == FIRST_TOKEN || token == REFRESHED_TOKEN) && !mock.lock().unwrap().revoked.contains(token);
    if !ok_token {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"status": 401, "message": "The access token expired"}}))).into_response();
    }
    let not_found = || (StatusCode::NOT_FOUND, Json(json!({"error": {"status": 404, "message": "Resource not found"}}))).into_response();
    let m = method.as_str();
    let body = match (m, api) {
        ("GET", "/me") => {
            let product = mock.lock().unwrap().product.clone().unwrap_or_else(|| "premium".into());
            json!({"id": USER, "display_name": "Alice", "product": product, "email": "alice@example.org"})
        }
        ("GET", "/search") => {
            let types = q.get("type").cloned().unwrap_or_default();
            let mut v = json!({});
            if types.contains("track") {
                v["tracks"] = paging(vec![track_json(T1, "Never Gonna Give You Up"), track_json(T2, "Whenever You Need Somebody")], &q);
            }
            if types.contains("album") {
                v["albums"] = paging(vec![album_json()], &q);
            }
            if types.contains("artist") {
                v["artists"] = paging(vec![json!({"id": ARTIST, "name": "Rick Astley", "genres": ["dance pop"]})], &q);
            }
            if types.contains("playlist") {
                // Spotify answers `null` entries in playlist results.
                v["playlists"] = paging(vec![playlist_json(EDITORIAL, "spotify"), Value::Null], &q);
            }
            v
        }
        ("GET", "/me/tracks") => paging(vec![json!({"added_at": "2026-01-01T00:00:00Z", "track": track_json(T1, "Never Gonna Give You Up")})], &q),
        ("GET", "/me/albums") => paging(vec![json!({"added_at": "2026-01-01T00:00:00Z", "album": album_json()})], &q),
        ("GET", "/me/following") => {
            // Cursor paging: two pages of 50 then one.
            let all: Vec<Value> = (0..101).map(|i| json!({"id": format!("artist{i}"), "name": format!("Artist {i}")})).collect();
            let start = match q.get("after").map(String::as_str) {
                None => 0,
                Some("c50") => 50,
                Some("c100") => 100,
                Some(_) => return (StatusCode::BAD_REQUEST, "bad cursor").into_response(),
            };
            let items: Vec<Value> = all.iter().skip(start).take(50).cloned().collect();
            let after = if start + 50 < 101 { json!(format!("c{}", start + 50)) } else { Value::Null };
            json!({"artists": {"items": items, "total": 101, "cursors": {"after": after}, "limit": 50}})
        }
        ("GET", "/me/playlists") => paging(vec![playlist_json(MINE, USER), playlist_json(EDITORIAL, "spotify")], &q),
        ("GET", "/me/player/recently-played") => json!({"items": [
            {"track": track_json(T1, "Never Gonna Give You Up"), "played_at": "x"},
            {"track": track_json(T1, "Never Gonna Give You Up"), "played_at": "y"},
            {"track": track_json(T2, "Whenever You Need Somebody"), "played_at": "z"}
        ]}),
        ("GET", "/me/top/tracks") => paging(vec![track_json(T2, "Whenever You Need Somebody")], &q),
        ("GET", "/me/top/artists") => paging(vec![json!({"id": ARTIST, "name": "Rick Astley"})], &q),
        // Cut off for this client.
        ("GET", "/browse/new-releases") => return not_found(),
        ("GET", "/me/tracks/contains") => {
            let ids = q.get("ids").cloned().unwrap_or_default();
            json!(ids.split(',').map(|id| id == T1).collect::<Vec<_>>())
        }
        ("GET", "/me/albums/contains") | ("GET", "/me/following/contains") => {
            let ids = q.get("ids").cloned().unwrap_or_default();
            json!(ids.split(',').map(|_| false).collect::<Vec<_>>())
        }
        ("PUT", "/me/tracks") | ("DELETE", "/me/tracks") | ("PUT", "/me/following") => return StatusCode::OK.into_response(),
        // Removed from the API for this client: the generic endpoint takes over.
        ("PUT", "/me/albums") => return not_found(),
        ("PUT", "/me/library") => return StatusCode::OK.into_response(),
        ("GET", p) if p == format!("/albums/{ALBUM}") => album_json(),
        ("GET", p) if p == format!("/tracks/{T1}") => track_json(T1, "Never Gonna Give You Up"),
        ("GET", p) if p == format!("/artists/{ARTIST}") => json!({"id": ARTIST, "name": "Rick Astley", "genres": ["dance pop"]}),
        ("GET", p) if p == format!("/artists/{ARTIST}/albums") => paging(vec![album_json()], &q),
        ("GET", "/artists/ratelimited0000000000000/albums") => {
            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "7")], Json(json!({"error": {"status": 429, "message": "API rate limit exceeded"}})))
                .into_response();
        }
        ("GET", p) if p == format!("/playlists/{MINE}") => playlist_json(MINE, USER),
        ("GET", p) if p == format!("/playlists/{EDITORIAL}") => playlist_json(EDITORIAL, "spotify"),
        ("GET", p) if p == format!("/playlists/{MINE}/tracks") => paging(
            vec![json!({"track": track_json(T1, "Never Gonna Give You Up")}), json!({"track": track_json(T2, "Whenever You Need Somebody")})],
            &q,
        ),
        ("GET", p) if p.starts_with(&format!("/playlists/{EDITORIAL}/")) => return not_found(),
        ("GET", "/recommendations") => json!({"tracks": [track_json(T1, "Never Gonna Give You Up"), track_json(T2, "Whenever You Need Somebody")]}),
        ("POST", "/me/playlists") => {
            let b: Value = serde_json::from_str(&body).unwrap_or_default();
            json!({"id": "newplaylist000000000001", "name": b["name"], "owner": {"id": USER, "display_name": "Alice"}, "tracks": {"total": 0}})
        }
        ("PUT", p) if p == format!("/playlists/{MINE}") => return StatusCode::OK.into_response(),
        ("POST", p) | ("DELETE", p) | ("PUT", p) if p == format!("/playlists/{MINE}/tracks") => json!({"snapshot_id": "snap2"}),
        ("DELETE", p) if p == format!("/playlists/{MINE}/followers") => return StatusCode::OK.into_response(),
        _ => return not_found(),
    };
    Json(body).into_response()
}

fn url_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), urlencoding::decode(&v.replace('+', " ")).ok()?.into_owned()))
        })
        .collect()
}

async fn start_mock() -> (String, Shared) {
    let shared = Shared::default();
    let app = Router::new().route("/{*path}", any(mock)).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, shared)
}

struct Host {
    child: Child,
    /// The plugin's stderr (its log), kept to check what it writes there.
    log: PathBuf,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    notifications: Vec<Value>,
}

impl Host {
    fn spawn(base: &str) -> Self {
        let log = std::env::temp_dir().join(format!("spconnect-plugin-log-{}", uuid::Uuid::new_v4()));
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_spconnect"))
            .args(["plugin", "--api-base", &format!("{base}/v1"), "--accounts-base", base, "--no-session"])
            .env("RUST_LOG", "debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Host { child, log, stdin, lines, next_id: 0, notifications: vec![] }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), self.lines.next_line())
            .await
            .expect("plugin answered in time")
            .unwrap()
            .expect("plugin still running");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// Whole response (`result` or `error`) to one request.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id") == Some(&json!(id)) {
                assert_eq!(msg["jsonrpc"], "2.0");
                return msg;
            }
            assert!(msg.get("method").is_some(), "unexpected message {msg}");
            self.notifications.push(msg);
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg.get("error").is_none(), "{method} failed: {msg}");
        msg["result"].clone()
    }

    async fn error(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg.get("error").is_some(), "{method} should fail: {msg}");
        msg["error"].clone()
    }

    async fn err_code(&mut self, method: &str, params: Value) -> i64 {
        self.error(method, params).await["code"].as_i64().unwrap()
    }

    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self.notifications.iter().position(|n| n["method"] == method) {
            return self.notifications.remove(i);
        }
        loop {
            let msg = self.read().await;
            if msg["method"] == method {
                return msg;
            }
            self.notifications.push(msg);
        }
    }

    async fn initialize(&mut self, data_dir: &Path, settings: Value) -> Value {
        self.ok(
            "initialize",
            json!({"protocol": 1, "host": {"name": "Fake Player", "version": "0"}, "data_dir": data_dir,
                   "cache_dir": data_dir.join("cache"), "locale": "fr-FR",
                   "output": {"device": "hw:1,0", "bit_perfect": true, "max_rate": 96000, "max_bits": 24, "rates": [44100, 48000, 96000]},
                   "settings": settings}),
        )
        .await
    }

    async fn shutdown(mut self) -> String {
        assert_eq!(self.ok("shutdown", Value::Null).await, Value::Null);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.unwrap().unwrap();
        assert!(status.success());
        self.logs()
    }
}

fn data_dir(refresh: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spconnect-plugin-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(refresh) = refresh {
        let path = dir.join("credentials.json");
        let creds = json!({"username": USER, "auth_type": 1, "auth_data": "", "refresh_token": refresh, "display_name": "Alice", "product": "premium"});
        std::fs::write(&path, creds.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    dir
}

fn query_of(url: &str, key: &str) -> String {
    url.split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .map(|v| urlencoding::decode(v).unwrap().into_owned())
        .unwrap_or_else(|| panic!("no {key} in {url}"))
}

fn calls_to(mock: &Shared, method: &str, path: &str) -> Vec<(HashMap<String, String>, String)> {
    mock.lock().unwrap().calls.iter().filter(|c| c.0 == method && c.1 == path).map(|c| (c.2.clone(), c.3.clone())).collect()
}

/// Nothing of the account in the log: not its id, its e-mail or a token.
fn assert_private(log: &str) {
    for secret in [USER, "alice@example.org", FIRST_TOKEN, REFRESHED_TOKEN, REFRESH, CODE] {
        assert!(!log.contains(secret), "{secret:?} in the log");
    }
}

#[tokio::test]
async fn handshake_and_sign_in() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(None);
    let mut host = Host::spawn(&base);
    let init = host.initialize(&dir, json!({"quality": "normal"})).await;
    assert_eq!(init["protocol"], 1);
    assert_eq!(init["plugin"]["id"], "spotify");
    for cap in ["auth", "browse", "search", "resolve", "favorites", "reporting", "remote_control", "library", "lyrics", "playlist_edit", "details", "radio"] {
        assert_eq!(init["capabilities"][cap], true, "{cap}");
    }
    let settings = init["settings"].as_array().unwrap();
    assert_eq!(settings.iter().map(|s| s["key"].as_str().unwrap()).collect::<Vec<_>>(), ["quality", "report_playback", "connect"]);
    assert_eq!(settings[0]["label"], "Qualité de lecture", "labelled in the host's language");

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_eq!(host.err_code("browse.root", json!({})).await, -32001);
    assert_eq!(host.err_code("search", json!({"query": "rick"})).await, -32001);
    assert_eq!(host.err_code("track.resolve", json!({"ref": format!("track/{T1}"), "purpose": "play"})).await, -32001);
    assert_eq!(host.err_code("nothing.here", json!({})).await, -32601);
    assert_eq!(host.err_code("auth.complete", json!({"input": "code=x"})).await, -32602, "no sign-in started");

    // Sign-in by paste: the address of the page the browser lands on.
    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["expects_input"], true);
    let url = begin["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{base}/authorize?response_type=code&client_id=")));
    let redirect = query_of(url, "redirect_uri");
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/login"), "{redirect}");
    assert_eq!(query_of(url, "code_challenge_method"), "S256");
    mock.lock().unwrap().challenge = Some(query_of(url, "code_challenge"));
    let state = query_of(url, "state");
    let wrong = host.error("auth.complete", json!({"input": format!("{redirect}?code=nope&state={state}")})).await;
    assert_eq!(wrong["code"], -32602);
    assert!(wrong["message"].as_str().unwrap().contains("Invalid authorization code"));

    let begin = host.ok("auth.begin", json!({})).await;
    let url = begin["url"].as_str().unwrap().to_string();
    mock.lock().unwrap().challenge = Some(query_of(&url, "code_challenge"));
    let landed = format!("{}?code={CODE}&state={}", query_of(&url, "redirect_uri"), query_of(&url, "state"));
    let status = host.ok("auth.complete", json!({"input": landed})).await;
    assert_eq!(status, json!({"state": "signed_in", "account": {"display_name": "Alice", "detail": "Spotify Premium"}}));
    let creds_path = dir.join("credentials.json");
    assert_eq!(std::fs::metadata(&creds_path).unwrap().permissions().mode() & 0o777, 0o600);
    let creds: Value = serde_json::from_str(&std::fs::read_to_string(&creds_path).unwrap()).unwrap();
    assert_eq!(creds["refresh_token"], REFRESH);
    let token_call = &calls_to(&mock, "POST", "api/token")[1].1;
    assert!(token_call.contains("grant_type=authorization_code") && token_call.contains("code_verifier="));
    assert_eq!(host.ok("auth.status", json!({})).await["state"], "signed_in");

    // Sign-in through the loopback redirect: the browser comes back to the
    // plugin, which tells the host.
    assert_eq!(host.ok("auth.sign_out", json!({})).await, json!({"state": "signed_out"}));
    assert!(!creds_path.exists());
    let begin = host.ok("auth.begin", json!({})).await;
    let url = begin["url"].as_str().unwrap().to_string();
    mock.lock().unwrap().challenge = Some(query_of(&url, "code_challenge"));
    let redirect = query_of(&url, "redirect_uri");
    let forged = reqwest::get(format!("{redirect}?code={CODE}&state=forged")).await.unwrap().text().await.unwrap();
    assert!(forged.contains("did not complete"), "a state that does not match is ignored");
    let page = reqwest::get(format!("{redirect}?code={CODE}&state={}", query_of(&url, "state"))).await.unwrap().text().await.unwrap();
    assert!(page.contains("Signed in"));
    let changed = host.notification("auth.changed").await;
    assert_eq!(changed["params"]["state"], "signed_in");
    assert!(creds_path.exists());

    let log = host.shutdown().await;
    assert_private(&log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn catalogue_library_and_favourites() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(REFRESH));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    assert_eq!(
        host.ok("auth.status", json!({})).await,
        json!({"state": "signed_in", "account": {"display_name": "Alice", "detail": "Spotify Premium"}})
    );

    // New releases are not served to this client: left out. "Made for you"
    // holds the Spotify playlists the account follows.
    let root = host.ok("browse.root", json!({})).await;
    let sections: Vec<&str> = root["sections"].as_array().unwrap().iter().map(|s| s["ref"].as_str().unwrap()).collect();
    assert_eq!(sections, ["library", "my/playlists", "made-for-you", "recent", "top/tracks", "top/artists"]);
    assert_eq!(root["sections"][0]["title"], "Bibliothèque");
    let home: Vec<&str> = root["home"].as_array().unwrap().iter().map(|s| s["ref"].as_str().unwrap()).collect();
    assert_eq!(home, ["recent", "made-for-you", "top/tracks", "top/artists"]);
    let mfy = host.ok("browse.list", json!({"ref": "made-for-you"})).await;
    assert_eq!(mfy["items"][0]["ref"], format!("playlist/{EDITORIAL}"));
    assert_eq!(mfy["total"], 1);

    let library = host.ok("browse.list", json!({"ref": "library"})).await;
    assert_eq!(library["items"].as_array().unwrap().len(), 3);

    // Library lists.
    let tracks = host.ok("library.tracks", json!({"offset": 0, "limit": 200})).await;
    let t = &tracks["items"][0];
    assert_eq!(t["ref"], format!("track/{T1}"));
    assert_eq!((t["artist"].as_str(), t["album"].as_str(), t["year"].as_u64()), (Some("Rick Astley"), Some("Whenever You Need Somebody"), Some(1987)));
    assert_eq!(t["album_ref"], format!("album/{ALBUM}"));
    assert_eq!(t["artist_ref"], format!("artist/{ARTIST}"));
    assert_eq!(t["format"], json!({"sample_rate": 44100, "codec": "vorbis"}));
    assert_eq!(t["favorite"], true, "a saved track");
    assert_eq!(t["actions"][0]["label"], "Radio à partir de ce titre");
    assert_eq!((tracks["total"].as_u64(), tracks["has_more"].as_bool()), (Some(1), Some(false)));
    let albums = host.ok("library.albums", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(albums["items"][0]["track_count"], 2);
    assert_eq!(albums["items"][0]["art"], "https://i.scdn.co/image/cover");
    // Followed artists come by cursor; a page of 200 is four calls of 50.
    let artists = host.ok("library.artists", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(artists["items"].as_array().unwrap().len(), 101);
    assert_eq!((artists["total"].as_u64(), artists["has_more"].as_bool()), (Some(101), Some(false)));
    let page2 = host.ok("library.artists", json!({"offset": 100, "limit": 50})).await;
    assert_eq!(page2["items"][0]["ref"], "artist/artist100");
    assert_eq!(calls_to(&mock, "GET", "v1/me/following").last().unwrap().0.get("after").map(String::as_str), Some("c100"), "the cursor kept");
    let playlists = host.ok("library.playlists", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(playlists["items"][0]["editable"], true, "the account's own");
    assert_eq!(playlists["items"][1]["editable"], false, "followed");
    assert_eq!(playlists["items"][0]["track_count"], 2);
    let small = host.ok("library.playlists", json!({"offset": 0, "limit": 1})).await;
    assert_eq!(small["has_more"], true);

    let recent = host.ok("browse.list", json!({"ref": "recent"})).await;
    assert_eq!(recent["items"].as_array().unwrap().len(), 2, "each track once");

    // Album, artist, playlist pages.
    let album = host.ok("browse.list", json!({"ref": format!("album/{ALBUM}")})).await;
    assert_eq!(album["items"][1]["ref"], format!("track/{T2}"));
    assert_eq!(album["items"][1]["art"], "https://i.scdn.co/image/cover", "the album fills in");
    assert_eq!(album["items"][0]["favorite"], true);
    assert_eq!(album["items"][1]["favorite"], false);
    let artist = host.ok("browse.list", json!({"ref": format!("artist/{ARTIST}")})).await;
    assert_eq!(artist["items"][0]["ref"], format!("album/{ALBUM}"));
    let mine = host.ok("browse.list", json!({"ref": format!("playlist/{MINE}")})).await;
    assert_eq!(mine["items"][1]["entry_id"], format!("1:{T2}"));

    // Search: groups per kind; `null` entries skipped.
    let all = host.ok("search", json!({"query": "rick"})).await;
    let kinds: Vec<&str> = all["groups"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["track", "album", "artist", "playlist"]);
    assert_eq!(all["groups"][3]["items"].as_array().unwrap().len(), 1);
    let found = host.ok("search", json!({"query": "rick", "kinds": ["album"], "offset": 0, "limit": 10})).await;
    assert_eq!(found["groups"][0]["items"][0]["subtitle"], "Rick Astley · 1987");
    assert_eq!(calls_to(&mock, "GET", "v1/search").last().unwrap().0["type"], "album");
    assert_eq!(host.err_code("search", json!({"query": "  "})).await, -32602);

    // item.get, radio.next (Web API recommendations without a session).
    let item = host.ok("item.get", json!({"ref": format!("track/{T1}")})).await;
    assert_eq!((item["title"].as_str(), item["favorite"].as_bool()), (Some("Never Gonna Give You Up"), Some(true)));
    assert_eq!(host.ok("item.get", json!({"ref": "liked"})).await["kind"], "folder");
    let radio = host.ok("radio.next", json!({"seed": format!("track/{T1}"), "exclude": [], "limit": 5})).await;
    assert_eq!(radio["items"].as_array().unwrap().len(), 1, "the seed left out");
    assert_eq!(radio["items"][0]["ref"], format!("track/{T2}"));
    let excluded = host.ok("radio.next", json!({"seed": format!("album/{ALBUM}"), "exclude": [format!("track/{T1}")]})).await;
    assert_eq!(excluded["items"].as_array().unwrap().len(), 1);
    assert_eq!(host.err_code("radio.next", json!({"seed": "liked"})).await, -32602);

    // Details: what the Web API serves.
    let details = host.ok("item.details", json!({"ref": format!("album/{ALBUM}")})).await;
    let facts = details["facts"].as_array().unwrap();
    assert!(facts.contains(&json!({"label": "Label", "value": "RCA"})));
    assert!(facts.contains(&json!({"label": "Sortie", "value": "12/11/1987"})));
    assert!(facts.contains(&json!({"label": "℗", "value": "1987 RCA"})));

    // Favourites.
    assert_eq!(host.ok("favorites.set", json!({"ref": format!("track/{T2}"), "on": true})).await, Value::Null);
    assert_eq!(calls_to(&mock, "PUT", "v1/me/tracks")[0].0["ids"], T2);
    assert_eq!(host.ok("item.get", json!({"ref": format!("track/{T1}")})).await["favorite"], true);
    host.ok("favorites.set", json!({"ref": format!("track/{T1}"), "on": false})).await;
    assert_eq!(calls_to(&mock, "DELETE", "v1/me/tracks")[0].0["ids"], T1);
    let after = host.ok("browse.list", json!({"ref": format!("album/{ALBUM}")})).await;
    assert_eq!((after["items"][0]["favorite"].as_bool(), after["items"][1]["favorite"].as_bool()), (Some(false), Some(true)));
    host.ok("favorites.set", json!({"ref": format!("album/{ALBUM}"), "on": true})).await;
    assert_eq!(calls_to(&mock, "PUT", "v1/me/library")[0].0["uris"], format!("spotify:album:{ALBUM}"), "generic endpoint as fallback");
    host.ok("favorites.set", json!({"ref": format!("artist/{ARTIST}"), "on": true})).await;
    assert_eq!(calls_to(&mock, "PUT", "v1/me/following")[0].0["type"], "artist");
    assert_eq!(host.err_code("favorites.set", json!({"ref": "recent", "on": true})).await, -32602);

    let log = host.shutdown().await;
    assert_private(&log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn playlist_edits() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(REFRESH));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    let mine = format!("playlist/{MINE}");

    let created = host.ok("playlists.create", json!({"name": "  Nouvelle  ", "description": "d", "public": false})).await;
    assert_eq!((created["ref"].as_str(), created["editable"].as_bool()), (Some("playlist/newplaylist000000000001"), Some(true)));
    let body: Value = serde_json::from_str(&calls_to(&mock, "POST", "v1/me/playlists")[0].1).unwrap();
    assert_eq!(body, json!({"name": "Nouvelle", "public": false, "description": "d"}));

    assert_eq!(host.ok("playlists.rename", json!({"ref": mine, "name": "Matin"})).await, Value::Null);
    let body: Value = serde_json::from_str(&calls_to(&mock, "PUT", &format!("v1/playlists/{MINE}"))[0].1).unwrap();
    assert_eq!(body, json!({"name": "Matin"}));

    host.ok("playlists.add", json!({"ref": mine, "items": [format!("track/{T1}"), format!("track/{T2}")]})).await;
    let body: Value = serde_json::from_str(&calls_to(&mock, "POST", &format!("v1/playlists/{MINE}/tracks"))[0].1).unwrap();
    assert_eq!(body, json!({"uris": [format!("spotify:track:{T1}"), format!("spotify:track:{T2}")]}));
    assert_eq!(host.err_code("playlists.add", json!({"ref": mine, "items": [format!("album/{ALBUM}")]})).await, -32602);

    host.ok("playlists.remove", json!({"ref": mine, "entries": [format!("1:{T2}"), format!("4:{T2}"), format!("0:{T1}")]})).await;
    let body: Value = serde_json::from_str(&calls_to(&mock, "DELETE", &format!("v1/playlists/{MINE}/tracks"))[0].1).unwrap();
    assert_eq!(
        body,
        json!({"tracks": [{"uri": format!("spotify:track:{T2}"), "positions": [1, 4]}, {"uri": format!("spotify:track:{T1}"), "positions": [0]}],
               "snapshot_id": "snap1"})
    );
    assert_eq!(host.err_code("playlists.remove", json!({"ref": mine, "entries": ["junk"]})).await, -32602);

    // A down move lands where the host asked; past the end means last.
    host.ok("playlists.move", json!({"ref": mine, "entry": format!("0:{T1}"), "to": 1})).await;
    host.ok("playlists.move", json!({"ref": mine, "entry": format!("1:{T2}"), "to": 0})).await;
    host.ok("playlists.move", json!({"ref": mine, "entry": format!("0:{T1}"), "to": 99})).await;
    let moves: Vec<Value> = calls_to(&mock, "PUT", &format!("v1/playlists/{MINE}/tracks")).iter().map(|c| serde_json::from_str(&c.1).unwrap()).collect();
    assert_eq!(moves[0], json!({"range_start": 0, "insert_before": 2, "range_length": 1, "snapshot_id": "snap1"}));
    assert_eq!(moves[1]["insert_before"], 0);
    assert_eq!(moves[2]["insert_before"], 2, "past the end: last of two");

    // Someone else's playlist is never touched.
    let editorial = format!("playlist/{EDITORIAL}");
    assert_eq!(host.err_code("playlists.rename", json!({"ref": editorial, "name": "x"})).await, -32602);
    assert_eq!(host.err_code("playlists.delete", json!({"ref": editorial})).await, -32602);
    assert_eq!(host.err_code("playlists.move", json!({"ref": editorial, "entry": format!("0:{T1}"), "to": 1})).await, -32602);
    assert!(calls_to(&mock, "PUT", &format!("v1/playlists/{EDITORIAL}")).is_empty());

    // Spotify has no deletion: the owner unfollows.
    assert_eq!(host.ok("playlists.delete", json!({"ref": mine})).await, Value::Null);
    assert_eq!(calls_to(&mock, "DELETE", &format!("v1/playlists/{MINE}/followers")).len(), 1);

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn errors_and_token_renewal() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(REFRESH));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    let limited = host.error("browse.list", json!({"ref": "artist/ratelimited0000000000000"})).await;
    assert_eq!((limited["code"].as_i64(), &limited["data"]), (Some(-32004), &json!({"retry_after": 7})));
    assert_eq!(host.err_code("browse.list", json!({"ref": "nonsense"})).await, -32002);
    assert_eq!(host.err_code("browse.list", json!({"ref": format!("track/{T1}")})).await, -32602);
    assert_eq!(host.err_code("item.get", json!({"ref": "album/doesnotexist"})).await, -32002);
    // Editorial playlist withheld by the Web API, and no session to ask instead.
    assert_eq!(host.err_code("browse.list", json!({"ref": format!("playlist/{EDITORIAL}")})).await, -32002);
    // Audio and lyrics need a session.
    assert_eq!(host.err_code("track.resolve", json!({"ref": format!("track/{T1}"), "purpose": "play"})).await, -32003);
    assert_eq!(host.err_code("lyrics.get", json!({"ref": format!("track/{T1}")})).await, -32002);

    // A refused token is renewed once, and the request goes through.
    let refreshes = |m: &Shared| calls_to(m, "POST", "api/token").len();
    let before = refreshes(&mock);
    mock.lock().unwrap().revoked.insert(REFRESHED_TOKEN.into());
    // Mid-session revocation of every token: the refresh hands out the same
    // revoked one again, so the request fails with `auth_required`…
    assert_eq!(host.err_code("browse.list", json!({"ref": "liked"})).await, -32001);
    assert!(refreshes(&mock) > before, "a new token was asked for");
    // …and the host hears of it once.
    assert_eq!(host.notification("auth.changed").await["params"]["state"], "expired");
    mock.lock().unwrap().revoked.clear();
    assert_eq!(host.ok("browse.list", json!({"ref": "liked"})).await["total"], 1);

    // Notifications are accepted without answers.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": {"output": {"max_rate": 48000}}})).await;
    host.send(json!({"jsonrpc": "2.0", "method": "settings.changed", "params": {"settings": {"report_playback": false, "connect": false}}})).await;
    host.send(json!({"jsonrpc": "2.0", "method": "locale.changed", "params": {"locale": "en-GB"}})).await;
    let declared = host.notification("settings.declared").await;
    assert_eq!(declared["params"]["settings"][0]["label"], "Streaming quality");
    host.send(json!({"jsonrpc": "2.0", "method": "playback.started", "params": {"ref": format!("track/{T1}")}})).await;
    host.send(json!({"jsonrpc": "2.0", "method": "player.state", "params": {"status": "playing", "item_ref": null, "pos_ms": 0, "dur_ms": 0, "volume": 50, "muted": false}})).await;
    assert_eq!(host.ok("auth.status", json!({})).await["state"], "signed_in");

    let log = host.shutdown().await;
    assert_private(&log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn revoked_sign_in_is_expired() {
    let (base, _mock) = start_mock().await;
    let dir = data_dir(Some("revoked-refresh"));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    assert_eq!(
        host.ok("auth.status", json!({})).await,
        json!({"state": "expired", "account": {"display_name": "Alice", "detail": "Spotify Premium"}})
    );
    assert_eq!(host.err_code("browse.list", json!({"ref": "liked"})).await, -32001);
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn free_accounts_are_refused() {
    let (base, mock) = start_mock().await;
    mock.lock().unwrap().product = Some("free".into());
    let dir = data_dir(None);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    let creds_path = dir.join("credentials.json");

    // By paste: the dialog shows why.
    let begin = host.ok("auth.begin", json!({})).await;
    let url = begin["url"].as_str().unwrap().to_string();
    mock.lock().unwrap().challenge = Some(query_of(&url, "code_challenge"));
    let landed = format!("{}?code={CODE}&state={}", query_of(&url, "redirect_uri"), query_of(&url, "state"));
    let refused = host.error("auth.complete", json!({"input": landed})).await;
    assert_eq!(refused["code"], -32602);
    assert_eq!(
        refused["message"],
        "Les comptes Spotify Free ne sont pas pris en charge : la lecture demande un abonnement Spotify Premium."
    );
    assert!(!creds_path.exists());
    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));

    // Through the loopback redirect: the browser page says why.
    let begin = host.ok("auth.begin", json!({})).await;
    let url = begin["url"].as_str().unwrap().to_string();
    mock.lock().unwrap().challenge = Some(query_of(&url, "code_challenge"));
    let redirect = query_of(&url, "redirect_uri");
    let page = reqwest::get(format!("{redirect}?code={CODE}&state={}", query_of(&url, "state"))).await.unwrap().text().await.unwrap();
    assert!(page.contains("Spotify Free"), "{page}");
    assert!(!creds_path.exists());

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stored_free_account_is_signed_out() {
    let (base, mock) = start_mock().await;
    mock.lock().unwrap().product = Some("free".into());
    // Credentials saved without the subscription, as older versions did.
    let dir = data_dir(Some(REFRESH));
    let creds_path = dir.join("credentials.json");
    let mut creds: Value = serde_json::from_str(&std::fs::read_to_string(&creds_path).unwrap()).unwrap();
    creds.as_object_mut().unwrap().remove("product");
    std::fs::write(&creds_path, creds.to_string()).unwrap();

    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert!(!creds_path.exists());
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
