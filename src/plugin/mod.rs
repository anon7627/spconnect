//! `spconnect plugin`: source plugin (protocol v1, see `docs/plugin.md`).
//! JSON-RPC on stdin/stdout.
//!
//! Once signed in, the plugin also shows the player as a Spotify Connect
//! device and drives it through `player.*` requests (`remote_control`).

pub mod catalog;
mod details;
pub mod items;
mod lyrics;
mod playlists;
mod relay;
mod report;
mod resolve;
pub mod rpc;
mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::task::AbortHandle;

use crate::connect::{self, HostEvent, HostState};
use crate::session::{self, Account, Pkce, SignedOut};
use crate::webapi::{HttpError, WebApi};
use resolve::Output;
use rpc::{AUTH_REQUIRED, Incoming, Out, RpcError, RpcResult};

pub const PROTOCOL: u64 = 1;
/// How long `auth.begin`'s loopback listener waits for the browser.
const LOGIN_WAIT: Duration = Duration::from_secs(15 * 60);
/// Redirect named when no loopback listener could be opened (paste mode).
const FALLBACK_REDIRECT: &str = "http://127.0.0.1:5588/login";

/// Command-line overrides.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub api_base: Option<String>,
    pub accounts_base: Option<String>,
    pub no_session: bool,
}

/// A sign-in under way: what `auth.complete` needs.
struct Login {
    pkce: Pkce,
    redirect: String,
    handles: Vec<AbortHandle>,
}

struct State {
    data_dir: Option<PathBuf>,
    output: Output,
    login: Option<Login>,
    /// `auth.changed {expired}` already sent for the current credentials.
    expiry_notified: bool,
    /// Host name from `initialize`, for the Spotify Connect device name.
    host_name: String,
    lang: String,
    settings: settings::Settings,
    connect: Option<ConnectHandle>,
}

/// The running Spotify Connect device.
struct ConnectHandle {
    task: AbortHandle,
    events: tokio::sync::mpsc::UnboundedSender<HostEvent>,
}

struct Plugin {
    opts: Options,
    out: Out,
    state: Mutex<State>,
    account: Mutex<Option<Arc<Account>>>,
    reports: report::Reporter,
    /// Setting `report_playback`, shared with the Connect device.
    report_flag: Arc<AtomicBool>,
    /// Serves tracks as plain Ogg files.
    relay: relay::Relay,
}

pub async fn run(opts: Options) -> Result<()> {
    let (out, writer) = Out::stdout();
    let plugin = Arc::new(Plugin {
        opts,
        out: out.clone(),
        state: Mutex::new(State {
            data_dir: None,
            output: Output::default(),
            login: None,
            expiry_notified: false,
            host_name: "spconnect".into(),
            lang: "en".into(),
            settings: settings::Settings::default(),
            connect: None,
        }),
        account: Mutex::new(None),
        reports: report::Reporter::default(),
        report_flag: Arc::new(AtomicBool::new(true)),
        relay: relay::Relay::default(),
    });
    tracing::info!("plugin mode, protocol v{PROTOCOL}");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match rpc::parse(&line) {
            Incoming::Request { id, method, params } => match method.as_str() {
                // Handled in order: nothing else makes sense before it ends.
                "initialize" => out.respond(id, plugin.clone().initialize(params).await),
                "shutdown" => {
                    out.respond(id, Ok(Value::Null));
                    break;
                }
                _ => {
                    let plugin = plugin.clone();
                    tokio::spawn(async move {
                        let result = plugin.clone().handle(&method, params).await;
                        plugin.note_auth_failure(&method, &result);
                        plugin.out.respond(id, result);
                    });
                }
            },
            Incoming::Notification { method, params } => plugin.notification(&method, params),
            Incoming::Response { id, result } => out.deliver(&id, result),
            Incoming::Invalid { id, error } => out.respond(id, Err(error)),
        }
    }
    tracing::info!("plugin stopping");
    plugin.cancel_login();
    plugin.stop_connect();
    // The device leaves Spotify Connect.
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(out);
    drop(plugin);
    // In-flight tasks may still hold a sender; give the writer a moment.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    // The blocking stdin reader would keep the runtime alive.
    std::process::exit(0)
}

/// Language of a locale: `fr-FR`, `fr_FR.UTF-8`, `fr` → `fr`; `en` if none.
fn lang_of(locale: Option<&str>) -> String {
    let lang = locale.unwrap_or("").split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
    if lang.is_empty() { "en".into() } else { lang }
}

fn params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

/// "Premium" from `premium`.
fn product_label(product: &str) -> String {
    let mut c = product.chars();
    match c.next() {
        Some(f) => format!("Spotify {}{}", f.to_uppercase(), c.as_str()),
        None => "Spotify".into(),
    }
}

/// The machine's name, for the Connect device.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default()
}

/// "<host name> (<machine name>)".
pub fn device_name(host_name: &str, machine: &str) -> String {
    if machine.is_empty() { host_name.to_string() } else { format!("{host_name} ({machine})") }
}

#[derive(Deserialize)]
struct InitializeParams {
    data_dir: PathBuf,
    #[serde(default)]
    output: Option<Output>,
    #[serde(default)]
    protocol: Option<u64>,
    #[serde(default)]
    host: Option<HostInfo>,
    #[serde(default)]
    locale: Option<String>,
    /// Values the host stored for our settings.
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Deserialize)]
struct HostInfo {
    name: String,
}

#[derive(Deserialize)]
struct RefParams {
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Deserialize)]
struct FavoriteParams {
    #[serde(rename = "ref")]
    reference: String,
    on: bool,
}

#[derive(Deserialize)]
struct CompleteParams {
    input: String,
}

impl Plugin {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn account(&self) -> Result<Arc<Account>, RpcError> {
        self.account.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| RpcError::new(rpc::INVALID_REQUEST, "initialize first"))
    }

    fn web(&self, account: Arc<Account>) -> WebApi {
        WebApi::new(self.opts.api_base.as_deref(), account)
    }

    /// The Web API for a signed-in account, or `auth_required`.
    fn api(&self) -> Result<WebApi, RpcError> {
        let account = self.account()?;
        if !account.signed_in() {
            return Err(RpcError::auth_required());
        }
        Ok(self.web(account))
    }

    async fn initialize(self: Arc<Self>, p: Value) -> RpcResult {
        let p: InitializeParams = params(p)?;
        if p.protocol.is_some_and(|v| v != PROTOCOL) {
            tracing::warn!("host speaks protocol {:?}, this plugin v{PROTOCOL}", p.protocol);
        }
        let opts = session::Options { accounts_base: self.opts.accounts_base.clone(), no_session: self.opts.no_session };
        let account = Arc::new(Account::new(p.data_dir.clone(), &opts).map_err(RpcError::from)?);
        let signed_in = account.signed_in();
        *self.account.lock().unwrap_or_else(|e| e.into_inner()) = Some(account);
        let lang = {
            let mut st = self.state();
            st.output = p.output.unwrap_or_default();
            st.data_dir = Some(p.data_dir);
            st.expiry_notified = false;
            if let Some(host) = p.host.filter(|h| !h.name.trim().is_empty()) {
                st.host_name = host.name;
            }
            st.lang = lang_of(p.locale.as_deref());
            items::set_lang(&st.lang);
            st.settings = settings::Settings::from_values(p.settings.as_ref());
            self.report_flag.store(st.settings.report_playback, Ordering::Relaxed);
            tracing::info!("settings: {:?}", st.settings);
            st.lang.clone()
        };
        items::set_user(None);
        tracing::info!("initialized, {}", if signed_in { "signed in" } else { "signed out" });
        if signed_in {
            let me = self.clone();
            tokio::spawn(async move {
                me.load_profile().await;
                me.start_connect();
                me.flush_reports();
            });
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "spotify", "name": "Spotify", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                // Always declared: whether reports go out is a setting.
                "favorites": true, "reporting": true, "remote_control": true, "library": true,
                "lyrics": true, "radio": true, "details": true, "playlist_edit": true
            },
            "settings": settings::declaration(&lang),
        }))
    }

    /// `/me`: the user id (playlist ownership), display name, subscription.
    async fn load_profile(&self) -> Option<Result<Value>> {
        let api = self.api().ok()?;
        let me = api.me().await;
        if let Ok(v) = &me {
            items::set_user(v.get("id").and_then(Value::as_str).map(str::to_string));
            api.account().note_profile(v.get("display_name").and_then(Value::as_str), v.get("product").and_then(Value::as_str));
        }
        Some(me)
    }

    async fn handle(self: Arc<Self>, method: &str, p: Value) -> RpcResult {
        if (method.starts_with("library.") || method == "browse.list" || method == "playlists.create") && items::user_id().is_none() {
            // Ownership of playlists needs the user id.
            self.load_profile().await;
        }
        match method {
            "auth.status" => self.auth_status().await,
            "auth.begin" => self.auth_begin().await,
            "auth.complete" => {
                let p: CompleteParams = params(p)?;
                let code = session::extract_code(&p.input).ok_or_else(|| RpcError::invalid_params("no authorisation code in the input"))?;
                let (verifier, redirect) = {
                    let st = self.state();
                    let login = st.login.as_ref().ok_or_else(|| RpcError::invalid_params("start the sign-in first"))?;
                    (login.pkce.verifier.clone(), login.redirect.clone())
                };
                self.cancel_login();
                self.sign_in(&code, &redirect, &verifier).await
            }
            "auth.sign_out" => {
                self.cancel_login();
                self.stop_connect();
                let account = self.account()?;
                account.sign_out().await.map_err(RpcError::from)?;
                items::set_user(None);
                catalog::forget_probes();
                catalog::forget_cursors();
                self.reports.clear(self.state().data_dir.as_deref());
                Ok(json!({"state": "signed_out"}))
            }
            "browse.root" => catalog::root(&self.api()?).await,
            "browse.list" => catalog::list(&self.api()?, params(p)?).await,
            "search" => catalog::search(&self.api()?, params(p)?).await,
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                catalog::library(&self.api()?, method, params(p)?).await
            }
            "item.get" => {
                let p: RefParams = params(p)?;
                catalog::get(&self.api()?, &p.reference).await
            }
            "favorites.set" => {
                let p: FavoriteParams = params(p)?;
                catalog::set_favorite(&self.api()?, &p.reference, p.on).await
            }
            "playlists.create" | "playlists.rename" | "playlists.delete" | "playlists.add" | "playlists.remove" | "playlists.move" => {
                playlists::edit(&self.api()?, method, params(p)?).await
            }
            "item.details" => {
                let p: RefParams = params(p)?;
                details::get(&self.api()?, &p.reference).await
            }
            "radio.next" => catalog::radio_next(&self.api()?, params(p)?).await,
            "lyrics.get" => {
                let p: RefParams = params(p)?;
                let api = self.api()?;
                match api.account().session().await {
                    Ok(session) => lyrics::get(&session, &p.reference).await,
                    // Without a session, the host goes on with its own sources.
                    Err(e) if !e.chain().any(|c| c.is::<SignedOut>()) => Err(RpcError::not_found(format!("no lyrics service: {e:#}"))),
                    Err(e) => Err(e.into()),
                }
            }
            "track.resolve" => {
                let p: RefParams = params(p)?;
                let api = self.api()?;
                let session = api.account().session().await?;
                let (quality, output) = {
                    let st = self.state();
                    (st.settings.quality, st.output.clone())
                };
                let (result, resolved) = resolve::resolve(&session, &self.relay, &p.reference, quality, &output).await?;
                self.reports.resolved(&p.reference, resolved);
                Ok(result)
            }
            _ => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }

    fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "settings.changed" => {
                let s = settings::Settings::from_values(p.get("settings"));
                tracing::info!("settings changed: {s:?}");
                let (connect_was, data_dir) = {
                    let mut st = self.state();
                    let was = st.settings.connect;
                    st.settings = s.clone();
                    (was, st.data_dir.clone())
                };
                self.report_flag.store(s.report_playback, Ordering::Relaxed);
                if !s.report_playback {
                    self.reports.clear(data_dir.as_deref());
                }
                match (connect_was, s.connect) {
                    (false, true) => self.start_connect(),
                    (true, false) => self.stop_connect(),
                    _ => {}
                }
            }
            "locale.changed" => {
                let lang = lang_of(p.get("locale").and_then(Value::as_str));
                tracing::info!("host language: {lang}");
                items::set_lang(&lang);
                self.state().lang = lang.clone();
                // Settings' labels follow.
                self.out.notify("settings.declared", json!({"settings": settings::declaration(&lang)}));
            }
            "playback.started" | "playback.ended" => self.playback(method, &p),
            "playback.progress" => {}
            "output.changed" => {
                // Accept the output object itself or wrapped in `output`.
                let p = p.get("output").cloned().unwrap_or(p);
                match serde_json::from_value::<Output>(p) {
                    Ok(output) => self.state().output = output,
                    Err(e) => tracing::warn!("output.changed: {e}"),
                }
            }
            "player.state" => match serde_json::from_value::<HostState>(p) {
                Ok(st) => self.to_connect(HostEvent::State(st)),
                Err(e) => tracing::warn!("player.state: {e}"),
            },
            "player.taken_over" => self.to_connect(HostEvent::TakenOver),
            _ => tracing::debug!("ignored notification {method}"),
        }
    }

    /// `playback.started` and `playback.ended`: play reports, when the user
    /// left them on.
    fn playback(self: &Arc<Self>, method: &str, p: &Value) {
        let (enabled, data_dir) = {
            let st = self.state();
            (st.settings.report_playback, st.data_dir.clone())
        };
        let Some(reference) = p.get("ref").and_then(Value::as_str) else { return };
        if !enabled || self.account().map(|a| !a.signed_in()).unwrap_or(true) {
            return;
        }
        if method == "playback.started" {
            self.reports.started(reference);
        } else {
            let listened_ms = p.get("listened_ms").and_then(Value::as_u64).unwrap_or(0);
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("stopped");
            if self.reports.ended(reference, listened_ms, reason, data_dir.as_deref()) {
                self.flush_reports();
            }
        }
    }

    /// Send the queued play reports in the background.
    fn flush_reports(self: &Arc<Self>) {
        if !self.state().settings.report_playback {
            return;
        }
        let Ok(account) = self.account() else { return };
        if !account.has_session_support() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(session) = account.session().await else { return };
            let data_dir = me.state().data_dir.clone();
            me.reports.flush(&session, data_dir.as_deref()).await;
        });
    }

    /// Credentials refused mid-session: tell the host once, without waiting
    /// for its next `auth.status`.
    fn note_auth_failure(&self, method: &str, result: &RpcResult) {
        if !matches!(result, Err(e) if e.code == AUTH_REQUIRED) || method.starts_with("auth.") {
            return;
        }
        if !self.account().is_ok_and(|a| a.signed_in()) {
            return;
        }
        let mut st = self.state();
        if !st.expiry_notified {
            st.expiry_notified = true;
            self.out.notify("auth.changed", json!({"state": "expired"}));
            drop(st);
            self.stop_connect();
        }
    }

    fn account_json(&self, account: &Account) -> Value {
        let stored = account.stored().unwrap_or_default();
        let name = if stored.display_name.is_empty() { "Spotify account".to_string() } else { stored.display_name.clone() };
        let mut v = json!({"display_name": name});
        if let Some(p) = stored.product.as_deref().filter(|p| !p.is_empty()) {
            v["detail"] = product_label(p).into();
        }
        v
    }

    async fn auth_status(&self) -> RpcResult {
        let account = self.account()?;
        if !account.signed_in() {
            return Ok(json!({"state": "signed_out"}));
        }
        match self.load_profile().await {
            Some(Ok(_)) => {}
            Some(Err(e)) if e.chain().any(|c| c.is::<SignedOut>()) || HttpError::status_of(&e) == Some(401) => {
                tracing::info!("stored credentials refused: {e:#}");
                return Ok(json!({"state": "expired", "account": self.account_json(&account)}));
            }
            // Offline or rate limited: still signed in.
            Some(Err(e)) => tracing::info!("profile not read: {e:#}"),
            None => {}
        }
        Ok(json!({"state": "signed_in", "account": self.account_json(&account)}))
    }

    /// Sign-in URL. A loopback listener also catches the redirect when the
    /// browser runs on this machine; otherwise the user pastes the address.
    async fn auth_begin(self: Arc<Self>) -> RpcResult {
        let account = self.account()?;
        self.cancel_login();
        let pkce = Pkce::new();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await;
        let (redirect, handles) = match listener.and_then(|l| Ok((l.local_addr()?.port(), l))) {
            Ok((port, listener)) => {
                let redirect = format!("http://127.0.0.1:{port}/login");
                let (code_tx, mut code_rx) = tokio::sync::mpsc::unbounded_channel();
                let server = session::serve_callback(listener, pkce.state.clone(), code_tx).abort_handle();
                let me = self.clone();
                let server_handle = server.clone();
                let (r, verifier) = (redirect.clone(), pkce.verifier.clone());
                let waiter = tokio::spawn(async move {
                    if let Ok(Some(code)) = tokio::time::timeout(LOGIN_WAIT, code_rx.recv()).await {
                        match me.sign_in(&code, &r, &verifier).await {
                            Ok(status) => me.out.notify("auth.changed", status),
                            Err(e) => tracing::warn!("sign-in through the loopback redirect failed: {}", e.message),
                        }
                    }
                    server_handle.abort();
                });
                (redirect, vec![server, waiter.abort_handle()])
            }
            Err(e) => {
                tracing::warn!("no loopback listener ({e}); paste mode only");
                (FALLBACK_REDIRECT.to_string(), vec![])
            }
        };
        let url = session::authorize_url(account.accounts_base(), &redirect, &pkce);
        self.state().login = Some(Login { pkce, redirect, handles });
        let instructions = "Sign in on the Spotify page (a Premium account is needed). If the browser runs on this machine, \
             sign-in completes by itself. Otherwise the browser ends on a page that does not load: copy its full address \
             (it starts with http://127.0.0.1 and contains \"code=\") and paste it here.";
        Ok(json!({"url": url, "instructions": instructions, "expects_input": true}))
    }

    async fn sign_in(self: &Arc<Self>, code: &str, redirect: &str, verifier: &str) -> RpcResult {
        let account = self.account()?;
        let token = session::exchange_code(&account.http, account.accounts_base(), code, redirect, verifier).await.map_err(|e| {
            if e.chain().any(|c| c.is::<session::Refused>()) { RpcError::invalid_params(format!("{e:#}")) } else { RpcError::from(e) }
        })?;
        self.stop_connect();
        account.sign_in(token, None).await.map_err(RpcError::from)?;
        self.state().expiry_notified = false;
        catalog::forget_probes();
        catalog::forget_cursors();
        items::set_user(None);
        if let Some(Err(e)) = self.load_profile().await {
            tracing::info!("profile not read: {e:#}");
        }
        tracing::info!("signed in");
        self.start_connect();
        Ok(json!({"state": "signed_in", "account": self.account_json(&account)}))
    }

    /// Join Spotify Connect; joined again whenever the session is renewed.
    fn start_connect(self: &Arc<Self>) {
        let Ok(account) = self.account() else { return };
        let (host_name, enabled) = {
            let st = self.state();
            (st.host_name.clone(), st.settings.connect)
        };
        if !enabled || !account.signed_in() || !account.has_session_support() || self.state().connect.is_some() {
            return;
        }
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let me = self.clone();
        let task = tokio::spawn(async move {
            let name = device_name(&host_name, &hostname());
            let mut backoff = Duration::from_secs(5);
            loop {
                let session = match account.session().await {
                    Ok(s) => s,
                    Err(e) if e.chain().any(|c| c.is::<SignedOut>()) => {
                        tracing::info!("Spotify Connect: signed out");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("Spotify Connect: no session ({e:#}); trying again in {}s", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(300));
                        continue;
                    }
                };
                let identity = connect::state::Identity {
                    name: name.clone(),
                    device_id: session.device_id().to_string(),
                    client_id: session.client_id(),
                    session_id: session.session_id(),
                };
                let ctx = connect::Ctx { session: session.clone(), identity, out: me.out.clone(), api: me.web(account.clone()), report: me.report_flag.clone() };
                match connect::run(ctx, &mut rx).await {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::info!("Spotify Connect: {e:#}; joining again in {}s", backoff.as_secs());
                        // A dealer runs once per session: the next run needs a new one.
                        account.drop_session(&session).await;
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(300));
                    }
                }
            }
        });
        let mut st = self.state();
        if st.connect.is_some() {
            task.abort(); // raced with another start
            return;
        }
        st.connect = Some(ConnectHandle { task: task.abort_handle(), events });
    }

    /// Leave Spotify Connect: the device says goodbye, then the task ends.
    fn stop_connect(&self) {
        if let Some(c) = self.state().connect.take() {
            let _ = c.events.send(HostEvent::Stop);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                c.task.abort();
            });
        }
    }

    fn to_connect(&self, ev: HostEvent) {
        if let Some(c) = &self.state().connect {
            let _ = c.events.send(ev);
        }
    }

    fn cancel_login(&self) {
        if let Some(login) = self.state().login.take() {
            for h in login.handles {
                h.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_languages() {
        assert_eq!(lang_of(Some("fr-FR")), "fr");
        assert_eq!(lang_of(Some("de_DE.UTF-8")), "de");
        assert_eq!(lang_of(None), "en");
        assert_eq!(device_name("Player", "box"), "Player (box)");
        assert_eq!(device_name("Player", ""), "Player");
        assert_eq!(product_label("premium"), "Spotify Premium");
    }
}
