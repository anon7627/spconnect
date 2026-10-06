//! Connect commands → what to do. Commands arrive from the dealer (a
//! WebSocket) already parsed by librespot-core; this module only decides,
//! so that the decisions can be tested without Spotify.

use librespot_core::dealer::protocol::{Command, PlayCommand, SkipTo as WireSkipTo};
use librespot_protocol::context_track::ContextTrack;
use librespot_protocol::player::PlayOrigin;
use librespot_protocol::transfer_state::TransferState;

use super::queue::SkipTo;

/// Play a context from a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Load {
    /// `spotify:album:…`, `spotify:playlist:…`…; `None` for a bare list of
    /// tracks.
    pub context_uri: Option<String>,
    /// `(uri, uid)` of the tracks the command carries, if any: the context is
    /// resolved when there are none.
    pub tracks: Vec<(String, Option<String>)>,
    pub skip_to: SkipTo,
    pub seek_ms: u32,
    pub paused: bool,
    pub shuffle: Option<bool>,
    pub repeat_context: Option<bool>,
    pub repeat_track: Option<bool>,
    /// Tracks queued by the user (transfers).
    pub queued: Vec<String>,
    pub origin: Option<PlayOrigin>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Load(Box<Load>),
    Pause,
    Resume,
    SeekTo(u32),
    Next,
    /// Skip forward to this track URI.
    SkipTo(String),
    Prev,
    Options { shuffle: Option<bool>, repeat_context: Option<bool>, repeat_track: Option<bool> },
    AddToQueue(String),
    /// Replace the upcoming tracks: `(uri, uid)`.
    SetQueue(Vec<(String, Option<String>)>),
    /// Nothing to do (context updates: the queue is the host's now).
    Ignore,
    Refuse(String),
}

fn tracks_of(pages: impl Iterator<Item = ContextTrack>) -> Vec<(String, Option<String>)> {
    pages
        .filter_map(|t| {
            let uri = t.uri.clone().filter(|u| !u.is_empty())?;
            Some((uri, t.uid.clone().filter(|u| !u.is_empty())))
        })
        .collect()
}

fn skip_to(s: Option<&WireSkipTo>) -> SkipTo {
    match s {
        Some(s) => SkipTo { uid: s.track_uid.clone(), uri: s.track_uri.clone(), index: s.track_index.map(|i| i as usize) },
        None => SkipTo::default(),
    }
}

/// A context URI worth resolving (`-` and empty ones are lists of tracks).
fn context_uri(uri: Option<&str>) -> Option<String> {
    uri.filter(|u| u.starts_with("spotify:") && !u.contains("spotify:internal")).map(str::to_string)
}

fn play(p: &PlayCommand) -> Plan {
    let tracks = tracks_of(p.context.pages.iter().flat_map(|page| page.tracks.iter().cloned()));
    let uri = context_uri(p.context.uri.as_deref());
    if uri.is_none() && tracks.is_empty() {
        return Plan::Refuse("play: no context and no tracks".into());
    }
    let overrides = p.options.player_options_override.as_ref();
    Plan::Load(Box::new(Load {
        context_uri: uri,
        tracks,
        skip_to: skip_to(p.options.skip_to.as_ref()),
        seek_ms: p.options.seek_to.unwrap_or(0),
        paused: p.options.initially_paused.unwrap_or(false),
        shuffle: overrides.and_then(|o| o.shuffling_context),
        repeat_context: overrides.and_then(|o| o.repeating_context),
        repeat_track: overrides.and_then(|o| o.repeating_track),
        queued: Vec::new(),
        origin: Some(p.play_origin.clone()),
    }))
}

/// A transfer: playback moves here from another device, where it was.
pub fn transfer(t: &TransferState, now_ms: i64) -> Plan {
    let session = t.current_session.as_ref();
    let context = session.and_then(|s| s.context.as_ref());
    let uri = context_uri(context.and_then(|c| c.uri.as_deref()));
    let tracks = tracks_of(context.into_iter().flat_map(|c| c.pages.iter()).flat_map(|p| p.tracks.iter().cloned()));
    let playing_queue = t.queue.as_ref().and_then(|q| q.is_playing_queue).unwrap_or(false);
    let current = if playing_queue { t.queue.as_ref().and_then(|q| q.tracks.first()) } else { t.playback.as_ref().and_then(|p| p.current_track.as_ref()) };
    let Some(current) = current.filter(|c| c.uri.as_deref().is_some_and(|u| !u.is_empty())) else {
        return Plan::Refuse("transfer: no current track".into());
    };
    let playback = t.playback.as_ref();
    let paused = playback.and_then(|p| p.is_paused).unwrap_or(false);
    // The position went on while the transfer was on its way.
    let position = playback.and_then(|p| p.position_as_of_timestamp).unwrap_or(0).max(0) as i64;
    let since = playback.and_then(|p| p.timestamp).filter(|&ts| ts > 0 && !paused).map_or(0, |ts| (now_ms - ts).clamp(0, 10_000));
    let options = t.options.as_ref();
    let queued: Vec<String> = t
        .queue
        .as_ref()
        .map(|q| q.tracks.iter().skip(usize::from(playing_queue)).filter_map(|tr| tr.uri.clone()).collect())
        .unwrap_or_default();
    let mut tracks = tracks;
    if uri.is_none() && tracks.is_empty() {
        tracks.push((current.uri.clone().unwrap_or_default(), current.uid.clone()));
    }
    Plan::Load(Box::new(Load {
        context_uri: uri,
        tracks,
        skip_to: SkipTo { uid: session.and_then(|s| s.current_uid.clone()).or_else(|| current.uid.clone()), uri: current.uri.clone(), index: None },
        seek_ms: u32::try_from(position + since).unwrap_or(0),
        paused,
        shuffle: options.and_then(|o| o.shuffling_context),
        repeat_context: options.and_then(|o| o.repeating_context),
        repeat_track: options.and_then(|o| o.repeating_track),
        queued,
        origin: session.and_then(|s| s.play_origin.as_ref()).map(|o| PlayOrigin {
            feature_identifier: o.feature_identifier.clone().unwrap_or_default(),
            feature_version: o.feature_version.clone().unwrap_or_default(),
            view_uri: o.view_uri.clone().unwrap_or_default(),
            referrer_identifier: o.referrer_identifier.clone().unwrap_or_default(),
            device_identifier: o.device_identifier.clone().unwrap_or_default(),
            ..Default::default()
        }),
    }))
}

pub fn plan(cmd: &Command, now_ms: i64) -> Plan {
    match cmd {
        Command::Transfer(t) => match &t.data {
            Some(data) => transfer(data, now_ms),
            None => Plan::Refuse("transfer without data".into()),
        },
        Command::Play(p) => play(p),
        Command::Pause(_) => Plan::Pause,
        Command::Resume(_) => Plan::Resume,
        // The position is in `value`, as librespot notes.
        Command::SeekTo(s) => Plan::SeekTo(s.value),
        Command::SkipNext(s) => match s.track.as_ref().map(|t| t.uri.clone()).filter(|u| !u.is_empty()) {
            Some(uri) => Plan::SkipTo(uri),
            None => Plan::Next,
        },
        Command::SkipPrev(_) => Plan::Prev,
        Command::SetShufflingContext(v) => Plan::Options { shuffle: Some(v.value), repeat_context: None, repeat_track: None },
        Command::SetRepeatingContext(v) => Plan::Options { shuffle: None, repeat_context: Some(v.value), repeat_track: None },
        Command::SetRepeatingTrack(v) => Plan::Options { shuffle: None, repeat_context: None, repeat_track: Some(v.value) },
        Command::SetOptions(o) => Plan::Options { shuffle: o.shuffling_context, repeat_context: o.repeating_context, repeat_track: o.repeating_track },
        Command::AddToQueue(a) if !a.track.uri.is_empty() => Plan::AddToQueue(a.track.uri.clone()),
        Command::AddToQueue(_) => Plan::Refuse("add_to_queue without a track".into()),
        Command::SetQueue(q) => Plan::SetQueue(
            q.next_tracks
                .iter()
                .filter(|t| t.provider != "autoplay" && !t.uri.is_empty())
                .map(|t| (t.uri.clone(), Some(t.uid.clone()).filter(|u| !u.is_empty())))
                .collect(),
        ),
        Command::UpdateContext(_) => Plan::Ignore,
        Command::Unknown(v) => Plan::Refuse(format!("unknown command {}", v.get("endpoint").and_then(|e| e.as_str()).unwrap_or("?"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use librespot_core::dealer::protocol::Request;
    use protobuf::Message;
    use serde_json::{Value, json};

    fn request(command: Value) -> Request {
        serde_json::from_value(json!({"message_id": 7, "sent_by_device_id": "phone", "command": command})).unwrap()
    }

    fn logging() -> Value {
        json!({"interaction_ids": ["i"], "command_id": "c"})
    }

    #[test]
    fn simple_commands() {
        let p = |c: Value| plan(&request(c).command, 0);
        assert_eq!(p(json!({"endpoint": "pause", "logging_params": logging()})), Plan::Pause);
        assert_eq!(p(json!({"endpoint": "resume", "logging_params": logging()})), Plan::Resume);
        assert_eq!(p(json!({"endpoint": "seek_to", "value": 61000, "position": 0, "logging_params": logging()})), Plan::SeekTo(61_000));
        assert_eq!(p(json!({"endpoint": "skip_next", "logging_params": logging()})), Plan::Next);
        assert_eq!(
            p(json!({"endpoint": "skip_next", "track": {"uri": "spotify:track:b", "uid": "ub"}, "logging_params": logging()})),
            Plan::SkipTo("spotify:track:b".into())
        );
        assert_eq!(p(json!({"endpoint": "skip_prev", "logging_params": logging()})), Plan::Prev);
        assert_eq!(
            p(json!({"endpoint": "set_shuffling_context", "value": true, "logging_params": logging()})),
            Plan::Options { shuffle: Some(true), repeat_context: None, repeat_track: None }
        );
        assert_eq!(
            p(json!({"endpoint": "set_options", "repeating_context": true, "repeating_track": false, "logging_params": logging()})),
            Plan::Options { shuffle: None, repeat_context: Some(true), repeat_track: Some(false) }
        );
        assert_eq!(
            p(json!({"endpoint": "add_to_queue", "track": {"uri": "spotify:track:q", "metadata": {"is_queued": "true"}}, "logging_params": logging()})),
            Plan::AddToQueue("spotify:track:q".into())
        );
        assert_eq!(
            p(json!({"endpoint": "set_queue", "queue_revision": "1", "prev_tracks": [], "logging_params": logging(),
                     "next_tracks": [{"uri": "spotify:track:x", "uid": "ux", "provider": "queue"},
                                     {"uri": "spotify:track:y", "provider": "context"},
                                     {"uri": "spotify:track:z", "provider": "autoplay"}]})),
            Plan::SetQueue(vec![("spotify:track:x".into(), Some("ux".into())), ("spotify:track:y".into(), None)])
        );
        assert!(matches!(p(json!({"endpoint": "do_a_flip"})), Plan::Refuse(_)));
    }

    #[test]
    fn play_carries_context_start_and_options() {
        let r = request(json!({
            "endpoint": "play",
            "context": {"uri": "spotify:playlist:p1", "url": "context://spotify:playlist:p1", "metadata": {}},
            "play_origin": {"feature_identifier": "playlist", "feature_version": "x"},
            "options": {"skip_to": {"track_uid": "u3", "track_index": 3}, "seek_to": 1500,
                        "player_options_override": {"shuffling_context": true}, "license": "premium"},
            "logging_params": logging()
        }));
        let Plan::Load(l) = plan(&r.command, 0) else { panic!() };
        assert_eq!(l.context_uri.as_deref(), Some("spotify:playlist:p1"));
        assert!(l.tracks.is_empty(), "to be resolved");
        assert_eq!((l.skip_to.uid.as_deref(), l.skip_to.index), (Some("u3"), Some(3)));
        assert_eq!((l.seek_ms, l.paused, l.shuffle), (1500, false, Some(true)));
        assert_eq!(l.origin.unwrap().feature_identifier, "playlist");

        // A bare list of tracks (Web API "uris").
        let r = request(json!({
            "endpoint": "play",
            "context": {"uri": "-", "pages": [{"tracks": [{"uri": "spotify:track:a"}, {"uri": "spotify:track:b", "uid": "ub"}]}]},
            "play_origin": {}, "options": {"skip_to": {"track_index": 1}, "initially_paused": true}, "logging_params": logging()
        }));
        let Plan::Load(l) = plan(&r.command, 0) else { panic!() };
        assert_eq!(l.context_uri, None);
        assert_eq!(l.tracks, [("spotify:track:a".to_string(), None), ("spotify:track:b".to_string(), Some("ub".to_string()))]);
        assert!(l.paused);
    }

    #[test]
    fn transfers_resume_where_playback_was() {
        let mut t = TransferState::new();
        let s = t.current_session.mut_or_insert_default();
        s.context.mut_or_insert_default().uri = Some("spotify:album:al".into());
        s.current_uid = Some("u2".into());
        let pb = t.playback.mut_or_insert_default();
        pb.timestamp = Some(10_000);
        pb.position_as_of_timestamp = Some(30_000);
        pb.is_paused = Some(false);
        let cur = pb.current_track.mut_or_insert_default();
        cur.uri = Some("spotify:track:b".into());
        let q = t.queue.mut_or_insert_default();
        q.tracks.push(ContextTrack { uri: Some("spotify:track:q".into()), ..Default::default() });
        t.options.mut_or_insert_default().shuffling_context = Some(true);
        let data = base64::engine::general_purpose::STANDARD.encode(t.write_to_bytes().unwrap());
        let r = request(json!({
            "endpoint": "transfer", "data": data,
            "options": {"restore_paused": "restore"}, "from_device_identifier": "phone", "logging_params": logging()
        }));
        let Plan::Load(l) = plan(&r.command, 12_500) else { panic!() };
        assert_eq!(l.context_uri.as_deref(), Some("spotify:album:al"));
        assert_eq!((l.skip_to.uid.as_deref(), l.skip_to.uri.as_deref()), (Some("u2"), Some("spotify:track:b")));
        assert_eq!(l.seek_ms, 32_500, "the position moved on during the transfer");
        assert_eq!(l.queued, ["spotify:track:q"]);
        assert_eq!(l.shuffle, Some(true));

        // Paused: the position stays.
        t.playback.mut_or_insert_default().is_paused = Some(true);
        let Plan::Load(l) = transfer(&t, 99_999) else { panic!() };
        assert_eq!((l.seek_ms, l.paused), (30_000, true));
        assert!(matches!(transfer(&TransferState::new(), 0), Plan::Refuse(_)));
    }
}
