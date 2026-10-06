//! `playlists.*` (capability `playlist_edit`): edits of the account's own
//! playlists through the Web API.
//!
//! | Method | Request |
//! |---|---|
//! | `playlists.create {name, description?, public?}` | `POST /me/playlists` (`POST /users/{id}/playlists` where the former is missing) |
//! | `playlists.rename {ref, name}` | `PUT /playlists/{id}` `{name}` |
//! | `playlists.delete {ref}` | `DELETE /playlists/{id}/followers`: Spotify has no deletion, the owner unfollows |
//! | `playlists.add {ref, items}` | `POST /playlists/{id}/tracks` `{uris}` (then `/items`) |
//! | `playlists.remove {ref, entries}` | `DELETE /playlists/{id}/tracks` `{tracks: [{uri, positions}], snapshot_id}` |
//! | `playlists.move {ref, entry, to}` | `PUT /playlists/{id}/tracks` `{range_start, insert_before, range_length: 1}` |
//!
//! Entry ids are `<position>:<track id>`: Spotify removes by position and
//! checks that the track there is the one named, so an entry that moved
//! meanwhile is refused instead of removing another track.
//!
//! `to` is where the entry ends up (0-based, protocol), while Spotify's
//! `insert_before` is a position in the list before the move: moving up,
//! the entry goes before the one now at `to`; moving down, before the one
//! now at `to + 1`.
//!
//! Every edit of an existing playlist first checks that the account owns
//! it: a followed playlist is never touched.

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use super::items::{self, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::webapi::{HttpError, WebApi};

const MAX_NAME: usize = 200;
const MAX_DESCRIPTION: usize = 300;
/// Tracks per request, the Web API's limit.
const MAX_TRACKS: usize = 100;

#[derive(Deserialize)]
pub struct Params {
    #[serde(rename = "ref")]
    reference: Option<String>,
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    entries: Vec<String>,
    entry: Option<String>,
    to: Option<u32>,
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::invalid_params(msg)
}

fn name(p: &Params) -> Result<String, RpcError> {
    let name = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| invalid("a name is needed"))?;
    Ok(name.chars().take(MAX_NAME).collect())
}

/// The playlist's id and the answer about it, once checked to be the
/// account's own.
async fn owned(api: &WebApi, p: &Params) -> Result<(String, Value), RpcError> {
    let reference = p.reference.as_deref().ok_or_else(|| invalid("ref is needed"))?;
    let Some(Ref::Playlist(id)) = Ref::parse(reference) else {
        return Err(invalid(format!("{reference} is not a playlist")));
    };
    let playlist = api.get(&format!("/playlists/{id}"), &[]).await?;
    if items::owned(&playlist) != Some(true) {
        return Err(invalid(format!("{reference} belongs to another account")));
    }
    Ok((id, playlist))
}

/// `/playlists/{id}/tracks`, or `/items` where the API has renamed it.
async fn tracks_request(api: &WebApi, method: Method, id: &str, body: &Value) -> anyhow::Result<Value> {
    match api.send(method.clone(), &format!("/playlists/{id}/tracks"), &[], Some(body)).await {
        Err(e) if matches!(HttpError::status_of(&e), Some(404 | 405 | 410)) => {
            let mut body = body.clone();
            if let Some(tracks) = body.as_object_mut().and_then(|o| o.remove("tracks")) {
                body["items"] = tracks;
            }
            api.send(method, &format!("/playlists/{id}/items"), &[], Some(&body)).await
        }
        other => other,
    }
}

/// `insert_before` for moving the entry at `from` to end up at `to`, or
/// `None` when it stays where it is.
fn insert_before(from: usize, to: usize) -> Option<usize> {
    match from.cmp(&to) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(to),
        std::cmp::Ordering::Less => Some(to + 1),
    }
}

pub async fn edit(api: &WebApi, method: &str, p: Params) -> RpcResult {
    match method {
        "playlists.create" => {
            let mut body = json!({"name": name(&p)?, "public": p.public.unwrap_or(false)});
            if let Some(d) = p.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                body["description"] = d.chars().take(MAX_DESCRIPTION).collect::<String>().into();
            }
            let created = match api.send(Method::POST, "/me/playlists", &[], Some(&body)).await {
                Err(e) if matches!(HttpError::status_of(&e), Some(404 | 405)) => {
                    let user = items::user_id().ok_or_else(RpcError::auth_required)?;
                    api.send(Method::POST, &format!("/users/{user}/playlists"), &[], Some(&body)).await?
                }
                other => other?,
            };
            items::playlist(&created)
                .map(|i| i.to_json())
                .ok_or_else(|| RpcError::new(super::rpc::INTERNAL, "playlist creation: unreadable answer"))
        }
        "playlists.rename" => {
            let new_name = name(&p)?;
            let (id, _) = owned(api, &p).await?;
            api.send(Method::PUT, &format!("/playlists/{id}"), &[], Some(&json!({"name": new_name}))).await?;
            Ok(Value::Null)
        }
        "playlists.delete" => {
            let (id, _) = owned(api, &p).await?;
            match api.send(Method::DELETE, &format!("/playlists/{id}/followers"), &[], None).await {
                Err(e) if HttpError::is_withheld(&e) => {
                    api.send(Method::DELETE, "/me/library", &[("uris", format!("spotify:playlist:{id}"))], None).await?;
                }
                other => {
                    other?;
                }
            }
            Ok(Value::Null)
        }
        "playlists.add" => {
            let uris: Vec<String> = p
                .items
                .iter()
                .map(|r| match Ref::parse(r) {
                    Some(t @ Ref::Track(_)) => Ok(t.uri().expect("a track has a URI")),
                    _ => Err(invalid(format!("{r} is not a track"))),
                })
                .collect::<Result<_, _>>()?;
            if uris.is_empty() {
                return Err(invalid("no tracks to add"));
            }
            let (id, _) = owned(api, &p).await?;
            for chunk in uris.chunks(MAX_TRACKS) {
                tracks_request(api, Method::POST, &id, &json!({"uris": chunk})).await?;
            }
            Ok(Value::Null)
        }
        "playlists.remove" => {
            let entries: Vec<(u64, String)> = p
                .entries
                .iter()
                .map(|e| items::parse_entry(e).ok_or_else(|| invalid(format!("{e:?} is not a playlist entry"))))
                .collect::<Result<_, _>>()?;
            if entries.is_empty() || entries.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} entries at a time")));
            }
            let (id, playlist) = owned(api, &p).await?;
            // One object per track, with all its positions.
            let mut by_track: Vec<(String, Vec<u64>)> = Vec::new();
            for (pos, track) in entries {
                match by_track.iter_mut().find(|(t, _)| *t == track) {
                    Some((_, positions)) => positions.push(pos),
                    None => by_track.push((track, vec![pos])),
                }
            }
            let tracks: Vec<Value> = by_track
                .into_iter()
                .map(|(t, positions)| json!({"uri": format!("spotify:track:{t}"), "positions": positions}))
                .collect();
            let mut body = json!({"tracks": tracks});
            if let Some(snapshot) = playlist.get("snapshot_id").filter(|s| s.is_string()) {
                body["snapshot_id"] = snapshot.clone();
            }
            tracks_request(api, Method::DELETE, &id, &body).await?;
            Ok(Value::Null)
        }
        "playlists.move" => {
            let entry = p.entry.as_deref().ok_or_else(|| invalid("entry is needed"))?;
            let (from, _) = items::parse_entry(entry).ok_or_else(|| invalid(format!("{entry:?} is not a playlist entry")))?;
            let to = p.to.ok_or_else(|| invalid("to is needed"))?;
            let (id, playlist) = owned(api, &p).await?;
            let len = playlist.pointer("/tracks/total").or(playlist.pointer("/items/total")).and_then(Value::as_u64).unwrap_or(from + 1).max(from + 1);
            // Past the end: last.
            let to = u64::from(to).min(len - 1);
            let Some(before) = insert_before(from as usize, to as usize) else { return Ok(Value::Null) };
            let mut body = json!({"range_start": from, "insert_before": before, "range_length": 1});
            if let Some(snapshot) = playlist.get("snapshot_id").filter(|s| s.is_string()) {
                body["snapshot_id"] = snapshot.clone();
            }
            tracks_request(api, Method::PUT, &id, &body).await?;
            Ok(Value::Null)
        }
        _ => Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spotify inserts before a position of the list before the move; the
    /// protocol's `to` is the final 0-based position.
    #[test]
    fn moves_up_and_down() {
        let simulate = |list: &[char], from: usize, to: usize| -> Vec<char> {
            let Some(before) = insert_before(from, to) else { return list.to_vec() };
            // What Spotify does: put the entry before `before` (in the list
            // as it was), then drop it from its old place.
            let mut out: Vec<Option<char>> = list.iter().map(|c| Some(*c)).collect();
            let moved = out[from].take();
            out.insert(before, moved);
            out.into_iter().flatten().collect()
        };
        let list = ['A', 'B', 'C', 'D'];
        for from in 0..4 {
            for to in 0..4 {
                let mut expected = list.to_vec();
                let e = expected.remove(from);
                expected.insert(to, e);
                assert_eq!(simulate(&list, from, to), expected, "from {from} to {to}");
            }
        }
    }

    #[test]
    fn names_are_trimmed_and_required() {
        let p = |n: &str| Params {
            reference: None,
            name: Some(n.into()),
            description: None,
            public: None,
            items: vec![],
            entries: vec![],
            entry: None,
            to: None,
        };
        assert_eq!(name(&p("  Soir  ")).unwrap(), "Soir");
        assert!(name(&p("   ")).is_err());
    }
}
