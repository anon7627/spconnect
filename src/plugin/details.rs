//! `item.details {ref}` (capability `details`): what a page shows beyond the
//! item's children, as `{biography?, related?, facts?}`:
//! - artist: biography (session metadata), top tracks, related artists;
//! - album: label, release date, tracks, copyright; more by the artist;
//! - track: credits (`track-credits-view`, the apps' credits page).
//!
//! Each part comes from wherever it is still served; a part that is not is
//! left out. Texts are plain: biographies carry some HTML, stripped here.

use futures_util::future::join;
use http::Method;
use librespot_core::{Session, SpotifyId, SpotifyUri};
use serde_json::{Value, json};

use super::catalog;
use super::items::{self, Ref, tr};
use super::rpc::{RpcError, RpcResult};
use crate::webapi::WebApi;

/// Items per related shelf.
const SHELF_ITEMS: usize = 20;
const MAX_FACTS: usize = 30;
const MAX_FACT_VALUE: usize = 500;

/// Plain text from HTML: tags dropped, `<br>` and paragraph ends as line
/// breaks, common entities decoded, blank lines collapsed.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/').split([' ', '/']).next().unwrap_or("");
        if matches!(name, "br" | "p" | "div" | "li") && (name == "br" || tag.starts_with('/')) {
            out.push('\n');
            if name == "p" {
                out.push('\n');
            }
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let decoded = decode_entities(&out);
    let mut text = String::new();
    let mut blank = 0;
    for line in decoded.lines().map(str::trim) {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        text.push_str(line);
    }
    text
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "rsquo" | "lsquo" => Some('’'),
            "hellip" => Some('…'),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn shelf(title: String, items: Vec<Value>) -> Option<Value> {
    (!items.is_empty()).then(|| json!({"title": title, "items": items}))
}

fn fact(label: String, value: impl Into<String>) -> Option<Value> {
    let value: String = value.into().trim().chars().take(MAX_FACT_VALUE).collect();
    (!value.is_empty()).then(|| json!({"label": label, "value": value}))
}

/// `2026-09-25` as the host's language writes dates.
fn date(iso: &str) -> String {
    match (iso.get(..4), iso.get(5..7), iso.get(8..10), tr("en", "fr").as_str()) {
        (Some(y), Some(m), Some(d), "fr") => format!("{d}/{m}/{y}"),
        _ => iso.to_string(),
    }
}

/// Credits page: `{roleCredits: [{roleTitle, artists: [{name, subroles}]}]}`
/// → one fact per person and role.
pub fn credits(doc: &Value) -> Vec<Value> {
    let mut facts = Vec::new();
    for role in doc.get("roleCredits").and_then(Value::as_array).into_iter().flatten() {
        let title = role.get("roleTitle").and_then(Value::as_str).unwrap_or("").trim();
        for artist in role.get("artists").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = artist.get("name").and_then(Value::as_str) else { continue };
            let subroles: Vec<&str> = artist.get("subroles").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let label = if subroles.is_empty() { title.to_string() } else { subroles.join(", ") };
            let label = if label.is_empty() { tr("Credits", "Crédits") } else { capitalise(&label) };
            facts.extend(fact(label.chars().take(80).collect(), name));
        }
    }
    facts
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

async fn session_artist(session: Option<&Session>, id: &str) -> Option<librespot_metadata::Artist> {
    let session = session?;
    let uri = SpotifyUri::Artist { id: SpotifyId::from_base62(id).ok()? };
    <librespot_metadata::Artist as librespot_metadata::Metadata>::get(session, &uri).await.ok()
}

pub async fn get(api: &WebApi, reference: &str) -> RpcResult {
    let r = Ref::parse(reference).ok_or_else(|| RpcError::not_found(format!("unknown ref {reference:?}")))?;
    let session = api.account().session().await.ok();
    let mut related: Vec<Value> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut biography = None;
    match &r {
        Ref::Artist(id) => {
            let (top, meta) = join(api.get(&format!("/artists/{id}/top-tracks"), &[("market", "from_token".into())]), session_artist(session.as_ref(), id)).await;
            biography = meta.as_ref().and_then(|a| a.biographies.iter().map(|b| plain_text(&b.text)).find(|t| !t.is_empty()));
            let mut top_items: Vec<Value> =
                top.ok().and_then(|v| v.get("tracks").cloned()).and_then(|t| t.as_array().cloned()).unwrap_or_default().iter().filter_map(|t| items::track(t, None)).take(SHELF_ITEMS).map(|i| i.to_json()).collect();
            if top_items.is_empty()
                && let Some(meta) = &meta
            {
                // The session's metadata lists top tracks per country.
                let country = session.as_ref().map(|s| s.country()).unwrap_or_default();
                let ids: Vec<String> = meta
                    .top_tracks
                    .iter()
                    .find(|t| t.country == country)
                    .or_else(|| meta.top_tracks.first())
                    .map(|t| t.tracks.iter().filter_map(|u| u.to_id().ok()).take(SHELF_ITEMS).collect())
                    .unwrap_or_default();
                top_items = catalog::tracks_by_ids(api, &ids).await.iter().map(|i| i.to_json()).collect();
            }
            related.extend(shelf(tr("Top tracks", "Titres phares"), top_items));
            let similar = catalog_related(api, id).await;
            related.extend(shelf(tr("Related artists", "Artistes similaires"), similar));
        }
        Ref::Album(id) => {
            let album = api.get(&format!("/albums/{id}"), &[]).await?;
            let s = |path: &str| album.pointer(path).and_then(Value::as_str).map(str::to_string);
            facts.extend(s("/label").and_then(|v| fact(tr("Label", "Label"), v)));
            facts.extend(s("/release_date").and_then(|v| fact(tr("Released", "Sortie"), date(&v))));
            if let Some(n) = album.get("total_tracks").and_then(Value::as_u64) {
                facts.extend(fact(tr("Tracks", "Titres"), n.to_string()));
            }
            if let Some(g) = album.get("genres").and_then(Value::as_array).filter(|g| !g.is_empty()) {
                let genres: Vec<&str> = g.iter().filter_map(Value::as_str).collect();
                facts.extend(fact(tr("Genre", "Genre"), genres.join(", ")));
            }
            for c in album.get("copyrights").and_then(Value::as_array).into_iter().flatten() {
                let label = if c.get("type").and_then(Value::as_str) == Some("P") { "℗" } else { "©" };
                facts.extend(c.get("text").and_then(Value::as_str).and_then(|t| fact(label.into(), t)));
            }
            if let Some(artist_id) = album.pointer("/artists/0/id").and_then(Value::as_str) {
                let q = [("include_groups", "album,single".to_string()), ("limit", "50".to_string())];
                if let Ok(v) = api.get(&format!("/artists/{artist_id}/albums"), &q).await {
                    let more: Vec<Value> = v
                        .get("items")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|a| a.get("id").and_then(Value::as_str) != Some(id))
                        .filter_map(items::album)
                        .take(SHELF_ITEMS)
                        .map(|i| i.to_json())
                        .collect();
                    let name = album.pointer("/artists/0/name").and_then(Value::as_str).unwrap_or("");
                    related.extend(shelf(format!("{} {name}", tr("More by", "Du même artiste :")).trim().to_string(), more));
                }
            }
        }
        Ref::Track(id) => {
            if let Some(session) = &session {
                let endpoint = format!("/track-credits-view/v0/experimental/{id}/credits");
                match session.spclient().request_as_json(&Method::GET, &endpoint, None, None).await {
                    Ok(raw) => {
                        if let Ok(doc) = serde_json::from_slice::<Value>(&raw) {
                            facts.extend(credits(&doc));
                        }
                    }
                    Err(e) => tracing::debug!("credits of {id}: {e}"),
                }
            }
        }
        _ => {}
    }
    facts.truncate(MAX_FACTS);
    let mut out = json!({});
    if let Some(text) = biography {
        out["biography"] = json!({"text": text, "source": "Spotify"});
    }
    if !related.is_empty() {
        out["related"] = related.into();
    }
    if !facts.is_empty() {
        out["facts"] = facts.into();
    }
    Ok(out)
}

/// The related-artists list of the catalogue, as items.
async fn catalog_related(api: &WebApi, id: &str) -> Vec<Value> {
    let r = json!({"ref": format!("related/{id}"), "offset": 0, "limit": SHELF_ITEMS});
    match serde_json::from_value(r) {
        Ok(p) => catalog::list(api, p).await.ok().and_then(|v| v.get("items").cloned()).and_then(|v| v.as_array().cloned()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        assert_eq!(
            plain_text("<p>Rick Astley s&rsquo;est <b>imposé</b>&nbsp;:</p><p>Line<br/>break &amp; co &#233;t&#xE9;</p>"),
            "Rick Astley s’est imposé :\n\nLine\nbreak & co été"
        );
        assert_eq!(plain_text("no markup, a & b"), "no markup, a & b");
        assert_eq!(plain_text("<a href=\"spotify:artist:x\">Link</a> text"), "Link text");
    }

    #[test]
    fn credits_become_facts() {
        let doc = json!({"trackUri": "spotify:track:x", "roleCredits": [
            {"roleTitle": "Performers", "artists": [{"name": "Rick Astley", "subroles": ["vocals"]}]},
            {"roleTitle": "Writers", "artists": [{"name": "Mike Stock", "subroles": []}, {"name": "Pete Waterman"}]},
            {"roleTitle": "Producers", "artists": []}
        ]});
        let f = credits(&doc);
        assert_eq!(f[0], json!({"label": "Vocals", "value": "Rick Astley"}));
        assert_eq!(f[1], json!({"label": "Writers", "value": "Mike Stock"}));
        assert_eq!(f.len(), 3);
    }
}
