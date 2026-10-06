//! Spotify Connect (`remote_control` capability): the player shows up as a
//! device in the Spotify apps and plays what they send it.
//!
//! The device's connect-state is kept here, on top of librespot-core's
//! dealer (a WebSocket that pushes cluster updates and commands) and
//! spclient (which stores the state). librespot-connect's own player is
//! not used: the host plays, and keeps its own queue.
//!
//! - Commands (`play`, `transfer`, `pause`, `resume`, `seek_to`,
//!   `skip_next`, `skip_prev`, `set_queue`, `add_to_queue`, options,
//!   volume) become plugin → host `player.*` requests (`commands.rs` decides).
//! - A context to play is resolved (`context-resolve`) into a queue
//!   (`queue.rs`); the host gets a window of it with `player.play`, and
//!   more with `player.enqueue` as it nears the end.
//! - The host's `player.state` notifications move the queue along and are
//!   reported to Spotify as the device's state (`state.rs`);
//!   `player.taken_over` makes the device inactive.
//! - With play reports on, a Spotify track the user starts in the player
//!   itself is shown as playing on this device too, as the desktop app
//!   shows what it plays.

pub mod commands;
pub mod queue;
pub mod state;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures_util::StreamExt;
use librespot_core::dealer::manager::Reply;
use librespot_core::dealer::protocol::{Message, Request};
use librespot_core::{Session, SpotifyUri};
use librespot_protocol::connect::{Cluster, ClusterUpdate, PutStateReason, SetVolumeCommand};
use protobuf::Message as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::plugin::catalog;
use crate::plugin::items::Ref;
use crate::plugin::rpc::Out;
use crate::webapi::WebApi;
use commands::{Load, Plan};
use queue::{Entry, Provider, Queue};
use state::{DeviceState, Identity, Status};

/// Entries of the queue handed to the host before and after the current one.
const WINDOW_BEFORE: usize = 20;
const WINDOW_AFTER: usize = 100;
/// More entries are enqueued when the host is this close to the window's end.
const WINDOW_REFILL: usize = 3;
const WINDOW_STEP: usize = 50;
/// Longest context read.
const MAX_CONTEXT: usize = 1000;
/// A `player.taken_over` this soon after our own `player.play` is the host
/// switching queues for us, not the user taking over.
const TAKEOVER_GRACE: Duration = Duration::from_secs(3);
/// State updates are grouped this long.
const UPDATE_DELAY: Duration = Duration::from_millis(300);
/// A position this far from where it should be is a seek.
const SEEK_JUMP_MS: i64 = 3000;

#[derive(Debug)]
pub enum HostEvent {
    State(HostState),
    TakenOver,
    /// Leave Spotify Connect.
    Stop,
}

/// `player.state` notification.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
pub struct HostState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub item_ref: Option<String>,
    #[serde(default)]
    pub pos_ms: u64,
    #[serde(default)]
    pub dur_ms: u64,
    #[serde(default)]
    pub volume: Option<u32>,
    #[serde(default)]
    pub muted: Option<bool>,
}

impl HostState {
    fn status(&self) -> Status {
        match self.status.as_str() {
            "playing" => Status::Playing,
            "paused" => Status::Paused,
            _ => Status::Stopped,
        }
    }

    fn track_id(&self) -> Option<&str> {
        self.item_ref.as_deref()?.strip_prefix("track/")
    }
}

/// Why the device plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Not the active device.
    Idle,
    /// Playing a queue set from the Spotify apps.
    Ours,
    /// Showing a Spotify track the user started in the player (play reports).
    Mirror,
}

/// Waiting for the host to start the track asked for.
struct Pending {
    id: String,
    seek_ms: Option<u32>,
    pause: bool,
    at: Instant,
}

/// What the task needs from outside.
pub struct Ctx {
    pub session: Session,
    pub identity: Identity,
    pub out: Out,
    pub api: WebApi,
    /// Setting `report_playback`.
    pub report: Arc<AtomicBool>,
}

struct Remote {
    ctx: Ctx,
    state: DeviceState,
    queue: Queue,
    mode: Mode,
    status: Status,
    pos_ms: u64,
    dur_ms: u64,
    /// Server time of `pos_ms`.
    pos_at: i64,
    pending: Option<Pending>,
    requested_at: Option<Instant>,
    repeat_context: bool,
    repeat_track: bool,
    established: bool,
    dirty: bool,
    volume_dirty: bool,
}

fn now_local_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn track_id_of(uri: &str) -> Option<String> {
    match Ref::from_uri(uri)? {
        Ref::Track(id) => Some(id),
        _ => None,
    }
}

/// Run the device until `Stop`, or until the session dies (`Err`).
pub async fn run(ctx: Ctx, events: &mut UnboundedReceiver<HostEvent>) -> Result<()> {
    let dealer = ctx.session.dealer();
    let mut connection_ids = dealer.listen_for("hm://pusher/v1/connections/", |msg: Message| {
        msg.headers.get("Spotify-Connection-Id").cloned().ok_or_else(|| librespot_core::Error::unavailable(anyhow!("no connection id")))
    })?;
    let mut clusters = dealer.listen_for("hm://connect-state/v1/cluster", Message::from_raw::<ClusterUpdate>)?;
    let mut volumes = dealer.listen_for("hm://connect-state/v1/connect/volume", Message::from_raw::<SetVolumeCommand>)?;
    let mut requests = dealer.handle_for("hm://connect-state/v1/player/command")?;
    // Started once per session; `close` below ends it with the task.
    dealer.start().await.context("Spotify Connect: dealer")?;

    let volume = state::volume_from_percent(50);
    let mut r = Remote {
        state: DeviceState::new(&ctx.identity, volume),
        ctx,
        queue: Queue::default(),
        mode: Mode::Idle,
        status: Status::Stopped,
        pos_ms: 0,
        dur_ms: 0,
        pos_at: 0,
        pending: None,
        requested_at: None,
        repeat_context: false,
        repeat_track: false,
        established: false,
        dirty: false,
        volume_dirty: false,
    };
    tracing::info!("Spotify Connect: device \"{}\"", r.ctx.identity.name);
    let result = loop {
        if r.ctx.session.is_invalid() {
            break Err(anyhow!("Spotify session closed"));
        }
        let flush = r.established && (r.dirty || r.volume_dirty);
        tokio::select! {
            id = connection_ids.next() => match id {
                Some(Ok(id)) => {
                    if let Err(e) = r.connection_id(&id).await {
                        tracing::warn!("Spotify Connect: announcing the device failed: {e:#}");
                    }
                }
                Some(Err(e)) => tracing::debug!("connection id: {e}"),
                None => break Err(anyhow!("dealer closed")),
            },
            update = clusters.next() => match update {
                Some(Ok(update)) => r.cluster(update).await,
                Some(Err(e)) => tracing::debug!("cluster update: {e}"),
                None => break Err(anyhow!("dealer closed")),
            },
            volume = volumes.next() => if let Some(Ok(v)) = volume {
                r.remote_volume(v.volume).await;
            },
            request = requests.next() => match request {
                Some((request, reply)) => {
                    let ok = r.request(request).await;
                    let _ = reply.send(if ok { Reply::Success } else { Reply::Failure });
                }
                None => break Err(anyhow!("dealer closed")),
            },
            ev = events.recv() => match ev {
                Some(HostEvent::Stop) | None => break Ok(()),
                Some(HostEvent::State(st)) => r.host_state(st).await,
                Some(HostEvent::TakenOver) => r.taken_over().await,
            },
            _ = tokio::time::sleep(UPDATE_DELAY), if flush => r.flush().await,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    };
    if r.established
        && let Err(e) = r.ctx.session.spclient().delete_connect_state_request().await
    {
        tracing::debug!("Spotify Connect: removing the device: {e}");
    }
    r.ctx.session.dealer().close().await;
    tracing::info!("Spotify Connect: left");
    result
}

impl Remote {
    /// Server time, as the state wants it.
    fn now_ms(&self) -> i64 {
        now_local_ms() + 1000 * self.ctx.session.time_delta()
    }

    /// Position now, from the last one the host reported.
    fn position(&self) -> u64 {
        if self.status == Status::Playing && self.pos_at > 0 {
            let elapsed = (self.now_ms() - self.pos_at).max(0) as u64;
            (self.pos_ms + elapsed).min(if self.dur_ms > 0 { self.dur_ms } else { u64::MAX })
        } else {
            self.pos_ms
        }
    }

    async fn host(&self, method: &str, params: Value) -> bool {
        match self.ctx.out.request(method, params).await {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("{method} refused by the host: {} ({})", e.message, e.code);
                false
            }
        }
    }

    async fn put(&mut self, reason: PutStateReason) -> Result<Vec<u8>> {
        let now = self.now_ms();
        let pos = self.position();
        self.state.set_playback(&self.queue, self.status, pos, self.dur_ms, now);
        self.state.set_options(self.queue.shuffled(), self.repeat_context, self.repeat_track);
        let req = self.state.request(reason, now_local_ms() as u64);
        Ok(self.ctx.session.spclient().put_connect_state_request(&req).await?.to_vec())
    }

    async fn flush(&mut self) {
        let reason = if self.volume_dirty { PutStateReason::VOLUME_CHANGED } else { PutStateReason::PLAYER_STATE_CHANGED };
        self.dirty = false;
        self.volume_dirty = false;
        if let Err(e) = self.put(reason).await {
            tracing::info!("Spotify Connect: state update failed: {e:#}");
        }
    }

    async fn connection_id(&mut self, id: &str) -> Result<()> {
        self.ctx.session.set_connection_id(id);
        let answer = self.put(PutStateReason::NEW_DEVICE).await?;
        let cluster = Cluster::parse_from_bytes(&answer).ok();
        self.established = true;
        tracing::info!("Spotify Connect: device announced");
        if let Some(c) = cluster.filter(|c| !c.active_device_id.is_empty()) {
            tracing::debug!("Spotify Connect: another device is active ({} devices)", c.device.len());
        }
        Ok(())
    }

    async fn cluster(&mut self, update: ClusterUpdate) {
        let Some(cluster) = update.cluster.as_ref() else { return };
        let ours = cluster.active_device_id == self.ctx.identity.device_id;
        if self.state.is_active() && !ours {
            tracing::info!("Spotify Connect: playback moved to another device");
            if self.mode != Mode::Idle && self.status == Status::Playing {
                self.host("player.pause", json!({})).await;
            }
            self.go_idle(false).await;
        }
    }

    /// Stop being the active device.
    async fn go_idle(&mut self, notify: bool) {
        self.mode = Mode::Idle;
        self.pending = None;
        self.status = Status::Stopped;
        self.state.set_active(false, now_local_ms() as u64);
        self.state.clear_player(&self.ctx.session.session_id());
        self.queue = Queue::default();
        self.dirty = false;
        if self.established
            && let Err(e) = self.ctx.session.spclient().put_connect_state_inactive(notify).await
        {
            tracing::debug!("Spotify Connect: going inactive: {e}");
        }
    }

    fn activate(&mut self) {
        self.state.set_active(true, now_local_ms() as u64);
    }

    async fn remote_volume(&mut self, volume: i32) {
        let volume = volume.clamp(0, state::VOLUME_MAX as i32) as u32;
        self.state.set_volume(volume);
        self.volume_dirty = true;
        self.host("player.set_volume", json!({"percent": state::percent_from_volume(volume)})).await;
    }

    /// A command from a Spotify app; `true` if done.
    async fn request(&mut self, request: Request) -> bool {
        self.state.set_last_command(request.message_id, &request.sent_by_device_id);
        let plan = commands::plan(&request.command, self.now_ms());
        tracing::debug!("Spotify Connect: {} → {plan:?}", request.command);
        let ok = match self.execute(plan).await {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!("Spotify Connect: {}: {e:#}", request.command);
                false
            }
        };
        if ok && self.established {
            self.flush().await;
        }
        ok
    }

    async fn execute(&mut self, plan: Plan) -> Result<bool> {
        let active = self.mode != Mode::Idle;
        Ok(match plan {
            Plan::Load(load) => {
                self.load(*load).await?;
                true
            }
            Plan::Pause if active => {
                self.pos_ms = self.position();
                self.status = Status::Paused;
                match &mut self.pending {
                    Some(p) => {
                        p.pause = true;
                        true
                    }
                    None => self.host("player.pause", json!({})).await,
                }
            }
            Plan::Resume if self.mode == Mode::Idle && !self.queue.is_empty() => {
                self.activate();
                self.play_window(self.pos_ms as u32, false).await
            }
            Plan::Resume if active => {
                self.status = Status::Playing;
                self.pos_at = self.now_ms();
                match &mut self.pending {
                    Some(p) => {
                        p.pause = false;
                        true
                    }
                    None => self.host("player.resume", json!({})).await,
                }
            }
            Plan::SeekTo(ms) if active => {
                self.pos_ms = u64::from(ms);
                self.pos_at = self.now_ms();
                match &mut self.pending {
                    Some(p) => {
                        p.seek_ms = Some(ms);
                        true
                    }
                    None => self.host("player.seek", json!({"ms": ms})).await,
                }
            }
            Plan::Next if active => self.host("player.next", json!({})).await,
            Plan::Prev if active => self.host("player.previous", json!({})).await,
            Plan::SkipTo(uri) if self.mode == Mode::Ours => {
                let id = track_id_of(&uri).ok_or_else(|| anyhow!("cannot play {uri}"))?;
                let found = (self.queue.current + 1..self.queue.entries.len()).find(|&i| self.queue.entries[i].id == id);
                match found {
                    Some(i) => {
                        self.queue.current = i;
                        self.play_window(0, self.status == Status::Paused).await
                    }
                    None => self.host("player.next", json!({})).await,
                }
            }
            Plan::SkipTo(_) if active => self.host("player.next", json!({})).await,
            Plan::Options { shuffle, repeat_context, repeat_track } => {
                self.repeat_context = repeat_context.unwrap_or(self.repeat_context);
                self.repeat_track = repeat_track.unwrap_or(self.repeat_track);
                if let Some(on) = shuffle
                    && on != self.queue.shuffled()
                    && self.mode == Mode::Ours
                {
                    self.queue.shuffle(on);
                    self.replay_from_current().await
                } else {
                    true
                }
            }
            Plan::AddToQueue(uri) if self.mode == Mode::Ours => {
                let id = track_id_of(&uri).ok_or_else(|| anyhow!("cannot queue {uri}"))?;
                let items = self.items(std::slice::from_ref(&id)).await;
                if self.host("player.enqueue", json!({"items": items, "at": "next"})).await {
                    self.queue.add_next(Entry::new(&id, None, Provider::Queue));
                    true
                } else {
                    false
                }
            }
            Plan::SetQueue(next) if self.mode == Mode::Ours => {
                let entries = next.iter().filter_map(|(uri, uid)| Some(Entry::new(&track_id_of(uri)?, uid.as_deref(), Provider::Context))).collect();
                self.queue.set_next(entries);
                self.replay_from_current().await
            }
            Plan::Ignore => true,
            Plan::Refuse(why) => {
                tracing::info!("Spotify Connect: {why}");
                false
            }
            other => {
                tracing::info!("Spotify Connect: {other:?} ignored while not playing from Spotify");
                false
            }
        })
    }

    /// Track ids of a context, resolved through the session.
    async fn resolve_context(&self, uri: &str) -> Result<Vec<Entry>> {
        let spclient = self.ctx.session.spclient();
        let ctx = spclient.get_context(uri).await.with_context(|| format!("resolving {uri}"))?;
        let mut entries = Vec::new();
        let mut pages: std::collections::VecDeque<librespot_protocol::context_page::ContextPage> = ctx.pages.into_iter().collect();
        while let Some(page) = pages.pop_front() {
            if entries.len() >= MAX_CONTEXT {
                break;
            }
            let mut tracks = page.tracks.clone();
            let mut next = page.next_page_url.clone().filter(|u| !u.is_empty());
            if tracks.is_empty()
                && let Some(url) = page.page_url.as_deref().filter(|u| !u.is_empty())
            {
                match spclient.get_next_page(url).await {
                    Ok(raw) => {
                        if let Ok(p) = protobuf_json_page(&raw) {
                            next = next.or(p.next_page_url.clone().filter(|u| !u.is_empty()));
                            tracks = p.tracks;
                        }
                    }
                    Err(e) => tracing::debug!("context page: {e}"),
                }
            }
            entries.extend(tracks.iter().filter_map(|t| Some(Entry::new(&track_id_of(t.uri.as_deref()?)?, t.uid.as_deref(), Provider::Context))));
            if pages.is_empty()
                && let Some(url) = next
            {
                pages.push_back(librespot_protocol::context_page::ContextPage { page_url: Some(url), ..Default::default() });
            }
        }
        entries.truncate(MAX_CONTEXT);
        Ok(entries)
    }

    async fn load(&mut self, load: Load) -> Result<()> {
        let mut entries: Vec<Entry> = load
            .tracks
            .iter()
            .filter_map(|(uri, uid)| Some(Entry::new(&track_id_of(uri)?, uid.as_deref(), Provider::Context)))
            .collect();
        let context_uri = load.context_uri.clone().unwrap_or_default();
        if entries.is_empty() && !context_uri.is_empty() {
            entries = self.resolve_context(&context_uri).await?;
        }
        anyhow::ensure!(!entries.is_empty(), "nothing to play in {context_uri}");
        let start = load.skip_to.find(&entries);
        self.queue = Queue::new(&context_uri, entries, start);
        if load.shuffle == Some(true) {
            self.queue.shuffle(true);
        }
        for uri in load.queued.iter().rev() {
            if let Some(id) = track_id_of(uri) {
                self.queue.add_next(Entry::new(&id, None, Provider::Queue));
            }
        }
        self.repeat_context = load.repeat_context.unwrap_or(false);
        self.repeat_track = load.repeat_track.unwrap_or(false);
        self.activate();
        self.state.set_origin(load.origin);
        anyhow::ensure!(self.play_window(load.seek_ms, load.paused).await, "the host refused to play");
        Ok(())
    }

    /// Items for the host, in order; a bare item for a track the catalogue
    /// does not describe.
    async fn items(&self, ids: &[String]) -> Vec<Value> {
        let found = catalog::tracks_by_ids(&self.ctx.api, ids).await;
        let mut found: std::collections::HashMap<String, Value> =
            found.into_iter().map(|i| (i.reference.trim_start_matches("track/").to_string(), i.to_json())).collect();
        ids.iter()
            .map(|id| {
                found.remove(id).unwrap_or_else(|| {
                    json!({"ref": format!("track/{id}"), "kind": "track", "title": "Spotify", "playable": true, "browsable": false})
                })
            })
            .collect()
    }

    /// Hand the host the window around the current entry.
    async fn play_window(&mut self, seek_ms: u32, paused: bool) -> bool {
        let (from, to) = self.queue.take_window(WINDOW_BEFORE, WINDOW_AFTER);
        let ids: Vec<String> = self.queue.entries[from..to].iter().map(|e| e.id.clone()).collect();
        let Some(current) = self.queue.current().map(|e| e.id.clone()) else { return false };
        let items = self.items(&ids).await;
        self.mode = Mode::Ours;
        self.requested_at = Some(Instant::now());
        self.pending = Some(Pending { id: current, seek_ms: (seek_ms > 0).then_some(seek_ms), pause: paused, at: Instant::now() });
        self.status = Status::Loading;
        self.pos_ms = u64::from(seek_ms);
        self.pos_at = self.now_ms();
        self.state.new_playback();
        self.dirty = true;
        let ok = self.host("player.play", json!({"items": items, "start": self.queue.current - from})).await;
        if !ok {
            self.pending = None;
            self.status = Status::Stopped;
        }
        ok
    }

    /// Hand the host the queue again from the current entry, where it was.
    async fn replay_from_current(&mut self) -> bool {
        let pos = self.position();
        self.play_window(pos as u32, self.status == Status::Paused).await
    }

    async fn taken_over(&mut self) {
        if self.mode != Mode::Ours {
            return;
        }
        if self.requested_at.is_some_and(|t| t.elapsed() < TAKEOVER_GRACE) {
            tracing::debug!("Spotify Connect: taken_over right after our own play; ignored");
            return;
        }
        tracing::info!("Spotify Connect: the player plays something else; device inactive");
        self.go_idle(false).await;
    }

    async fn host_state(&mut self, st: HostState) {
        // Volume, wherever it changed.
        if let Some(percent) = st.volume {
            let v = state::volume_from_percent(percent);
            if state::percent_from_volume(self.state.volume()) != percent {
                self.state.set_volume(v);
                self.volume_dirty = self.state.is_active();
            }
        }
        match self.mode {
            Mode::Ours => self.follow_ours(st).await,
            Mode::Mirror | Mode::Idle => self.mirror(st).await,
        }
    }

    fn take_position(&mut self, st: &HostState) {
        let expected = self.position() as i64;
        let status = st.status();
        if status != self.status || (st.pos_ms as i64 - expected).abs() > SEEK_JUMP_MS {
            self.dirty = true;
        }
        self.status = status;
        self.pos_ms = st.pos_ms;
        self.pos_at = self.now_ms();
        if st.dur_ms > 0 {
            self.dur_ms = st.dur_ms;
        }
    }

    async fn follow_ours(&mut self, st: HostState) {
        if let Some(p) = &self.pending {
            if st.track_id() != Some(p.id.as_str()) || st.status() == Status::Stopped {
                if p.at.elapsed() > Duration::from_secs(20) {
                    tracing::info!("Spotify Connect: the player did not start the track");
                    self.pending = None;
                    self.status = Status::Stopped;
                    self.dirty = true;
                }
                return; // still loading
            }
            let (seek, pause) = (p.seek_ms, p.pause);
            self.pending = None;
            self.take_position(&st);
            if let Some(ms) = seek {
                self.pos_ms = u64::from(ms);
                self.host("player.seek", json!({"ms": ms})).await;
            }
            if pause && st.status() == Status::Playing {
                self.status = Status::Paused;
                self.host("player.pause", json!({})).await;
            }
            self.dirty = true;
            return;
        }
        match st.track_id() {
            Some(id) => {
                let moved = self.queue.current().is_none_or(|e| e.id != id);
                if moved {
                    if !self.queue.follow(id) {
                        // A Spotify track queued in the player itself.
                        tracing::debug!("Spotify Connect: the player left the queue");
                        self.mirror_track(id);
                    }
                    self.state.new_playback();
                    self.dirty = true;
                }
                self.take_position(&st);
                self.refill().await;
            }
            None if st.status() == Status::Stopped => {
                let at_end = self.queue.current + 1 >= self.queue.entries.len();
                if at_end && self.repeat_context && !self.queue.is_empty() {
                    self.queue.current = 0;
                    self.play_window(0, false).await;
                } else if self.status != Status::Stopped {
                    self.status = Status::Stopped;
                    self.pos_ms = 0;
                    self.dirty = true;
                }
            }
            // Something else plays: `player.taken_over` follows.
            None => {}
        }
    }

    /// Enqueue more of the queue as the host nears the end of its window.
    async fn refill(&mut self) {
        let (_, to) = self.queue.window;
        if to >= self.queue.entries.len() || self.queue.current + WINDOW_REFILL < to {
            return;
        }
        let (from, new_to) = self.queue.extend_window(WINDOW_STEP);
        let ids: Vec<String> = self.queue.entries[from..new_to].iter().map(|e| e.id.clone()).collect();
        let items = self.items(&ids).await;
        if !self.host("player.enqueue", json!({"items": items, "at": "end"})).await {
            self.queue.window.1 = from;
        }
    }

    fn mirror_track(&mut self, id: &str) {
        let uri = SpotifyUri::Track { id: match librespot_core::SpotifyId::from_base62(id) { Ok(i) => i, Err(_) => return } };
        let context = uri.to_uri().unwrap_or_default();
        self.queue = Queue::new(&context, vec![Entry::new(id, None, Provider::Context)], 0);
        self.queue.take_window(0, 0);
        self.mode = Mode::Mirror;
    }

    /// Play reports: show a Spotify track the player plays on its own.
    async fn mirror(&mut self, st: HostState) {
        let reporting = self.ctx.report.load(Ordering::Relaxed);
        let id = st.track_id().filter(|_| st.status() != Status::Stopped).map(str::to_string);
        match (self.mode, id) {
            (_, Some(id)) if reporting => {
                if self.mode == Mode::Idle || self.queue.current().is_none_or(|e| e.id != id) {
                    self.mirror_track(&id);
                    self.activate();
                    self.state.set_origin(None);
                    self.state.new_playback();
                    self.dirty = true;
                }
                self.take_position(&st);
            }
            (Mode::Mirror, _) => self.go_idle(false).await,
            _ => {}
        }
    }
}

/// A context page fetched by URL (JSON).
fn protobuf_json_page(raw: &[u8]) -> Result<librespot_protocol::context_page::ContextPage> {
    let text = std::str::from_utf8(raw)?;
    // Pages come as `{"pages": [...]}` (a context) or a page alone.
    if let Ok(ctx) = protobuf_json_mapping::parse_from_str::<librespot_protocol::context::Context>(text)
        && let Some(page) = ctx.pages.into_iter().next()
    {
        return Ok(page);
    }
    Ok(protobuf_json_mapping::parse_from_str(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_states_parse() {
        let st: HostState =
            serde_json::from_value(json!({"status": "paused", "item_ref": "track/abc", "pos_ms": 5, "dur_ms": 9, "volume": 40, "muted": false}))
                .unwrap();
        assert_eq!((st.status(), st.track_id(), st.volume), (Status::Paused, Some("abc"), Some(40)));
        let other: HostState = serde_json::from_value(json!({"status": "playing", "item_ref": null, "pos_ms": 0})).unwrap();
        assert_eq!((other.status(), other.track_id()), (Status::Playing, None));
        assert_eq!(track_id_of("spotify:track:x1"), Some("x1".into()));
        assert_eq!(track_id_of("spotify:episode:x1"), None);
    }

    #[test]
    fn context_pages_parse_alone_or_wrapped() {
        let page = protobuf_json_page(br#"{"tracks": [{"uri": "spotify:track:a", "uid": "u"}], "next_page_url": "hm://x"}"#).unwrap();
        assert_eq!(page.tracks[0].uri.as_deref(), Some("spotify:track:a"));
        assert_eq!(page.next_page_url.as_deref(), Some("hm://x"));
        let wrapped = protobuf_json_page(br#"{"pages": [{"tracks": [{"uri": "spotify:track:b"}]}]}"#).unwrap();
        assert_eq!(wrapped.tracks[0].uri.as_deref(), Some("spotify:track:b"));
    }
}
