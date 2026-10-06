//! Account check in a child process (`spconnect account-check`).
//!
//! librespot ends the whole process with `exit(1)` when the access point
//! says the account is not Premium. A first connection is therefore made by
//! a child process: its death says "not Premium", and the plugin only opens
//! its own session for accounts the child saw through.
//!
//! The child reads the credentials on stdin (never on its command line) and
//! prints the reusable credentials and the account type on stdout. Its
//! stderr is discarded: librespot logs the account name there.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use librespot_core::authentication::Credentials;
use librespot_core::{Session, SessionConfig};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::session::{PremiumRequired, SignedOut};

/// Hidden subcommand the child runs.
pub const SUBCOMMAND: &str = "account-check";
/// A connection takes a second or two; past this the child is killed.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// librespot's exit status for an account that is not Premium.
const EXIT_NOT_PREMIUM: i32 = 1;
/// The child's own failures.
const EXIT_FAILED: i32 = 2;

/// What the child is given.
#[derive(Serialize, Deserialize)]
struct Request {
    device_id: String,
    username: Option<String>,
    auth_type: i32,
    /// Base64.
    auth_data: String,
}

/// What the child answers.
#[derive(Serialize, Deserialize, Default)]
struct Answer {
    username: String,
    auth_type: i32,
    /// Base64: reusable credentials.
    auth_data: String,
    /// Account `type` attribute: `premium`…
    product: Option<String>,
    /// Set when the connection failed.
    error: Option<String>,
    /// The access point refused the credentials.
    refused: bool,
}

/// An account the access point accepted.
pub struct Checked {
    pub username: String,
    pub auth_type: i32,
    /// Base64.
    pub auth_data: String,
    pub product: Option<String>,
}

/// Connect once in a child process with `creds`. Not Premium →
/// [`PremiumRequired`]; credentials refused → [`SignedOut`].
pub async fn check(device_id: &str, creds: &Credentials) -> Result<Checked> {
    let request = Request {
        device_id: device_id.to_string(),
        username: creds.username.clone(),
        auth_type: creds.auth_type as i32,
        auth_data: STANDARD.encode(&creds.auth_data),
    };
    let exe = std::env::current_exe().context("locating the plugin's binary")?;
    let mut child = tokio::process::Command::new(exe)
        .arg(SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting the account check")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(&serde_json::to_vec(&request)?).await?;
    drop(stdin);
    let mut stdout = child.stdout.take().context("no stdout")?;
    let run = async {
        let mut raw = Vec::new();
        stdout.read_to_end(&mut raw).await?;
        let status = child.wait().await?;
        anyhow::Ok((status, raw))
    };
    let (status, raw) = tokio::time::timeout(CHECK_TIMEOUT, run).await.map_err(|_| anyhow!("the account check timed out"))??;
    if status.code() == Some(EXIT_NOT_PREMIUM) {
        return Err(PremiumRequired.into());
    }
    let answer: Answer = serde_json::from_slice(&raw).with_context(|| format!("account check ended with {status}"))?;
    if let Some(error) = answer.error {
        if answer.refused {
            return Err(anyhow::Error::from(SignedOut).context(format!("Spotify refused the credentials: {error}")));
        }
        bail!("connecting to Spotify: {error}");
    }
    match answer.product.as_deref() {
        Some(p) if crate::session::is_premium(p) => {}
        Some(_) => return Err(PremiumRequired.into()),
        // Opening a session without knowing could end the plugin.
        None => bail!("Spotify did not say which subscription the account has"),
    }
    Ok(Checked { username: answer.username, auth_type: answer.auth_type, auth_data: answer.auth_data, product: answer.product })
}

/// The child: connect, print what the access point said, end.
pub async fn run() -> ! {
    let answer = match connect().await {
        Ok(a) => a,
        Err((error, refused)) => Answer { error: Some(error), refused, ..Default::default() },
    };
    let failed = answer.error.is_some();
    println!("{}", serde_json::to_string(&answer).unwrap_or_default());
    std::process::exit(if failed { EXIT_FAILED } else { 0 })
}

async fn connect() -> Result<Answer, (String, bool)> {
    let failed = |e: String| (e, false);
    let mut raw = Vec::new();
    tokio::io::stdin().read_to_end(&mut raw).await.map_err(|e| failed(e.to_string()))?;
    let request: Request = serde_json::from_slice(&raw).map_err(|e| failed(e.to_string()))?;
    let auth_data = STANDARD.decode(&request.auth_data).map_err(|e| failed(e.to_string()))?;
    let auth_type = protobuf::Enum::from_i32(request.auth_type).ok_or_else(|| failed("unknown credentials type".into()))?;
    let creds = Credentials { username: request.username, auth_type, auth_data };
    let session = Session::new(SessionConfig { device_id: request.device_id, ..SessionConfig::default() }, None);
    if let Err(e) = session.connect(creds, false).await {
        use librespot_core::error::ErrorKind;
        let refused = matches!(e.kind, ErrorKind::PermissionDenied | ErrorKind::Unauthenticated);
        return Err((e.to_string(), refused));
    }
    // The account type comes in a packet right after the login: librespot
    // exits here if it is not Premium.
    let mut product = None;
    for _ in 0..50 {
        product = session.get_user_attribute("type");
        if product.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let answer = Answer {
        username: session.username(),
        auth_type: librespot_protocol::authentication::AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS as i32,
        auth_data: STANDARD.encode(session.auth_data()),
        product,
        ..Default::default()
    };
    session.shutdown();
    Ok(answer)
}
