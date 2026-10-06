//! `browse.root`, `browse.list`, `library.*`, `search`, `item.get`,
//! `favorites.set`, `radio.next`.
//!
//! The Web API answers most of it. Where it no longer serves something to
//! this client (Spotify cut editorial playlists, recommendations and
//! related artists off for many apps in late 2024), the session's own
//! endpoints (`spclient`) take over, and a section that neither serves is
//! left out rather than shown failing.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use super::items::{self, Item, RadioSeed, Ref, tr};
use super::rpc::{RpcError, RpcResult};
use crate::webapi::{HttpError, WebApi};

/// Protocol maximum page size.
const MAX_PAGE: u32 = 200;
const DEFAULT_PAGE: u32 = 50;
/// The Web API's largest page.
const API_PAGE: u32 = 50;
/// Metadata lookups run at once.
const LOOKUPS: usize = 8;
/// Radio track lists kept, so that pages of one radio stay consistent.
const RADIO_TTL: Duration = Duration::from_secs(10 * 60);
/// How long the answer of a section probe holds.
const PROBE_TTL: Duration = Duration::from_secs(3600);
/// Owner of Spotify's own playlists (Discover Weekly, Daily Mix…).
const SPOTIFY_OWNER: &str = "spotify";

fn clamp(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

fn parse_ref(s: &str) -> Result<Ref, RpcError> {
    Ref::parse(s).ok_or_else(|| RpcError::not_found(format!("unknown ref {s:?}")))
}

pub fn section_title(r: &Ref) -> String {
    match r {
        Ref::Library => tr("Library", "Bibliothèque"),
        Ref::Liked => tr("Liked songs", "Titres likés"),
        Ref::MyAlbums => tr("Albums", "Albums"),
        Ref::MyArtists => tr("Artists", "Artistes"),
        Ref::MyPlaylists => tr("My playlists", "Mes playlists"),
        Ref::MadeForYou => tr("Made for you", "Conçu pour vous"),
        Ref::Recent => tr("Recently played", "Écoutés récemment"),
        Ref::TopTracks => tr("Your top tracks", "Vos titres préférés"),
        Ref::TopArtists => tr("Your top artists", "Vos artistes préférés"),
        Ref::NewReleases => tr("New releases", "Nouveautés"),
        _ => "?".into(),
    }
}

fn folder(r: Ref) -> Value {
    items::folder(&r, &section_title(&r)).to_json()
}

/// Sections whose availability is checked once in a while.
static PROBES: Mutex<Option<HashMap<&'static str, (bool, Instant)>>> = Mutex::new(None);

pub fn forget_probes() {
    *PROBES.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

async fn probe(name: &'static str, check: impl std::future::Future<Output = Result<bool>>) -> bool {
    if let Some((ok, at)) = PROBES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(name)
        && at.elapsed() < PROBE_TTL
    {
        return *ok;
    }
    let ok = match tokio::time::timeout(Duration::from_secs(4), check).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) if HttpError::is_withheld(&e) => {
            tracing::info!("{name}: not served to this client, left out");
            false
        }
        // Unknown: offered, and asked again next time.
        Ok(Err(e)) => {
            tracing::debug!("{name}: {e:#}");
            return true;
        }
        Err(_) => return true,
    };
    PROBES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(name, (ok, Instant::now()));
    ok
}

/// `sections` for hosts that show the plugin in their sidebar; `home`, the
/// shelves, for hosts that merge the `library` lists instead.
pub async fn root(api: &WebApi) -> RpcResult {
    let (made_for_you, new_releases) = tokio::join!(
        probe("made-for-you", async { Ok(!made_for_you(api).await?.is_empty()) }),
        probe("new-releases", async {
            api.get("/browse/new-releases", &crate::webapi::page(0, 1)).await?;
            Ok(true)
        }),
    );
    let mut sections = vec![Ref::Library, Ref::MyPlaylists];
    let mut home = vec![Ref::Recent];
    if made_for_you {
        sections.push(Ref::MadeForYou);
        home.push(Ref::MadeForYou);
    }
    sections.extend([Ref::Recent, Ref::TopTracks, Ref::TopArtists]);
    home.extend([Ref::TopTracks, Ref::TopArtists]);
    if new_releases {
        sections.push(Ref::NewReleases);
        home.push(Ref::NewReleases);
    }
    let sections: Vec<Value> = sections.into_iter().map(folder).collect();
    let home: Vec<Value> = home.into_iter().map(folder).collect();
    Ok(json!({"sections": sections, "home": home}))
}

#[derive(Deserialize)]
pub struct PageParams {
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// One protocol page of a Web API list, in as many calls as it takes
/// (the API serves 50 at most). `key`: the paging object's key in the
/// answer (`albums` of `/browse/new-releases`), or `None` when the answer
/// is the paging object.
pub async fn paged(api: &WebApi, path: &str, query: &[(&str, String)], key: Option<&str>, offset: u32, limit: u32) -> Result<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut total = None;
    let mut next = false;
    let mut pos = offset;
    while (items.len() as u32) < limit {
        let chunk = (limit - items.len() as u32).min(API_PAGE);
        let mut q = query.to_vec();
        q.extend(crate::webapi::page(pos, chunk));
        let v = api.get(path, &q).await?;
        let c = match key {
            Some(k) => v.get(k).cloned().unwrap_or(Value::Null),
            None => v,
        };
        let got = c.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        total = c.get("total").and_then(Value::as_u64).or(total);
        next = c.get("next").is_some_and(|n| !n.is_null());
        let n = got.len() as u32;
        items.extend(got);
        pos += n;
        if n == 0 || (total.is_none() && !next) || total.is_some_and(|t| u64::from(pos) >= t) {
            break;
        }
    }
    let mut out = json!({"items": items, "offset": offset, "next": if next { json!("more") } else { Value::Null }});
    if let Some(t) = total {
        out["total"] = t.into();
    }
    Ok(out)
}

/// Followed artists come by cursor: the `after` cursor of every 50th
/// artist is kept, so that pages asked in order cost one call each.
static ARTIST_CURSORS: Mutex<Vec<String>> = Mutex::new(Vec::new());

async fn followed_artists(api: &WebApi, offset: u32, limit: u32) -> Result<Value> {
    // `cursors[i]` is the `after` that gives page `i + 1` (pages of 50).
    let (mut page_no, mut after) = {
        let cursors = ARTIST_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
        let start = ((offset / API_PAGE) as usize).min(cursors.len());
        (start, start.checked_sub(1).map(|i| cursors[i].clone()))
    };
    let mut skip = offset as usize - page_no * API_PAGE as usize;
    let mut out: Vec<Value> = Vec::new();
    let mut total = None;
    loop {
        let mut q = vec![("type", "artist".to_string()), ("limit", API_PAGE.to_string())];
        if let Some(a) = &after {
            q.push(("after", a.clone()));
        }
        let v = api.get("/me/following", &q).await?;
        let c = v.get("artists").cloned().unwrap_or(Value::Null);
        total = c.get("total").and_then(Value::as_u64).or(total);
        let got = c.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let cursor = c.pointer("/cursors/after").and_then(Value::as_str).map(str::to_string);
        if let Some(cur) = &cursor {
            let mut cursors = ARTIST_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
            if cursors.len() == page_no {
                cursors.push(cur.clone());
            }
        }
        let n = got.len();
        out.extend(got.into_iter().skip(skip));
        skip = skip.saturating_sub(n);
        page_no += 1;
        if out.len() >= limit as usize || n < API_PAGE as usize || cursor.is_none() {
            break;
        }
        after = cursor;
    }
    out.truncate(limit as usize);
    let mut c = json!({"items": out, "offset": offset});
    if let Some(t) = total {
        c["total"] = t.into();
    }
    Ok(c)
}

pub fn forget_cursors() {
    ARTIST_CURSORS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// `library.albums`, `.artists`, `.tracks`, `.playlists`: the account's
/// saved albums and tracks, followed artists, and playlists.
pub async fn library(api: &WebApi, method: &str, p: PageParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let r = match method {
        "library.albums" => Ref::MyAlbums,
        "library.artists" => Ref::MyArtists,
        "library.tracks" => Ref::Liked,
        "library.playlists" => Ref::MyPlaylists,
        _ => return Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    };
    list_ref(api, r, offset, limit).await
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub async fn list(api: &WebApi, p: ListParams) -> RpcResult {
    let r = parse_ref(&p.reference)?;
    let mut page = list_ref(api, r, p.offset, clamp(p.limit)).await?;
    annotate_favorites(api, &mut page).await;
    Ok(page)
}

/// Mark the items of a saved list as favourites without asking.
fn all_favorite(page: &mut Value, kind: &str) {
    for item in page.get_mut("items").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(id) = item["ref"].as_str().and_then(|r| r.strip_prefix(&format!("{kind}/"))).map(str::to_string) {
            items::note_favorite(kind, &id, true);
            item["favorite"] = true.into();
        }
    }
}

async fn list_ref(api: &WebApi, r: Ref, offset: u32, limit: u32) -> RpcResult {
    let page = match r {
        Ref::Library => {
            let items: Vec<Value> = [Ref::Liked, Ref::MyAlbums, Ref::MyArtists].into_iter().map(folder).collect();
            return Ok(json!({"items": items, "total": 3, "has_more": false}));
        }
        Ref::Liked => {
            let v = paged(api, "/me/tracks", &[], None, offset, limit).await?;
            let mut page = items::page(Some(&v), offset, limit, |t, _| items::track(t, None));
            all_favorite(&mut page, "track");
            page
        }
        Ref::MyAlbums => {
            let v = paged(api, "/me/albums", &[], None, offset, limit).await?;
            let mut page = items::page(Some(&v), offset, limit, |a, _| items::album(a));
            all_favorite(&mut page, "album");
            page
        }
        Ref::MyArtists => {
            let v = followed_artists(api, offset, limit).await?;
            let mut page = items::page(Some(&v), offset, limit, |a, _| items::artist(a));
            all_favorite(&mut page, "artist");
            page
        }
        Ref::MyPlaylists => {
            let v = paged(api, "/me/playlists", &[], None, offset, limit).await?;
            items::page(Some(&v), offset, limit, |p, _| items::playlist(p))
        }
        Ref::MadeForYou => {
            let all = made_for_you(api).await?;
            let container = json!({"items": all, "offset": 0, "total": all.len()});
            items::page(Some(&container), offset, limit, |p, _| items::playlist(p))
        }
        Ref::Recent => {
            // At most the last 50 plays, by cursor: one page, each track once.
            let v = api.get("/me/player/recently-played", &[("limit", API_PAGE.to_string())]).await?;
            let mut seen = HashSet::new();
            let tracks: Vec<Value> = v
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| e.get("track"))
                .filter(|t| t.get("id").and_then(Value::as_str).is_some_and(|id| seen.insert(id.to_string())))
                .cloned()
                .collect();
            let container = json!({"items": tracks, "offset": 0, "total": tracks.len()});
            items::page(Some(&container), offset, limit, |t, _| items::track(t, None))
        }
        Ref::TopTracks => {
            let v = paged(api, "/me/top/tracks", &[], None, offset, limit).await?;
            items::page(Some(&v), offset, limit, |t, _| items::track(t, None))
        }
        Ref::TopArtists => {
            let v = paged(api, "/me/top/artists", &[], None, offset, limit).await?;
            items::page(Some(&v), offset, limit, |a, _| items::artist(a))
        }
        Ref::NewReleases => {
            let v = paged(api, "/browse/new-releases", &[], Some("albums"), offset, limit).await?;
            items::page(Some(&v), offset, limit, |a, _| items::album(a))
        }
        Ref::Album(id) => {
            let album = api.get(&format!("/albums/{id}"), &[]).await?;
            let embedded = album.get("tracks").filter(|_| offset == 0 && limit <= API_PAGE).cloned();
            let tracks = match embedded {
                Some(t) => t,
                None => paged(api, &format!("/albums/{id}/tracks"), &[], None, offset, limit).await?,
            };
            items::page(Some(&tracks), offset, limit, |t, _| items::track(t, Some(&album)))
        }
        Ref::Artist(id) => {
            let q = [("include_groups", "album,single,compilation".to_string())];
            let v = paged(api, &format!("/artists/{id}/albums"), &q, None, offset, limit).await?;
            items::page(Some(&v), offset, limit, |a, _| items::album(a))
        }
        Ref::Playlist(id) => playlist_tracks(api, &id, offset, limit).await?,
        Ref::Radio(seed, id) => {
            let tracks = radio_items(api, seed, &id).await?;
            let all: Vec<Value> = tracks.iter().map(Item::to_json).collect();
            let total = all.len();
            let items: Vec<Value> = all.into_iter().skip(offset as usize).take(limit as usize).collect();
            json!({"items": items, "total": total, "has_more": (offset as usize + items.len()) < total})
        }
        Ref::Related(id) => {
            let all = related_artists(api, &id).await?;
            let total = all.len();
            let items: Vec<Value> = all.iter().skip(offset as usize).take(limit as usize).map(Item::to_json).collect();
            json!({"items": items, "total": total, "has_more": (offset as usize + items.len()) < total})
        }
        Ref::Track(_) => return Err(RpcError::invalid_params("a track is not browsable")),
    };
    Ok(page)
}

/// The account's playlists made by Spotify (Discover Weekly, Release
/// Radar, Daily Mix…), among those it follows.
async fn made_for_you(api: &WebApi) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let v = paged(api, "/me/playlists", &[], None, offset, MAX_PAGE).await?;
        let got = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let n = got.len() as u32;
        all.extend(got.into_iter().filter(|p| p.pointer("/owner/id").and_then(Value::as_str) == Some(SPOTIFY_OWNER)));
        offset += n;
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        if n == 0 || u64::from(offset) >= total || offset >= 2000 {
            break;
        }
    }
    Ok(all)
}

/// A playlist's tracks: Web API (`/tracks`, `/items` in newer versions),
/// else the session's playlist service for playlists the Web API keeps
/// from this client (Spotify's editorial ones).
async fn playlist_tracks(api: &WebApi, id: &str, offset: u32, limit: u32) -> Result<Value, RpcError> {
    let mut last = None;
    for path in [format!("/playlists/{id}/tracks"), format!("/playlists/{id}/items")] {
        match paged(api, &path, &[], None, offset, limit).await {
            Ok(v) => return Ok(items::page(Some(&v), offset, limit, items::playlist_track)),
            Err(e) if HttpError::is_withheld(&e) => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    let Ok(session) = api.account().session().await else {
        return Err(last.map(RpcError::from).unwrap_or_else(|| RpcError::not_found("playlist not served")));
    };
    let ids = session_playlist(&session, id).await?;
    let total = ids.len();
    let window: Vec<String> = ids.into_iter().skip(offset as usize).take(limit as usize).collect();
    let tracks = tracks_by_ids(api, &window).await;
    let items: Vec<Value> = tracks
        .into_iter()
        .zip(offset as u64..)
        .map(|(mut t, pos)| {
            if let Some(tid) = t.reference.strip_prefix("track/").map(str::to_string) {
                t.entry_id = Some(items::entry_id(pos, &tid));
            }
            t.to_json()
        })
        .collect();
    Ok(json!({"items": items, "total": total, "has_more": (offset as usize + window_len(limit, offset, total)) < total}))
}

fn window_len(limit: u32, offset: u32, total: usize) -> usize {
    total.saturating_sub(offset as usize).min(limit as usize)
}

/// Track ids of a playlist, from the session's playlist service.
async fn session_playlist(session: &Session, id: &str) -> Result<Vec<String>> {
    let uri = SpotifyUri::Playlist { user: None, id: SpotifyId::from_base62(id)? };
    let playlist = <librespot_metadata::Playlist as librespot_metadata::Metadata>::get(session, &uri).await?;
    Ok(playlist
        .contents
        .items
        .iter()
        .filter_map(|i| match &i.id {
            SpotifyUri::Track { id } => id.to_base62().ok(),
            _ => None,
        })
        .collect())
}

/// Items for track ids, in their order: the Web API by fifties, else the
/// session's metadata, else one Web API call per track.
pub async fn tracks_by_ids(api: &WebApi, ids: &[String]) -> Vec<Item> {
    let mut found: HashMap<String, Item> = HashMap::new();
    let mut batch_ok = true;
    for chunk in ids.chunks(API_PAGE as usize) {
        match api.get("/tracks", &[("ids", chunk.join(","))]).await {
            Ok(v) => {
                for t in v.get("tracks").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(item) = items::track(t, None) {
                        found.insert(item.reference.trim_start_matches("track/").to_string(), item);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("batched tracks: {e:#}");
                batch_ok = false;
                break;
            }
        }
    }
    if !batch_ok {
        let session = api.account().current_session().await;
        let missing: Vec<String> = ids.iter().filter(|id| !found.contains_key(*id)).cloned().collect();
        let fetched: Vec<Item> = stream::iter(missing)
            .map(|id| {
                let session = session.clone();
                async move {
                    match &session {
                        Some(s) => metadata_track(s, &id).await,
                        None => api.get(&format!("/tracks/{id}"), &[]).await.ok().and_then(|t| items::track(&t, None)),
                    }
                }
            })
            .buffered(LOOKUPS)
            .filter_map(|t| async move { t })
            .collect()
            .await;
        for item in fetched {
            found.insert(item.reference.trim_start_matches("track/").to_string(), item);
        }
    }
    ids.iter().filter_map(|id| found.remove(id)).collect()
}

/// `https://i.scdn.co/image/<file id>`.
fn cover_url(images: &librespot_metadata::image::Images) -> Option<String> {
    use librespot_metadata::image::ImageSize;
    let pick = images.iter().find(|i| i.size == ImageSize::DEFAULT).or_else(|| images.iter().max_by_key(|i| i.width));
    pick.and_then(|i| i.id.to_base16().ok()).map(|hex| format!("https://i.scdn.co/image/{hex}"))
}

/// A track item from the session's metadata service.
pub async fn metadata_track(session: &Session, id: &str) -> Option<Item> {
    let uri = SpotifyUri::Track { id: SpotifyId::from_base62(id).ok()? };
    let t = <librespot_metadata::Track as librespot_metadata::Metadata>::get(session, &uri).await.ok()?;
    let artist_names: Vec<&str> = t.artists.iter().map(|a| a.name.as_str()).collect();
    let artist = (!artist_names.is_empty()).then(|| artist_names.join(", "));
    let album_id = t.album.id.to_id().ok();
    let artist_id = t.artists.first().and_then(|a| a.id.to_id().ok());
    let year = t.album.date.as_utc().year();
    let album_title = Some(t.album.name.clone()).filter(|n| !n.is_empty());
    Some(Item {
        reference: Ref::Track(id.to_string()).to_string(),
        kind: "track",
        title: t.name.clone(),
        subtitle: Some([artist.as_deref(), album_title.as_deref()].iter().flatten().copied().collect::<Vec<_>>().join(" · "))
            .filter(|s| !s.is_empty()),
        artist,
        album: album_title,
        album_artist: t.album.artists.first().map(|a| a.name.clone()),
        track_no: u64::try_from(t.number).ok(),
        disc_no: u64::try_from(t.disc_number).ok(),
        year: u32::try_from(year).ok().filter(|y| *y > 1900),
        duration_ms: u64::try_from(t.duration).ok(),
        art: cover_url(&t.album.covers),
        format: Some(items::VORBIS),
        playable: true,
        album_ref: album_id.map(|a| Ref::Album(a).to_string()),
        artist_ref: artist_id.map(|a| Ref::Artist(a).to_string()),
        favorite: items::is_favorite("track", id),
        actions: vec![items::Action {
            id: "radio",
            label: tr("Radio from this track", "Radio à partir de ce titre"),
            reference: Ref::Radio(RadioSeed::Track, id.to_string()).to_string(),
            kind: "play",
        }],
        ..Default::default()
    })
}

/// Radio track lists by seed, and when they were read.
type Radios = HashMap<String, (Vec<Item>, Instant)>;

static RADIOS: Mutex<Option<Radios>> = Mutex::new(None);

/// Tracks of a radio: from the session (the track's "inspired by" mix, the
/// autoplay continuation of an album or artist), else Web API
/// recommendations. Empty when neither answers.
async fn radio_items(api: &WebApi, seed: RadioSeed, id: &str) -> Result<Vec<Item>, RpcError> {
    let key = format!("{}/{id}", seed.as_str());
    if let Some((tracks, at)) = RADIOS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(&key)
        && at.elapsed() < RADIO_TTL
    {
        return Ok(tracks.clone());
    }
    let mut ids: Vec<String> = Vec::new();
    if api.account().has_session_support() {
        match api.account().session().await {
            Ok(session) => match session_radio(&session, seed, id).await {
                Ok(found) => ids = found,
                Err(e) => tracing::info!("radio from the session: {e:#}"),
            },
            Err(e) => tracing::info!("radio: no session ({e:#})"),
        }
    }
    let tracks = if ids.is_empty() {
        recommendations(api, seed, id).await.unwrap_or_else(|e| {
            tracing::info!("recommendations: {e:#}");
            Vec::new()
        })
    } else {
        ids.retain(|t| t != id);
        let mut seen = HashSet::new();
        ids.retain(|t| seen.insert(t.clone()));
        ids.truncate(100);
        tracks_by_ids(api, &ids).await
    };
    let mut radios = RADIOS.lock().unwrap_or_else(|e| e.into_inner());
    let radios = radios.get_or_insert_with(HashMap::new);
    radios.retain(|_, (_, at)| at.elapsed() < RADIO_TTL);
    radios.insert(key, (tracks.clone(), Instant::now()));
    Ok(tracks)
}

async fn session_radio(session: &Session, seed: RadioSeed, id: &str) -> Result<Vec<String>> {
    let sid = SpotifyId::from_base62(id)?;
    match seed {
        RadioSeed::Track => {
            let v = session.spclient().get_radio_for_track(&SpotifyUri::Track { id: sid }).await?;
            let v: Value = serde_json::from_slice(&v)?;
            let uri = v.pointer("/mediaItems/0/uri").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no radio playlist"))?;
            let Some(Ref::Playlist(pid)) = Ref::from_uri(uri) else { anyhow::bail!("unexpected radio {uri}") };
            session_playlist(session, &pid).await
        }
        RadioSeed::Album | RadioSeed::Artist => {
            let uri = match seed {
                RadioSeed::Album => SpotifyUri::Album { id: sid },
                _ => SpotifyUri::Artist { id: sid },
            };
            let request = librespot_protocol::autoplay_context_request::AutoplayContextRequest {
                context_uri: Some(uri.to_uri()?),
                ..Default::default()
            };
            let ctx = session.spclient().get_autoplay_context(&request).await?;
            Ok(context_track_ids(&ctx))
        }
    }
}

/// Track ids of a resolved context, in order.
pub fn context_track_ids(ctx: &librespot_protocol::context::Context) -> Vec<String> {
    ctx.pages
        .iter()
        .flat_map(|p| p.tracks.iter())
        .filter_map(|t| t.uri.as_deref().and_then(Ref::from_uri))
        .filter_map(|r| match r {
            Ref::Track(id) => Some(id),
            _ => None,
        })
        .collect()
}

async fn recommendations(api: &WebApi, seed: RadioSeed, id: &str) -> Result<Vec<Item>> {
    let (key, seed_id) = match seed {
        RadioSeed::Track => ("seed_tracks", id.to_string()),
        RadioSeed::Artist => ("seed_artists", id.to_string()),
        RadioSeed::Album => {
            let album = api.get(&format!("/albums/{id}"), &[]).await?;
            match album.pointer("/artists/0/id").and_then(Value::as_str) {
                Some(a) => ("seed_artists", a.to_string()),
                None => return Ok(Vec::new()),
            }
        }
    };
    let v = api.get("/recommendations", &[(key, seed_id), ("limit", "50".into())]).await?;
    Ok(v.get("tracks").and_then(Value::as_array).into_iter().flatten().filter_map(|t| items::track(t, None)).collect())
}

/// Artists related to an artist: Web API, else the session's metadata.
async fn related_artists(api: &WebApi, id: &str) -> Result<Vec<Item>, RpcError> {
    match api.get(&format!("/artists/{id}/related-artists"), &[]).await {
        Ok(v) => return Ok(v.get("artists").and_then(Value::as_array).into_iter().flatten().filter_map(items::artist).collect()),
        Err(e) if !HttpError::is_withheld(&e) => return Err(e.into()),
        Err(_) => {}
    }
    let Ok(session) = api.account().session().await else { return Ok(Vec::new()) };
    let uri = SpotifyUri::Artist { id: SpotifyId::from_base62(id).map_err(anyhow::Error::from)? };
    let artist = <librespot_metadata::Artist as librespot_metadata::Metadata>::get(&session, &uri).await.map_err(anyhow::Error::from)?;
    Ok(artist
        .related
        .iter()
        .filter_map(|a| {
            let id = a.id.to_id().ok()?;
            items::artist(&json!({"id": id, "name": a.name}))
        })
        .collect())
}

#[derive(Deserialize)]
pub struct RadioNextParams {
    seed: String,
    #[serde(default)]
    exclude: Vec<String>,
    limit: Option<u32>,
}

/// `radio.next {seed, exclude, limit}`: playable tracks close to the seed
/// (a track, album or artist ref), without those in `exclude`.
pub async fn radio_next(api: &WebApi, p: RadioNextParams) -> RpcResult {
    let (seed, id) = match parse_ref(&p.seed)? {
        Ref::Track(id) => (RadioSeed::Track, id),
        Ref::Album(id) => (RadioSeed::Album, id),
        Ref::Artist(id) => (RadioSeed::Artist, id),
        Ref::Radio(seed, id) => (seed, id),
        _ => return Err(RpcError::invalid_params(format!("{} cannot seed a radio", p.seed))),
    };
    let limit = p.limit.unwrap_or(20).clamp(1, 50) as usize;
    let tracks = radio_items(api, seed, &id).await?;
    let exclude: HashSet<&str> = p.exclude.iter().map(String::as_str).collect();
    let seed_ref = (seed == RadioSeed::Track).then(|| Ref::Track(id.clone()).to_string());
    let items: Vec<Value> = tracks
        .iter()
        .filter(|t| t.playable && !exclude.contains(t.reference.as_str()) && Some(&t.reference) != seed_ref.as_ref())
        .take(limit)
        .map(Item::to_json)
        .collect();
    Ok(json!({"items": items}))
}

#[derive(Deserialize)]
pub struct SearchParams {
    query: String,
    kinds: Option<Vec<String>>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// Protocol kind → Web API search type and answer key.
fn search_type(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "track" => ("track", "tracks"),
        "album" => ("album", "albums"),
        "artist" => ("artist", "artists"),
        "playlist" => ("playlist", "playlists"),
        _ => return None,
    })
}

pub async fn search(api: &WebApi, p: SearchParams) -> RpcResult {
    let query = p.query.trim();
    if query.is_empty() {
        return Err(RpcError::invalid_params("empty query"));
    }
    let kinds: Vec<&str> = match &p.kinds {
        Some(kinds) => kinds.iter().map(String::as_str).filter(|k| search_type(k).is_some()).collect(),
        None => vec!["track", "album", "artist", "playlist"],
    };
    if kinds.is_empty() {
        return Ok(json!({"groups": []}));
    }
    let limit = clamp(p.limit).min(API_PAGE);
    let types: Vec<&str> = kinds.iter().filter_map(|k| search_type(k)).map(|(t, _)| t).collect();
    let q = [("q", query.to_string()), ("type", types.join(",")), ("offset", p.offset.to_string()), ("limit", limit.to_string())];
    let v = api.get("/search", &q).await?;
    let mut groups: Vec<Value> = kinds
        .iter()
        .map(|&kind| {
            let container = v.get(search_type(kind).expect("filtered above").1);
            let mut group = match kind {
                "track" => items::page(container, p.offset, limit, |t, _| items::track(t, None)),
                "album" => items::page(container, p.offset, limit, |a, _| items::album(a)),
                "artist" => items::page(container, p.offset, limit, |a, _| items::artist(a)),
                _ => items::page(container, p.offset, limit, |p, _| items::playlist(p)),
            };
            group["kind"] = kind.into();
            group
        })
        .collect();
    for g in &mut groups {
        annotate_favorites(api, g).await;
    }
    Ok(json!({"groups": groups}))
}

pub async fn get(api: &WebApi, reference: &str) -> RpcResult {
    let r = parse_ref(reference)?;
    let item = match &r {
        Ref::Track(id) => match api.get(&format!("/tracks/{id}"), &[]).await {
            Ok(t) => items::track(&t, None),
            Err(e) if HttpError::is_withheld(&e) => match api.account().current_session().await {
                Some(s) => metadata_track(&s, id).await,
                None => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        },
        Ref::Album(id) => items::album(&api.get(&format!("/albums/{id}"), &[]).await?),
        Ref::Artist(id) => items::artist(&api.get(&format!("/artists/{id}"), &[]).await?),
        Ref::Playlist(id) => items::playlist(&api.get(&format!("/playlists/{id}"), &[]).await?),
        Ref::Radio(seed, id) => Some(items::radio(*seed, id, None, None)),
        _ => return Ok(folder(r)),
    };
    let mut v = item.map(|i| i.to_json()).ok_or_else(|| RpcError::not_found(format!("{reference}: unreadable answer")))?;
    let mut wrapper = json!({"items": [v.clone()]});
    annotate_favorites(api, &mut wrapper).await;
    if let Some(fav) = wrapper.pointer("/items/0/favorite") {
        v["favorite"] = fav.clone();
    }
    Ok(v)
}

/// `(kind, id)` of an item ref the library can hold.
fn library_kind(reference: &str) -> Option<(&'static str, String)> {
    match Ref::parse(reference)? {
        Ref::Track(id) => Some(("track", id)),
        Ref::Album(id) => Some(("album", id)),
        Ref::Artist(id) => Some(("artist", id)),
        Ref::Playlist(id) => Some(("playlist", id)),
        _ => None,
    }
}

/// Fill in `favorite` where it is not known: the API is asked by fifties,
/// per kind. Failures leave the field absent.
pub async fn annotate_favorites(api: &WebApi, page: &mut Value) {
    let Some(list) = page.get_mut("items").and_then(Value::as_array_mut) else { return };
    let mut wanted: HashMap<&'static str, Vec<String>> = HashMap::new();
    for item in list.iter() {
        if item.get("favorite").is_some() {
            continue;
        }
        if let Some((kind, id)) = item["ref"].as_str().and_then(library_kind).filter(|(k, _)| *k != "playlist") {
            wanted.entry(kind).or_default().push(id);
        }
    }
    for (kind, ids) in wanted {
        let mut seen = HashSet::new();
        let ids: Vec<String> = ids.into_iter().filter(|i| seen.insert(i.clone())).collect();
        for chunk in ids.chunks(API_PAGE as usize) {
            match contains(api, kind, chunk).await {
                Ok(states) => {
                    for (id, on) in chunk.iter().zip(states) {
                        items::note_favorite(kind, id, on);
                    }
                }
                Err(e) => {
                    tracing::debug!("library states of {kind}s: {e:#}");
                    break;
                }
            }
        }
    }
    for item in list.iter_mut() {
        if item.get("favorite").is_none()
            && let Some((kind, id)) = item["ref"].as_str().and_then(library_kind)
            && let Some(on) = items::is_favorite(kind, &id)
        {
            item["favorite"] = on.into();
        }
    }
}

/// Whether each id is in the library.
async fn contains(api: &WebApi, kind: &str, ids: &[String]) -> Result<Vec<bool>> {
    let (path, extra) = match kind {
        "track" => ("/me/tracks/contains", None),
        "album" => ("/me/albums/contains", None),
        _ => ("/me/following/contains", Some(("type", "artist".to_string()))),
    };
    let mut q = vec![("ids", ids.join(","))];
    q.extend(extra);
    let v = match api.get(path, &q).await {
        Ok(v) => v,
        Err(e) if HttpError::is_withheld(&e) => {
            let uris: Vec<String> = ids.iter().map(|id| format!("spotify:{kind}:{id}")).collect();
            api.get("/me/library/contains", &[("uris", uris.join(","))]).await?
        }
        Err(e) => return Err(e),
    };
    let states: Vec<bool> = v.as_array().map(|a| a.iter().map(|b| b.as_bool().unwrap_or(false)).collect()).unwrap_or_default();
    anyhow::ensure!(states.len() == ids.len(), "{} answers for {} ids", states.len(), ids.len());
    Ok(states)
}

pub async fn set_favorite(api: &WebApi, reference: &str, on: bool) -> RpcResult {
    let (kind, id) = library_kind(reference).ok_or_else(|| RpcError::invalid_params(format!("{reference} cannot be a favourite")))?;
    let method = if on { Method::PUT } else { Method::DELETE };
    let result = match kind {
        "track" => api.send(method.clone(), "/me/tracks", &[("ids", id.clone())], None).await,
        "album" => api.send(method.clone(), "/me/albums", &[("ids", id.clone())], None).await,
        "artist" => api.send(method.clone(), "/me/following", &[("type", "artist".into()), ("ids", id.clone())], None).await,
        _ => api.send(method.clone(), &format!("/playlists/{id}/followers"), &[], None).await,
    };
    match result {
        Ok(_) => {}
        Err(e) if HttpError::is_withheld(&e) => {
            api.send(method, "/me/library", &[("uris", format!("spotify:{kind}:{id}"))], None).await?;
        }
        Err(e) => return Err(e.into()),
    }
    items::note_favorite(kind, &id, on);
    if kind == "artist" {
        forget_cursors();
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_tracks_keep_their_order() {
        let ctx: librespot_protocol::context::Context = protobuf_json(json!({
            "uri": "spotify:album:al",
            "pages": [
                {"tracks": [{"uri": "spotify:track:a"}, {"uri": "spotify:episode:e"}]},
                {"tracks": [{"uri": "spotify:track:b"}, {"uri": "spotify:local:x:y:z:1"}]}
            ]
        }));
        assert_eq!(context_track_ids(&ctx), ["a", "b"]);
    }

    fn protobuf_json<M: protobuf::MessageFull>(v: Value) -> M {
        protobuf_json_mapping::parse_from_str(&v.to_string()).unwrap()
    }

    #[test]
    fn library_kinds() {
        assert_eq!(library_kind("track/a"), Some(("track", "a".into())));
        assert_eq!(library_kind("playlist/p"), Some(("playlist", "p".into())));
        assert_eq!(library_kind("liked"), None);
        assert_eq!(window_len(50, 40, 60), 20);
        assert_eq!(window_len(50, 80, 60), 0);
    }
}
