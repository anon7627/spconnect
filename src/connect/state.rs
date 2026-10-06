//! The device's state as Spotify's connect-state service stores it: a
//! `PutStateRequest` holding the device's description and its player state
//! (track, position, queue, options), sent on each change.

use librespot_protocol::connect::{Capabilities, Device, DeviceInfo, MemberType, PutStateReason, PutStateRequest};
use librespot_protocol::media::AudioQuality;
use librespot_protocol::player::{ContextIndex, ContextPlayerOptions, PlayOrigin, PlayerState, ProvidedTrack, Suppressions};
use protobuf::{EnumOrUnknown, MessageField};

use super::queue::{Entry, Provider, Queue};

/// Spotify's volume scale.
pub const VOLUME_MAX: u32 = 65_535;
const VOLUME_STEPS: i32 = 64;

pub fn volume_from_percent(percent: u32) -> u32 {
    percent.min(100) * VOLUME_MAX / 100
}

pub fn percent_from_volume(volume: u32) -> u32 {
    (volume.min(VOLUME_MAX) * 100 + VOLUME_MAX / 2) / VOLUME_MAX
}

/// What the host is doing with the current track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    /// Asked for, not started yet.
    Loading,
    Stopped,
}

pub struct DeviceState {
    request: PutStateRequest,
    active_since_ms: Option<u64>,
}

/// Who the device says it is.
#[derive(Clone, Debug)]
pub struct Identity {
    pub name: String,
    pub device_id: String,
    pub client_id: String,
    pub session_id: String,
}

impl DeviceState {
    pub fn new(id: &Identity, volume: u32) -> Self {
        let device_info = DeviceInfo {
            can_play: true,
            volume,
            name: id.name.clone(),
            device_id: id.device_id.clone(),
            device_type: EnumOrUnknown::new(librespot_core::config::DeviceType::Computer.into()),
            device_software_version: format!("spconnect {}", env!("CARGO_PKG_VERSION")),
            spirc_version: librespot_core::version::SPOTIFY_SPIRC_VERSION.to_string(),
            client_id: id.client_id.clone(),
            capabilities: MessageField::some(Capabilities {
                can_be_player: true,
                gaia_eq_connect_id: true,
                supports_logout: false,
                is_observable: true,
                volume_steps: VOLUME_STEPS,
                supported_types: vec!["audio/track".into()],
                command_acks: true,
                is_controllable: true,
                supports_transfer_command: true,
                supports_command_request: true,
                supports_gzip_pushes: true,
                needs_full_player_state: true,
                supports_playlist_v2: true,
                supports_set_options_command: true,
                supported_audio_quality: EnumOrUnknown::new(AudioQuality::VERY_HIGH),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = DeviceState {
            request: PutStateRequest {
                member_type: EnumOrUnknown::new(MemberType::CONNECT_STATE),
                put_state_reason: EnumOrUnknown::new(PutStateReason::PLAYER_STATE_CHANGED),
                device: MessageField::some(Device { device_info: MessageField::some(device_info), ..Default::default() }),
                ..Default::default()
            },
            active_since_ms: None,
        };
        state.clear_player(&id.session_id);
        state
    }

    fn device(&mut self) -> &mut Device {
        self.request.device.as_mut().expect("always set")
    }

    fn player(&mut self) -> &mut PlayerState {
        self.device().player_state.mut_or_insert_default()
    }

    #[cfg(test)]
    pub fn player_state(&self) -> &PlayerState {
        &self.request.device.player_state
    }

    pub fn is_active(&self) -> bool {
        self.request.is_active
    }

    pub fn volume(&self) -> u32 {
        self.request.device.device_info.volume
    }

    /// An empty player: nothing playing.
    pub fn clear_player(&mut self, session_id: &str) {
        self.device().player_state = MessageField::some(PlayerState {
            session_id: session_id.to_string(),
            is_system_initiated: true,
            playback_speed: 1.0,
            play_origin: MessageField::some(PlayOrigin::new()),
            suppressions: MessageField::some(Suppressions::new()),
            options: MessageField::some(ContextPlayerOptions::new()),
            ..Default::default()
        });
    }

    pub fn set_active(&mut self, active: bool, now_ms: u64) {
        if active && !self.request.is_active {
            self.active_since_ms = Some(now_ms);
        }
        if !active {
            self.active_since_ms = None;
        }
        self.request.is_active = active;
    }

    pub fn set_volume(&mut self, volume: u32) {
        self.device().device_info.mut_or_insert_default().volume = volume.min(VOLUME_MAX);
    }

    pub fn set_last_command(&mut self, message_id: u32, sent_by: &str) {
        self.request.last_command_message_id = message_id;
        self.request.last_command_sent_by_device_id = sent_by.to_string();
    }

    pub fn set_origin(&mut self, origin: Option<PlayOrigin>) {
        self.player().play_origin = MessageField::some(origin.unwrap_or_default());
    }

    pub fn set_options(&mut self, shuffle: bool, repeat_context: bool, repeat_track: bool) {
        let o = self.player().options.mut_or_insert_default();
        o.shuffling_context = shuffle;
        o.repeating_context = repeat_context;
        o.repeating_track = repeat_track;
    }

    /// A new playback (another track): a new playback id.
    pub fn new_playback(&mut self) {
        self.player().playback_id = uuid::Uuid::new_v4().simple().to_string();
    }

    /// Track, queue, status and position, as of `now_ms` (server time).
    pub fn set_playback(&mut self, queue: &Queue, status: Status, pos_ms: u64, dur_ms: u64, now_ms: i64) {
        let context_uri = queue.context_uri.clone();
        let provided = |e: &Entry| {
            let mut t = ProvidedTrack { uri: e.uri(), uid: e.uid.clone(), provider: e.provider.as_str().to_string(), ..Default::default() };
            if !context_uri.is_empty() {
                t.metadata.insert("context_uri".into(), context_uri.clone());
                t.metadata.insert("entity_uri".into(), context_uri.clone());
            }
            if e.provider == Provider::Queue {
                t.metadata.insert("is_queued".into(), "true".into());
            }
            t
        };
        let track = queue.current().map(provided);
        let prev: Vec<ProvidedTrack> = queue.prev().iter().map(provided).collect();
        let next: Vec<ProvidedTrack> = queue.next().iter().map(provided).collect();
        let revision = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for t in &next {
                t.uid.hash(&mut h);
            }
            h.finish().to_string()
        };
        let current = queue.current;
        let p = self.player();
        p.context_uri = context_uri.clone();
        p.context_url = if context_uri.is_empty() { String::new() } else { format!("context://{context_uri}") };
        p.track = track.into();
        p.index = MessageField::some(ContextIndex { page: 0, track: current as u32, ..Default::default() });
        p.prev_tracks = prev;
        p.next_tracks = next;
        p.queue_revision = revision;
        p.duration = dur_ms as i64;
        p.position_as_of_timestamp = pos_ms as i64;
        p.timestamp = now_ms;
        // Paused and loading count as "playing" for the apps: their play
        // button and preview stay enabled.
        let (playing, paused, buffering) = match status {
            Status::Playing => (true, false, false),
            Status::Paused => (true, true, true),
            Status::Loading => (true, false, true),
            Status::Stopped => (false, true, false),
        };
        p.is_playing = playing;
        p.is_paused = paused;
        p.is_buffering = buffering;
        p.playback_speed = if status == Status::Playing { 1.0 } else { 0.0 };
    }

    /// The request to send for `reason`, stamped `now_ms` (client time).
    pub fn request(&mut self, reason: PutStateReason, now_ms: u64) -> PutStateRequest {
        let mut r = self.request.clone();
        r.put_state_reason = EnumOrUnknown::new(reason);
        r.client_side_timestamp = now_ms;
        if let Some(since) = self.active_since_ms {
            r.started_playing_at = since;
            r.has_been_playing_for_ms = now_ms.saturating_sub(since);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity { name: "Player (box)".into(), device_id: "d1".into(), client_id: "c1".into(), session_id: "s1".into() }
    }

    #[test]
    fn volumes_convert_both_ways() {
        assert_eq!(volume_from_percent(100), VOLUME_MAX);
        assert_eq!(volume_from_percent(150), VOLUME_MAX);
        assert_eq!(volume_from_percent(0), 0);
        for p in [0, 1, 33, 50, 99, 100] {
            assert_eq!(percent_from_volume(volume_from_percent(p)), p);
        }
    }

    #[test]
    fn state_carries_track_queue_and_status() {
        let mut s = DeviceState::new(&identity(), 32_768);
        assert_eq!(s.request.device.device_info.name, "Player (box)");
        assert!(s.request.device.device_info.capabilities.supports_transfer_command);
        let entries = ["a", "b", "c"].iter().map(|id| Entry::new(id, Some(&format!("u{id}")), Provider::Context)).collect();
        let mut q = Queue::new("spotify:album:x", entries, 1);
        q.add_next(Entry::new("q", Some("uq"), Provider::Queue));
        s.set_active(true, 1000);
        s.set_playback(&q, Status::Paused, 42_000, 180_000, 5_000);
        let p = s.player_state();
        assert_eq!(p.track.uri, "spotify:track:b");
        assert_eq!(p.context_uri, "spotify:album:x");
        assert_eq!((p.is_playing, p.is_paused, p.is_buffering), (true, true, true));
        assert_eq!(p.prev_tracks.len(), 1);
        assert_eq!(p.next_tracks[0].provider, "queue");
        assert_eq!(p.next_tracks[0].metadata.get("is_queued").map(String::as_str), Some("true"));
        assert_eq!((p.position_as_of_timestamp, p.duration, p.timestamp), (42_000, 180_000, 5_000));
        let r = s.request(PutStateReason::PLAYER_STATE_CHANGED, 3000);
        assert!(r.is_active);
        assert_eq!((r.started_playing_at, r.has_been_playing_for_ms), (1000, 2000));
        s.set_active(false, 4000);
        assert_eq!(s.request(PutStateReason::PLAYER_STATE_CHANGED, 5000).started_playing_at, 0);
    }
}
