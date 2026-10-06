//! The Spotify account: OAuth sign-in (PKCE, loopback redirect), the
//! stored reusable credentials, the librespot session built from them, and
//! the access tokens the Web API needs.
//!
//! Nothing here logs the account name, the user id or a token: hosts keep
//! the plugin's stderr in their logs and diagnostic reports.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use librespot_core::authentication::Credentials;
use librespot_core::{Session, SessionConfig};

use crate::probe;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The client id librespot presents (Spotify's desktop "keymaster" client).
/// It accepts loopback redirects on any port.
pub const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
pub const ACCOUNTS_BASE: &str = "https://accounts.spotify.com";
/// Asked for at sign-in. `streaming` lets the token open a session; the
/// others are for the Web API when it is used with the OAuth token itself.
pub const SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "user-library-read",
    "user-library-modify",
    "user-follow-read",
    "user-follow-modify",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-read-recently-played",
    "user-top-read",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "app-remote-control",
];
/// Tokens are renewed this long before they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
/// A dead session is not rebuilt more often than this.
const RECONNECT_GAP: Duration = Duration::from_secs(5);

/// No stored credentials, or Spotify refused them: sign in again.
#[derive(Debug)]
pub struct SignedOut;

/// The account is not Premium. librespot refuses such accounts, and ends
/// the whole process when an access point says so: the subscription is
/// checked through the Web API before any session is opened.
#[derive(Debug)]
pub struct PremiumRequired;

impl std::fmt::Display for PremiumRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::plugin::items::tr(
            "Spotify Free accounts are not supported: playback needs a Spotify Premium subscription.",
            "Les comptes Spotify Free ne sont pas pris en charge : la lecture demande un abonnement Spotify Premium.",
        ))
    }
}

impl std::error::Error for PremiumRequired {}

/// `product` of `/me`, or the access point's account `type`.
pub fn is_premium(product: &str) -> bool {
    product == "premium"
}

impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not signed in to Spotify")
    }
}

impl std::error::Error for SignedOut {}

/// `data_dir/credentials.json`.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct Stored {
    /// Spotify account name, needed to log in again. Never logged.
    pub username: String,
    /// librespot's reusable credentials (`AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS`).
    #[serde(default)]
    pub auth_type: i32,
    /// Base64.
    #[serde(default)]
    pub auth_data: String,
    /// OAuth refresh token of the sign-in, for the Web API when no session
    /// token can be had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub display_name: String,
    /// `premium`, `free`… from the session's attributes or `/me`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stored { … }")
    }
}

fn credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("credentials.json")
}

impl Stored {
    pub fn load(data_dir: &Path) -> Result<Option<Stored>> {
        let path = credentials_path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Stored>(&raw) {
                Ok(s) if !s.username.is_empty() => Ok(Some(s)),
                Ok(_) => Ok(None),
                Err(e) => {
                    tracing::warn!("{} unreadable ({e}); signed out", path.display());
                    Ok(None)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Written with mode 600: the file gives access to the account.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let path = credentials_path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let raw = serde_json::to_vec_pretty(self)?;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&raw)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn delete(data_dir: &Path) -> Result<()> {
        match std::fs::remove_file(credentials_path(data_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn credentials(&self) -> Option<Credentials> {
        let auth_data = STANDARD.decode(&self.auth_data).ok().filter(|d| !d.is_empty())?;
        let auth_type = protobuf::Enum::from_i32(self.auth_type)?;
        Some(Credentials { username: Some(self.username.clone()), auth_type, auth_data })
    }
}

/// PKCE pair and anti-forgery state of one sign-in.
#[derive(Clone, Debug, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

fn random_string(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| UNRESERVED[rng.random_range(0..UNRESERVED.len())] as char).collect()
}

pub fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = random_string(64);
        Pkce { challenge: challenge_of(&verifier), verifier, state: random_string(24) }
    }
}

/// The page the browser opens.
pub fn authorize_url(accounts_base: &str, redirect_uri: &str, pkce: &Pkce) -> String {
    let q = |s: &str| urlencoding::encode(s).into_owned();
    format!(
        "{accounts_base}/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={}&scope={}\
         &code_challenge_method=S256&code_challenge={}&state={}",
        q(redirect_uri),
        q(&SCOPES.join(" ")),
        pkce.challenge,
        pkce.state
    )
}

/// The authorisation code in what the user pasted: the whole address of the
/// page the browser landed on, its query, or the bare code.
pub fn extract_code(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let query = match input.split_once('?') {
        Some((_, q)) => q,
        None if input.contains('=') => input,
        None => {
            let bare = input.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c));
            return bare.then(|| input.to_string());
        }
    };
    let query = query.split('#').next().unwrap_or("");
    query.split('&').find_map(|kv| {
        let v = kv.strip_prefix("code=")?;
        urlencoding::decode(v).ok().map(|v| v.into_owned()).filter(|v| !v.is_empty())
    })
}

/// `/api/token` answer.
#[derive(Deserialize, Debug)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// The accounts service refused the code or the refresh token.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spotify refused the sign-in: {}", self.0)
    }
}

impl std::error::Error for Refused {}

async fn token_request(http: &reqwest::Client, accounts_base: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = http.post(format!("{accounts_base}/api/token")).form(form).send().await.context("accounts service")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_client_error() {
        // `{"error": "invalid_grant", "error_description": "…"}`: no token in it.
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error_description").or(v.get("error")).and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(Refused(reason).into());
    }
    if !status.is_success() {
        bail!("accounts service: HTTP {}", status.as_u16());
    }
    serde_json::from_str(&body).context("accounts service: unreadable token")
}

pub async fn exchange_code(
    http: &reqwest::Client,
    accounts_base: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    token_request(http, accounts_base, &form).await
}

pub async fn refresh(http: &reqwest::Client, accounts_base: &str, refresh_token: &str) -> Result<TokenResponse> {
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", CLIENT_ID)];
    token_request(http, accounts_base, &form).await
}

/// Stable device id of this installation (`data_dir/device.json`): Spotify
/// Connect keeps showing the same device.
pub fn device_id(data_dir: &Path) -> String {
    let path = data_dir.join("device.json");
    let stored = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("device_id").and_then(|d| d.as_str()).map(str::to_string))
        .filter(|id| id.len() == 40 && id.chars().all(|c| c.is_ascii_hexdigit()));
    if let Some(id) = stored {
        return id;
    }
    let mut rng = rand::rng();
    let id: String = (0..20).map(|_| format!("{:02x}", rng.random::<u8>())).collect();
    let _ = std::fs::create_dir_all(data_dir);
    if let Err(e) = std::fs::write(&path, serde_json::json!({"device_id": id}).to_string()) {
        tracing::warn!("cannot save the device id: {e}");
    }
    id
}

/// Where the account's pieces come from.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub accounts_base: Option<String>,
    /// Web API root, for the subscription check.
    pub api_base: Option<String>,
    /// Web API only (tests): no access point, no audio.
    pub no_session: bool,
}

struct OAuthToken {
    token: String,
    expires: Instant,
}

/// The signed-in account, shared by everything that talks to Spotify.
pub struct Account {
    data_dir: PathBuf,
    accounts_base: String,
    api_base: String,
    no_session: bool,
    device_id: String,
    pub http: reqwest::Client,
    stored: Mutex<Option<Stored>>,
    session: tokio::sync::Mutex<Option<Session>>,
    last_connect: Mutex<Option<Instant>>,
    oauth: Mutex<Option<OAuthToken>>,
}

impl Account {
    pub fn new(data_dir: PathBuf, opts: &Options) -> Result<Self> {
        let stored = Stored::load(&data_dir)?;
        Ok(Account {
            device_id: device_id(&data_dir),
            data_dir,
            accounts_base: opts.accounts_base.clone().unwrap_or_else(|| ACCOUNTS_BASE.into()),
            api_base: opts.api_base.clone().unwrap_or_else(|| crate::webapi::API_BASE.into()).trim_end_matches('/').to_string(),
            no_session: opts.no_session,
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
            stored: Mutex::new(stored),
            session: tokio::sync::Mutex::new(None),
            last_connect: Mutex::new(None),
            oauth: Mutex::new(None),
        })
    }

    pub fn accounts_base(&self) -> &str {
        &self.accounts_base
    }

    pub fn has_session_support(&self) -> bool {
        !self.no_session
    }

    pub fn stored(&self) -> Option<Stored> {
        self.stored.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn signed_in(&self) -> bool {
        self.stored().is_some()
    }

    fn set_stored(&self, stored: Option<Stored>) -> Result<()> {
        match &stored {
            Some(s) => s.save(&self.data_dir)?,
            None => Stored::delete(&self.data_dir)?,
        }
        *self.stored.lock().unwrap_or_else(|e| e.into_inner()) = stored;
        Ok(())
    }

    /// Display name and subscription, once known.
    pub fn note_profile(&self, display_name: Option<&str>, product: Option<&str>) {
        let Some(mut s) = self.stored() else { return };
        let before = s.clone();
        if let Some(name) = display_name.filter(|n| !n.trim().is_empty()) {
            s.display_name = name.trim().to_string();
        }
        if let Some(p) = product.filter(|p| !p.is_empty()) {
            s.product = Some(p.to_string());
        }
        if s != before {
            let _ = self.set_stored(Some(s));
        }
    }

    pub async fn sign_out(&self) -> Result<()> {
        if let Some(session) = self.session.lock().await.take() {
            session.shutdown();
        }
        *self.oauth.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.set_stored(None)
    }

    /// Sign in with an OAuth token: open a session with it and keep the
    /// reusable credentials it hands out. Without a session (tests), only
    /// the refresh token is kept.
    pub async fn sign_in(&self, token: TokenResponse, username_hint: Option<String>) -> Result<()> {
        if let Some(old) = self.session.lock().await.take() {
            old.shutdown();
        }
        let mut stored = Stored { refresh_token: token.refresh_token.clone(), ..Default::default() };
        if !self.no_session {
            // A first connection in a child process: see `probe`.
            let checked = probe::check(&self.device_id, &Credentials::with_access_token(&token.access_token)).await?;
            stored.username = checked.username;
            stored.auth_type = checked.auth_type;
            stored.auth_data = checked.auth_data;
            stored.product = checked.product;
        } else {
            // Web API only (tests): the subscription comes from `/me`.
            let product = self.product_of(&token.access_token).await.context("cannot check the Spotify subscription")?;
            if product.as_deref().is_some_and(|p| !is_premium(p)) {
                return Err(PremiumRequired.into());
            }
            stored.product = product;
            stored.username = username_hint.unwrap_or_else(|| "account".into());
        }
        if stored.username.is_empty() {
            bail!("Spotify did not name the account");
        }
        *self.oauth.lock().unwrap_or_else(|e| e.into_inner()) = Some(OAuthToken {
            token: token.access_token,
            expires: Instant::now() + Duration::from_secs(token.expires_in.unwrap_or(3600)).saturating_sub(TOKEN_MARGIN),
        });
        self.set_stored(Some(stored))
    }

    fn new_session(&self) -> Session {
        let config = SessionConfig { device_id: self.device_id.clone(), ..SessionConfig::default() };
        Session::new(config, None)
    }

    /// The connected session, opened again from the stored credentials when
    /// it has died.
    pub async fn session(&self) -> Result<Session> {
        if self.no_session {
            bail!(librespot_core::Error::unavailable(anyhow!("no Spotify session (Web API only)")));
        }
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref().filter(|s| !s.is_invalid()) {
            return Ok(s.clone());
        }
        let stored = self.stored().ok_or(SignedOut)?;
        let creds = stored.credentials().ok_or(SignedOut)?;
        let stored = self.check_premium(stored, &creds).await?;
        let wait = {
            let last = self.last_connect.lock().unwrap_or_else(|e| e.into_inner());
            last.and_then(|t| RECONNECT_GAP.checked_sub(t.elapsed()))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
        *self.last_connect.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let session = self.new_session();
        session.connect(creds, false).await.map_err(login_error)?;
        tracing::info!("connected to Spotify");
        // The access point may hand out fresher credentials.
        let auth_data = STANDARD.encode(session.auth_data());
        let product = session.get_user_attribute("type");
        if !session.auth_data().is_empty() && (auth_data != stored.auth_data || product != stored.product) {
            let mut s = stored;
            s.auth_data = auth_data;
            s.product = product.or(s.product);
            if let Err(e) = self.set_stored(Some(s)) {
                tracing::warn!("cannot update the credentials: {e:#}");
            }
        }
        *guard = Some(session.clone());
        Ok(session)
    }

    /// Refuse to open a session for an account that is not Premium: librespot
    /// would end the plugin. Credentials whose subscription is not known yet
    /// (saved by older versions) go through [`probe::check`] first.
    async fn check_premium(&self, stored: Stored, creds: &Credentials) -> Result<Stored> {
        match stored.product.as_deref() {
            Some(p) if is_premium(p) => Ok(stored),
            Some(_) => Err(PremiumRequired.into()),
            None => {
                let checked = match probe::check(&self.device_id, creds).await {
                    Err(e) if e.chain().any(|c| c.is::<PremiumRequired>()) => {
                        self.note_profile(None, Some("free"));
                        return Err(e);
                    }
                    r => r?,
                };
                let mut s = stored;
                s.product = checked.product;
                self.set_stored(Some(s.clone()))?;
                Ok(s)
            }
        }
    }

    /// `product` of `GET /me` with `token`: `premium`, `free`… `None` when
    /// the answer does not say.
    async fn product_of(&self, token: &str) -> Result<Option<String>> {
        let r = self.http.get(format!("{}/me", self.api_base)).bearer_auth(token).send().await?;
        let status = r.status();
        if !status.is_success() {
            bail!("GET /me: HTTP {}", status.as_u16());
        }
        let v: serde_json::Value = r.json().await?;
        Ok(v.get("product").and_then(|p| p.as_str()).filter(|p| !p.is_empty()).map(str::to_string))
    }

    /// Close `session` if it is still the current one: the next call to
    /// [`Account::session`] opens a new one.
    pub async fn drop_session(&self, session: &Session) {
        let mut guard = self.session.lock().await;
        if guard.as_ref().is_some_and(|s| s.session_id() == session.session_id())
            && let Some(s) = guard.take()
        {
            s.shutdown();
        }
    }

    /// The current session if one is open, without connecting.
    pub async fn current_session(&self) -> Option<Session> {
        self.session.lock().await.clone().filter(|s| !s.is_invalid())
    }

    /// An access token for the Web API, other than `rejected` (a token the
    /// API just refused). From the session (login5), else from the sign-in's
    /// refresh token, else from the session's older token service.
    pub async fn web_token(&self, rejected: Option<&str>) -> Result<String> {
        if self.stored().is_none() {
            return Err(SignedOut.into());
        }
        let ok = |t: &str| Some(t) != rejected;
        let mut session_error = None;
        if !self.no_session {
            match self.session().await {
                Ok(session) => match session.login5().auth_token().await {
                    Ok(t) if ok(&t.access_token) => return Ok(t.access_token),
                    Ok(_) => {}
                    Err(e) => tracing::debug!("login5 token: {e}"),
                },
                Err(e) if e.chain().any(|c| c.is::<SignedOut>()) => return Err(e),
                Err(e) => session_error = Some(e),
            }
        }
        match self.oauth_token(rejected).await {
            Ok(Some(t)) => return Ok(t),
            Ok(None) => {}
            Err(e) if e.chain().any(|c| c.is::<Refused>()) => {
                tracing::info!("refresh token refused: {e:#}");
                if self.no_session {
                    return Err(anyhow::Error::from(SignedOut).context(format!("{e:#}")));
                }
            }
            Err(e) => tracing::debug!("refresh token: {e:#}"),
        }
        if let Some(session) = self.current_session().await {
            let t = session.token_provider().get_token(&SCOPES.join(",")).await?;
            if ok(&t.access_token) {
                return Ok(t.access_token);
            }
        }
        Err(session_error.unwrap_or_else(|| anyhow!("no Web API token available")))
    }

    async fn oauth_token(&self, rejected: Option<&str>) -> Result<Option<String>> {
        {
            let cached = self.oauth.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = cached.as_ref().filter(|t| t.expires > Instant::now() && Some(t.token.as_str()) != rejected) {
                return Ok(Some(t.token.clone()));
            }
        }
        let Some(refresh_token) = self.stored().and_then(|s| s.refresh_token) else { return Ok(None) };
        let fresh = refresh(&self.http, &self.accounts_base, &refresh_token).await?;
        if let Some(rotated) = fresh.refresh_token.as_ref().filter(|r| **r != refresh_token)
            && let Some(mut s) = self.stored()
        {
            s.refresh_token = Some(rotated.clone());
            self.set_stored(Some(s))?;
        }
        let expires = Instant::now() + Duration::from_secs(fresh.expires_in.unwrap_or(3600)).saturating_sub(TOKEN_MARGIN);
        *self.oauth.lock().unwrap_or_else(|e| e.into_inner()) = Some(OAuthToken { token: fresh.access_token.clone(), expires });
        Ok(Some(fresh.access_token))
    }
}

/// Access point refusals that mean "sign in again".
fn login_error(e: librespot_core::Error) -> anyhow::Error {
    use librespot_core::error::ErrorKind;
    // `LoginFailed` from the access point is the only `PermissionDenied`
    // a connection gives.
    let refused = matches!(e.kind, ErrorKind::PermissionDenied | ErrorKind::Unauthenticated);
    let premium = format!("{e}").contains("Premium");
    if premium {
        return PremiumRequired.into();
    }
    if refused {
        return anyhow::Error::from(SignedOut).context(format!("Spotify refused the stored credentials: {e}"));
    }
    anyhow::Error::from(e).context("connecting to Spotify")
}

/// A code caught by the loopback redirect, and where to send the outcome
/// of the sign-in it starts (`Err`: the message to show).
pub type CallbackCode = (String, tokio::sync::oneshot::Sender<Result<(), String>>);

/// How long the browser page waits for the sign-in to finish.
const CALLBACK_WAIT: Duration = Duration::from_secs(30);

/// Serve the loopback redirect of a sign-in: `GET /login?code=…&state=…`.
/// Each code whose state matches goes to `codes`; the page then shows how
/// the sign-in ended.
pub fn serve_callback(
    listener: tokio::net::TcpListener,
    state: String,
    codes: tokio::sync::mpsc::UnboundedSender<CallbackCode>,
) -> tokio::task::JoinHandle<()> {
    use axum::extract::{Query, State};
    use axum::response::Html;
    use axum::routing::get;
    type Shared = (String, tokio::sync::mpsc::UnboundedSender<CallbackCode>);

    async fn login(
        State(shared): State<std::sync::Arc<Shared>>,
        Query(q): Query<std::collections::HashMap<String, String>>,
    ) -> Html<String> {
        let (state, codes) = &*shared;
        let (Some(code), Some(s)) = (q.get("code"), q.get("state")) else { return Html(page(PAGE_FAILED)) };
        if s != state {
            return Html(page(PAGE_FAILED));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if codes.send((code.clone(), tx)).is_err() {
            return Html(page(PAGE_FAILED));
        }
        match tokio::time::timeout(CALLBACK_WAIT, rx).await {
            Ok(Ok(Ok(()))) => Html(page(PAGE_DONE)),
            Ok(Ok(Err(message))) => Html(page(&message)),
            _ => Html(page(PAGE_FAILED)),
        }
    }

    let app = axum::Router::new().route("/login", get(login)).with_state(std::sync::Arc::new((state, codes)));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("sign-in listener stopped: {e}");
        }
    })
}

const PAGE_DONE: &str = "Signed in. You can close this page and go back to the player.";
const PAGE_FAILED: &str = "Sign-in did not complete. Start it again from the player.";

/// The browser page, with `message` as plain text.
fn page(message: &str) -> String {
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<!doctype html><meta charset=utf-8><title>Spotify</title><p>{escaped}</p>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::authentication::AuthenticationType;

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let p = Pkce::new();
        assert_eq!(p.verifier.len(), 64);
        assert_eq!(p.challenge, challenge_of(&p.verifier));
        assert_ne!(Pkce::new().state, p.state);
    }

    #[test]
    fn authorize_url_carries_the_pkce_challenge() {
        let p = Pkce { verifier: "v".into(), challenge: "ch".into(), state: "st".into() };
        let url = authorize_url(ACCOUNTS_BASE, "http://127.0.0.1:4567/login", &p);
        assert!(url.starts_with("https://accounts.spotify.com/authorize?response_type=code&client_id="));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A4567%2Flogin"));
        assert!(url.contains("code_challenge_method=S256&code_challenge=ch&state=st"));
        assert!(url.contains("scope=streaming%20user-read-private"));
    }

    #[test]
    fn codes_are_found_in_what_users_paste() {
        assert_eq!(extract_code("http://127.0.0.1:4567/login?code=AQB-x_1&state=s").as_deref(), Some("AQB-x_1"));
        assert_eq!(extract_code("  code=abc&state=s ").as_deref(), Some("abc"));
        assert_eq!(extract_code("AQBxyz").as_deref(), Some("AQBxyz"));
        assert_eq!(extract_code("http://127.0.0.1/login?error=access_denied"), None);
        assert_eq!(extract_code("not a code"), None);
        assert_eq!(extract_code(""), None);
    }

    #[test]
    fn credentials_are_private_and_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spconnect-creds-{}", uuid::Uuid::new_v4()));
        let s = Stored {
            username: "u".into(),
            auth_type: 1,
            auth_data: STANDARD.encode(b"blob"),
            refresh_token: Some("r".into()),
            display_name: "Ann".into(),
            product: Some("premium".into()),
        };
        s.save(&dir).unwrap();
        let mode = std::fs::metadata(dir.join("credentials.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(Stored::load(&dir).unwrap(), Some(s.clone()));
        let creds = s.credentials().unwrap();
        assert_eq!(creds.auth_data, b"blob");
        assert_eq!(creds.auth_type, AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS);
        assert_eq!(format!("{s:?}"), "Stored { … }", "nothing of the account in debug output");
        Stored::delete(&dir).unwrap();
        assert_eq!(Stored::load(&dir).unwrap(), None);
        let id = device_id(&dir);
        assert_eq!(id.len(), 40);
        assert_eq!(device_id(&dir), id, "stable");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
