//! Protocol `ref`s and conversion of Spotify Web API objects into protocol
//! items.
//!
//! Refs: `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<id>` for
//! catalogue entries (base62 Spotify ids); `library` (a folder holding
//! `liked`, `my/albums`, `my/artists`), `my/playlists`, `made-for-you`,
//! `recent`, `top/tracks`, `top/artists`, `new-releases` for sections;
//! `radio/<track|album|artist>/<id>` and `related/<artist id>` for related
//! content.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Library,
    Liked,
    MyAlbums,
    MyArtists,
    MyPlaylists,
    MadeForYou,
    Recent,
    TopTracks,
    TopArtists,
    NewReleases,
    /// `radio/<seed kind>/<id>`: tracks close to a track, album or artist.
    Radio(RadioSeed, String),
    /// Artists related to an artist.
    Related(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioSeed {
    Track,
    Album,
    Artist,
}

impl RadioSeed {
    pub fn as_str(self) -> &'static str {
        match self {
            RadioSeed::Track => "track",
            RadioSeed::Album => "album",
            RadioSeed::Artist => "artist",
        }
    }
}

/// Spotify ids are 22 base62 characters; a little slack for the future.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Ref {
    pub fn parse(s: &str) -> Option<Self> {
        let r = match s {
            "library" => Ref::Library,
            "liked" => Ref::Liked,
            "my/albums" => Ref::MyAlbums,
            "my/artists" => Ref::MyArtists,
            "my/playlists" => Ref::MyPlaylists,
            "made-for-you" => Ref::MadeForYou,
            "recent" => Ref::Recent,
            "top/tracks" => Ref::TopTracks,
            "top/artists" => Ref::TopArtists,
            "new-releases" => Ref::NewReleases,
            _ => {
                let (kind, id) = s.split_once('/')?;
                if kind == "radio" {
                    let (seed, id) = id.split_once('/')?;
                    let seed = match seed {
                        "track" => RadioSeed::Track,
                        "album" => RadioSeed::Album,
                        "artist" => RadioSeed::Artist,
                        _ => return None,
                    };
                    return valid_id(id).then(|| Ref::Radio(seed, id.to_string()));
                }
                if !valid_id(id) {
                    return None;
                }
                let id = id.to_string();
                match kind {
                    "track" => Ref::Track(id),
                    "album" => Ref::Album(id),
                    "artist" => Ref::Artist(id),
                    "playlist" => Ref::Playlist(id),
                    "related" => Ref::Related(id),
                    _ => return None,
                }
            }
        };
        Some(r)
    }

    /// `spotify:<kind>:<id>` for catalogue entries.
    pub fn uri(&self) -> Option<String> {
        let (kind, id) = match self {
            Ref::Track(id) => ("track", id),
            Ref::Album(id) => ("album", id),
            Ref::Artist(id) => ("artist", id),
            Ref::Playlist(id) => ("playlist", id),
            _ => return None,
        };
        Some(format!("spotify:{kind}:{id}"))
    }

    /// The ref of a `spotify:<kind>:<id>` URI.
    pub fn from_uri(uri: &str) -> Option<Self> {
        let mut parts = uri.split(':');
        if parts.next()? != "spotify" {
            return None;
        }
        let (kind, id) = (parts.next()?, parts.next()?);
        if parts.next().is_some() || !matches!(kind, "track" | "album" | "artist" | "playlist") {
            return None;
        }
        Ref::parse(&format!("{kind}/{id}"))
    }
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Track(id) => write!(f, "track/{id}"),
            Ref::Album(id) => write!(f, "album/{id}"),
            Ref::Artist(id) => write!(f, "artist/{id}"),
            Ref::Playlist(id) => write!(f, "playlist/{id}"),
            Ref::Library => f.write_str("library"),
            Ref::Liked => f.write_str("liked"),
            Ref::MyAlbums => f.write_str("my/albums"),
            Ref::MyArtists => f.write_str("my/artists"),
            Ref::MyPlaylists => f.write_str("my/playlists"),
            Ref::MadeForYou => f.write_str("made-for-you"),
            Ref::Recent => f.write_str("recent"),
            Ref::TopTracks => f.write_str("top/tracks"),
            Ref::TopArtists => f.write_str("top/artists"),
            Ref::NewReleases => f.write_str("new-releases"),
            Ref::Radio(seed, id) => write!(f, "radio/{}/{id}", seed.as_str()),
            Ref::Related(id) => write!(f, "related/{id}"),
        }
    }
}

/// What Spotify streams: Ogg Vorbis at 44.1 kHz.
#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Format {
    pub sample_rate: u32,
    pub codec: &'static str,
}

pub const VORBIS: Format = Format { sample_rate: 44_100, codec: "vorbis" };

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Albums, playlists: number of tracks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    pub playable: bool,
    pub browsable: bool,
    /// Tracks: their album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    /// Tracks and albums: their main artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
    /// In the account's library; absent when not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Tracks of a playlist: their entry in it, `<position>:<track id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Playlists: the account owns it, so it may edit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    /// Related content offered in the item's menu.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// An entry of `Item.actions`: play or open a related ref.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Action {
    pub id: &'static str,
    pub label: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
}

/// How long a favourite state read from the API is trusted: it may change
/// in other apps.
const FAVORITE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_FAVORITES: usize = 20_000;

/// The signed-in account, as far as item conversion needs it. One account
/// per plugin process, hence a global.
#[derive(Default)]
struct Account {
    /// Spotify user id: playlists it owns are editable. Never logged.
    user_id: Option<String>,
    /// `<kind>:<id>` → in the library, and since when that is known.
    favorites: Option<HashMap<String, (bool, Instant)>>,
    /// The host's language, for action labels.
    french: bool,
}

static ACCOUNT: Mutex<Account> = Mutex::new(Account { user_id: None, favorites: None, french: false });

fn account() -> std::sync::MutexGuard<'static, Account> {
    ACCOUNT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Language of the labels the plugin writes itself (`fr`, else English).
pub fn set_lang(lang: &str) {
    account().french = lang == "fr";
}

/// `en` or `fr` text, per the host's language.
pub fn tr(en: &str, fr: &str) -> String {
    if account().french { fr } else { en }.to_string()
}

pub fn user_id() -> Option<String> {
    account().user_id.clone()
}

/// The signed-in account's user id (`None`: signed out). Another account
/// forgets the favourite states.
pub fn set_user(user_id: Option<String>) {
    let mut a = account();
    if a.user_id != user_id {
        a.favorites = None;
    }
    a.user_id = user_id;
}

/// Record whether `id` of `kind` (`track`, `album`, `artist`, `playlist`)
/// is in the library.
pub fn note_favorite(kind: &str, id: &str, on: bool) {
    let mut a = account();
    let favs = a.favorites.get_or_insert_with(HashMap::new);
    if favs.len() >= MAX_FAVORITES {
        favs.retain(|_, (_, at)| at.elapsed() < FAVORITE_TTL);
        if favs.len() >= MAX_FAVORITES {
            favs.clear();
        }
    }
    favs.insert(format!("{kind}:{id}"), (on, Instant::now()));
}

/// Whether `id` of `kind` is in the library; `None` when not known lately.
pub fn is_favorite(kind: &str, id: &str) -> Option<bool> {
    let a = account();
    let (on, at) = a.favorites.as_ref()?.get(&format!("{kind}:{id}"))?;
    (at.elapsed() < FAVORITE_TTL).then_some(*on)
}

fn action(id: &'static str, label: String, r: Ref, kind: &'static str) -> Action {
    Action { id, label, reference: r.to_string(), kind }
}

impl Item {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("item serialises")
    }
}

pub fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn id_of(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).filter(|id| valid_id(id)).map(str::to_string)
}

/// The largest picture up to 640 pixels wide (Spotify lists 640, 300, 64).
pub fn image_of(v: &Value) -> Option<String> {
    let images = v.get("images")?.as_array()?;
    let width = |i: &Value| i.get("width").and_then(Value::as_u64).unwrap_or(0);
    let url = |i: &Value| i.get("url").and_then(Value::as_str).filter(|u| u.starts_with("https://")).map(str::to_string);
    images
        .iter()
        .filter(|i| width(i) <= 640 && url(i).is_some())
        .max_by_key(|i| width(i))
        .or_else(|| images.iter().find(|i| url(i).is_some()))
        .and_then(url)
}

fn first_artist(v: &Value) -> Option<&Value> {
    v.get("artists")?.as_array()?.first()
}

/// "A, B & C" from an `artists` list.
fn artist_names(v: &Value) -> Option<String> {
    let names: Vec<&str> = v
        .get("artists")?
        .as_array()?
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .filter(|n| !n.trim().is_empty())
        .collect();
    match names.len() {
        0 => None,
        1 => Some(names[0].to_string()),
        n => Some(format!("{} & {}", names[..n - 1].join(", "), names[n - 1])),
    }
}

fn year_of(album: &Value) -> Option<u32> {
    str_at(album, &["release_date"])?.get(..4)?.parse().ok()
}

fn join(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<&str> = parts.iter().flatten().copied().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn count(v: Option<&Value>) -> Option<u32> {
    v.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

/// The object inside a list entry: saved tracks and albums wrap it
/// (`{added_at, track}`), playlists too (`{track}`, or `{item}` in newer
/// answers).
pub fn unwrap_entry<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).or_else(|| v.get("item")).filter(|inner| inner.is_object()).unwrap_or(v)
}

/// A track; `album` supplies what tracks listed inside an album omit.
pub fn track(v: &Value, album: Option<&Value>) -> Option<Item> {
    let v = unwrap_entry(v, "track");
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") || v.get("is_local") == Some(&Value::Bool(true)) {
        return None; // podcast episodes, local files
    }
    let id = id_of(v)?;
    let album = v.get("album").filter(|a| a.is_object()).or(album);
    let album_title = album.and_then(|a| str_at(a, &["name"]));
    let album_artist = album.and_then(|a| first_artist(a)).and_then(|a| str_at(a, &["name"]));
    let artist = artist_names(v).or_else(|| album_artist.clone());
    let restricted = v.pointer("/restrictions/reason").is_some();
    Some(Item {
        reference: Ref::Track(id.clone()).to_string(),
        kind: "track",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        subtitle: join(&[artist.as_deref(), album_title.as_deref()]),
        artist,
        album: album_title,
        album_artist,
        track_no: v.get("track_number").and_then(Value::as_u64),
        disc_no: v.get("disc_number").and_then(Value::as_u64),
        year: album.and_then(year_of),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64),
        art: album.and_then(image_of),
        format: Some(VORBIS),
        playable: v.get("is_playable").and_then(Value::as_bool).unwrap_or(true) && !restricted,
        browsable: false,
        album_ref: album.and_then(id_of).map(|a| Ref::Album(a).to_string()),
        artist_ref: first_artist(v).or_else(|| album.and_then(first_artist)).and_then(id_of).map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("track", &id),
        actions: vec![action("radio", tr("Radio from this track", "Radio à partir de ce titre"), Ref::Radio(RadioSeed::Track, id), "play")],
        ..Default::default()
    })
}

pub fn album(v: &Value) -> Option<Item> {
    let v = unwrap_entry(v, "album");
    let id = id_of(v)?;
    let artist = artist_names(v);
    let year = year_of(v);
    let year_s = year.map(|y| y.to_string());
    let artist_id = first_artist(v).and_then(id_of);
    let mut actions = vec![action("radio", tr("Radio from this album", "Radio à partir de cet album"), Ref::Radio(RadioSeed::Album, id.clone()), "play")];
    if let Some(a) = &artist_id {
        actions.push(action("related", tr("Related artists", "Artistes similaires"), Ref::Related(a.clone()), "browse"));
    }
    Some(Item {
        kind: "album",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        genre: v.get("genres").and_then(Value::as_array).and_then(|g| g.first()).and_then(Value::as_str).map(str::to_string),
        track_count: count(v.get("total_tracks")),
        art: image_of(v),
        format: Some(VORBIS),
        playable: v.pointer("/restrictions/reason").is_none(),
        browsable: true,
        artist_ref: artist_id.map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("album", &id),
        actions,
        reference: Ref::Album(id).to_string(),
        ..Default::default()
    })
}

pub fn artist(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    Some(Item {
        kind: "artist",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        genre: v.get("genres").and_then(Value::as_array).and_then(|g| g.first()).and_then(Value::as_str).map(str::to_string),
        art: image_of(v),
        browsable: true,
        favorite: is_favorite("artist", &id),
        actions: vec![
            action("radio", tr("Radio from this artist", "Radio à partir de cet artiste"), Ref::Radio(RadioSeed::Artist, id.clone()), "play"),
            action("related", tr("Related artists", "Artistes similaires"), Ref::Related(id.clone()), "browse"),
        ],
        reference: Ref::Artist(id).to_string(),
        ..Default::default()
    })
}

/// Whether a playlist belongs to the signed-in account; `None` signed out.
pub fn owned(v: &Value) -> Option<bool> {
    let user = user_id()?;
    Some(v.pointer("/owner/id").and_then(Value::as_str) == Some(user.as_str()))
}

pub fn playlist(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    let total = v.pointer("/tracks/total").or_else(|| v.pointer("/items/total"));
    Some(Item {
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        kind: "playlist",
        subtitle: str_at(v, &["owner", "display_name"]),
        track_count: count(total),
        art: image_of(v),
        browsable: true,
        editable: owned(v),
        favorite: is_favorite("playlist", &id),
        reference: Ref::Playlist(id).to_string(),
        ..Default::default()
    })
}

/// A radio, shown as a playlist of its tracks.
pub fn radio(seed: RadioSeed, id: &str, name: Option<&str>, art: Option<String>) -> Item {
    Item {
        reference: Ref::Radio(seed, id.to_string()).to_string(),
        kind: "playlist",
        title: match name {
            Some(name) => format!("{} {name}", tr("Radio:", "Radio :")),
            None => tr("Radio", "Radio"),
        },
        art,
        browsable: true,
        ..Default::default()
    }
}

/// A section: a folder the host lists under the plugin's name.
pub fn folder(r: &Ref, title: &str) -> Item {
    Item { reference: r.to_string(), kind: "folder", title: title.to_string(), browsable: true, ..Default::default() }
}

/// `{items, total, has_more}` from a Spotify paging object (`{items, total,
/// offset, limit, next}`) or a bare list. `map` gets each entry and its
/// position in the whole list.
pub fn page(container: Option<&Value>, offset: u32, limit: u32, map: impl Fn(&Value, u64) -> Option<Item>) -> Value {
    let raw: &[Value] = match container {
        Some(Value::Array(a)) => a,
        Some(c) => c.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]),
        None => &[],
    };
    // A bare list is the whole list; a paging object starts where it says,
    // else where it was asked to.
    let served_from = match container {
        Some(Value::Array(_)) => 0,
        c => c.and_then(|c| c.get("offset")).and_then(Value::as_u64).unwrap_or(offset as u64),
    };
    let skip = (offset as u64).saturating_sub(served_from) as usize;
    let raw = raw.get(skip..).unwrap_or(&[]);
    let items: Vec<Value> = raw
        .iter()
        .take(limit as usize)
        .enumerate()
        .filter_map(|(i, v)| map(v, offset as u64 + i as u64).map(|i| i.to_json()))
        .collect();
    let total = container.and_then(|c| c.get("total")).and_then(Value::as_u64);
    let seen = offset as u64 + raw.len().min(limit as usize) as u64;
    let next = container.and_then(|c| c.get("next")).map(|n| !n.is_null());
    let has_more = match (total, next) {
        (Some(total), _) => seen < total,
        (None, Some(next)) => next || raw.len() > limit as usize,
        (None, None) => raw.len() > limit as usize,
    };
    let mut out = json!({"items": items, "has_more": has_more});
    if let Some(total) = total {
        out["total"] = total.into();
    }
    out
}

/// Entry id of the track at `position` of a playlist.
pub fn entry_id(position: u64, track_id: &str) -> String {
    format!("{position}:{track_id}")
}

/// `(position, track id)` of an entry id.
pub fn parse_entry(entry: &str) -> Option<(u64, String)> {
    let (pos, id) = entry.split_once(':')?;
    let pos = pos.parse().ok()?;
    valid_id(id).then(|| (pos, id.to_string()))
}

/// A playlist entry: the track, with its `entry_id`.
pub fn playlist_track(v: &Value, position: u64) -> Option<Item> {
    let mut item = track(v, None)?;
    let id = item.reference.strip_prefix("track/")?.to_string();
    item.entry_id = Some(entry_id(position, &id));
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_json() -> Value {
        json!({
            "id": "4uLU6hMCjMI75M1A2tKUQC", "name": "Never Gonna Give You Up", "type": "track",
            "duration_ms": 213573, "track_number": 1, "disc_number": 1, "is_playable": true,
            "artists": [{"id": "0gxyHStUsqpMadRV0Di1Qt", "name": "Rick Astley"}],
            "album": {"id": "6N9PS4QXF1D0OWPk0Sxtb4", "name": "Whenever You Need Somebody", "release_date": "1987-11-12",
                      "artists": [{"id": "0gxyHStUsqpMadRV0Di1Qt", "name": "Rick Astley"}],
                      "images": [{"url": "https://i.scdn.co/image/big", "width": 640}, {"url": "https://i.scdn.co/image/small", "width": 64}]}
        })
    }

    #[test]
    fn refs_round_trip_and_reject_junk() {
        for s in [
            "track/4uLU6hMCjMI75M1A2tKUQC", "album/x1", "artist/9", "playlist/37i9dQZF1DXcBWIGoYBM5M", "library", "liked",
            "my/albums", "my/artists", "my/playlists", "made-for-you", "recent", "top/tracks", "top/artists",
            "new-releases", "radio/track/a", "radio/album/b", "radio/artist/c", "related/d",
        ] {
            assert_eq!(Ref::parse(s).unwrap().to_string(), s);
        }
        for s in ["", "track/", "track/a/b", "album/a b", "radio/1", "radio/playlist/1", "radio/track/", "my/stuff", "track/a-b"] {
            assert_eq!(Ref::parse(s), None, "{s}");
        }
        assert_eq!(Ref::from_uri("spotify:track:abc"), Some(Ref::Track("abc".into())));
        assert_eq!(Ref::from_uri("spotify:episode:abc"), None);
        assert_eq!(Ref::from_uri("spotify:user:x:playlist:y"), None);
        assert_eq!(Ref::Playlist("p".into()).uri().as_deref(), Some("spotify:playlist:p"));
    }

    #[test]
    fn tracks_from_recorded_answers() {
        let item = track(&track_json(), None).unwrap();
        assert_eq!(item.reference, "track/4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(item.artist.as_deref(), Some("Rick Astley"));
        assert_eq!(item.subtitle.as_deref(), Some("Rick Astley · Whenever You Need Somebody"));
        assert_eq!(item.year, Some(1987));
        assert_eq!(item.art.as_deref(), Some("https://i.scdn.co/image/big"));
        assert_eq!(item.album_ref.as_deref(), Some("album/6N9PS4QXF1D0OWPk0Sxtb4"));
        assert_eq!(item.artist_ref.as_deref(), Some("artist/0gxyHStUsqpMadRV0Di1Qt"));
        assert_eq!(item.format, Some(VORBIS));
        assert!(item.playable && !item.browsable);
        assert_eq!(item.actions[0].reference, "radio/track/4uLU6hMCjMI75M1A2tKUQC");

        // Saved tracks and playlist entries wrap the track.
        let saved = json!({"added_at": "2026-01-01T00:00:00Z", "track": track_json()});
        assert_eq!(track(&saved, None).unwrap().reference, item.reference);
        let newer = json!({"added_at": "2026-01-01T00:00:00Z", "item": track_json()});
        assert_eq!(playlist_track(&newer, 7).unwrap().entry_id.as_deref(), Some("7:4uLU6hMCjMI75M1A2tKUQC"));

        // Not playable here; episodes and local files are left out.
        let mut t = track_json();
        t["restrictions"] = json!({"reason": "market"});
        assert!(!track(&t, None).unwrap().playable);
        assert!(track(&json!({"id": "e1", "type": "episode", "name": "x"}), None).is_none());
        assert!(track(&json!({"id": null, "is_local": true, "name": "x"}), None).is_none());

        // Inside an album listing: the album passed along fills in.
        let album_json = json!({"id": "al", "name": "A", "artists": [{"id": "ar", "name": "B"}, {"id": "ar2", "name": "C"}], "release_date": "2001"});
        let t = track(&json!({"id": "t1", "name": "T", "artists": [{"id": "ar", "name": "B"}]}), Some(&album_json)).unwrap();
        assert_eq!((t.album_ref.as_deref(), t.album_artist.as_deref()), (Some("album/al"), Some("B")));
        let a = album(&album_json).unwrap();
        assert_eq!(a.artist.as_deref(), Some("B & C"));
        assert_eq!(a.subtitle.as_deref(), Some("B & C · 2001"));
    }

    #[test]
    fn pages_follow_spotify_paging() {
        let container = json!({"items": [{"id": "a", "name": "A"}, {"name": "no id"}], "total": 10, "offset": 0, "next": "https://…"});
        let p = page(Some(&container), 0, 2, |v, _| album(v));
        assert_eq!(p["items"].as_array().unwrap().len(), 1);
        assert_eq!((p["total"].as_u64(), p["has_more"].as_bool()), (Some(10), Some(true)));
        assert_eq!(page(None, 0, 50, |v, _| album(v)), json!({"items": [], "has_more": false}));
        let last = json!({"items": [{"id": "a"}], "next": null});
        assert_eq!(page(Some(&last), 0, 50, |v, _| album(v))["has_more"], false);
        // A bare list, cut at the offset asked.
        let list = json!([{"id": "a"}, {"id": "b"}, {"id": "c"}]);
        let p = page(Some(&list), 1, 1, |v, pos| playlist_track(&json!({"track": {"id": v["id"], "name": "x"}}), pos));
        assert_eq!(p["items"][0]["entry_id"], "1:b");
        assert_eq!(p["has_more"], true);
    }

    #[test]
    fn pictures_prefer_640_pixels() {
        let v = json!({"images": [{"url": "https://x/1000", "width": 1000}, {"url": "https://x/640", "width": 640},
                                  {"url": "https://x/64", "width": 64}]});
        assert_eq!(image_of(&v).as_deref(), Some("https://x/640"));
        assert_eq!(image_of(&json!({"images": [{"url": "https://x/u", "width": null}]})).as_deref(), Some("https://x/u"));
        assert_eq!(image_of(&json!({"images": [{"url": "http://insecure"}]})), None);
        assert_eq!(parse_entry("12:abc"), Some((12, "abc".into())));
        assert_eq!(parse_entry("x:abc"), None);
    }

    /// Account-dependent fields; one test, since the account is global.
    #[test]
    fn favourites_and_ownership_follow_the_account() {
        let mine = json!({"id": "p1", "name": "Soir", "owner": {"id": "me"}, "tracks": {"total": 4}});
        let theirs = json!({"id": "p2", "name": "Top", "owner": {"id": "spotify", "display_name": "Spotify"}});
        set_user(None);
        assert_eq!(playlist(&mine).unwrap().editable, None, "unknown when signed out");
        set_user(Some("me".into()));
        assert_eq!(playlist(&mine).unwrap().editable, Some(true));
        assert_eq!(playlist(&mine).unwrap().track_count, Some(4));
        assert_eq!(playlist(&theirs).unwrap().editable, Some(false));
        assert_eq!(playlist(&theirs).unwrap().subtitle.as_deref(), Some("Spotify"));

        assert_eq!(track(&track_json(), None).unwrap().favorite, None, "not known yet");
        note_favorite("track", "4uLU6hMCjMI75M1A2tKUQC", true);
        assert_eq!(track(&track_json(), None).unwrap().favorite, Some(true));
        note_favorite("track", "4uLU6hMCjMI75M1A2tKUQC", false);
        assert_eq!(track(&track_json(), None).unwrap().favorite, Some(false));
        set_user(Some("other".into()));
        assert_eq!(track(&track_json(), None).unwrap().favorite, None, "another account: states dropped");
        set_user(None);
    }

    #[test]
    fn item_json_skips_missing_fields() {
        let v = folder(&Ref::MyPlaylists, "Playlists").to_json();
        assert_eq!(v, json!({"ref": "my/playlists", "kind": "folder", "title": "Playlists", "playable": false, "browsable": true}));
    }
}
