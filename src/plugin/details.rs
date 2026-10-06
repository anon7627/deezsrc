//! `item.details {ref}` (capability `details`): what a page shows beyond the
//! item's children, as `{biography?, related?, facts?}`:
//! - artist: biography (gateway `deezer.pageArtist`); fans and albums
//!   (`/artist/<id>`); top tracks, similar artists, playlists;
//! - album (`/album/<id>`, gateway `album.getData`): label, genres, release
//!   date, type, contributors, copyright; more albums of the artist;
//! - track (gateway `song.getData`, else `/track/<id>`): credits by role,
//!   BPM, release date.
//!
//! Every part is optional: a part Deezer does not answer is left out.
//! Texts are plain: Deezer sends some HTML, which is stripped here.

use serde_json::{json, Value};

use super::items::{self, tr, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::deezer::{num, Deezer};

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

fn text_of(v: Option<&Value>) -> Option<String> {
    let t = plain_text(v?.as_str()?);
    (!t.is_empty()).then_some(t)
}

fn shelf(title: String, items: Vec<Value>) -> Option<Value> {
    (!items.is_empty()).then(|| json!({"title": title, "items": items}))
}

/// `data` of a public API list.
fn data(v: &Option<Value>) -> &[Value] {
    v.as_ref().and_then(|v| v.get("data")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn mapped(list: &[Value], map: impl Fn(&Value) -> Option<items::Item>) -> Vec<Value> {
    list.iter().filter_map(map).take(SHELF_ITEMS).map(|i| i.to_json()).collect()
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

/// Role names of the gateway's `SNG_CONTRIBUTORS`, in the host's language.
fn role(key: &str) -> String {
    match key {
        "main_artist" | "mainartist" => tr("Artist", "Artiste"),
        "featuring" | "featured_artist" => tr("Featuring", "Avec"),
        "composer" => tr("Composer", "Compositeur"),
        "author" | "lyricist" => tr("Lyricist", "Parolier"),
        "producer" => tr("Producer", "Producteur"),
        "engineer" | "mixer" | "mixing_engineer" => tr("Engineer", "Ingénieur du son"),
        "arranger" => tr("Arranger", "Arrangeur"),
        "publisher" | "music_publisher" => tr("Publisher", "Éditeur"),
        other => {
            let mut s = other.replace('_', " ");
            if let Some(first) = s.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            s
        }
    }
}

/// `{role: [names]}` → one fact per role.
fn credits(contributors: &Value) -> Vec<Value> {
    let Some(map) = contributors.as_object() else { return vec![] };
    map.iter()
        .filter_map(|(k, names)| {
            let names: Vec<&str> = names.as_array()?.iter().filter_map(Value::as_str).collect();
            fact(role(k).chars().take(80).collect(), names.join(", "))
        })
        .collect()
}

/// The public API's `contributors: [{name, role}]` → one fact per role.
fn public_credits(contributors: Option<&Value>) -> Vec<Value> {
    let mut roles: Vec<(String, Vec<String>)> = Vec::new();
    for c in contributors.and_then(Value::as_array).into_iter().flatten() {
        let (Some(name), r) = (items::text(c, "name"), items::text(c, "role").unwrap_or_default()) else { continue };
        let label = if r == "Main" || r.is_empty() { tr("Artist", "Artiste") } else { r };
        match roles.iter_mut().find(|(l, _)| *l == label) {
            Some((_, names)) => names.push(name),
            None => roles.push((label, vec![name])),
        }
    }
    roles.into_iter().filter_map(|(label, names)| fact(label, names.join(", "))).collect()
}

pub async fn get(dz: &Deezer, reference: &str, lang: &str) -> RpcResult {
    let r = Ref::parse(reference).ok_or_else(|| RpcError::not_found(format!("unknown ref {reference:?}")))?;
    let mut related: Vec<Value> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut biography = None;
    let ok = |r: anyhow::Result<Value>| r.map_err(|e| tracing::debug!("details: {e:#}")).ok();
    let shelf_q = || vec![("limit", SHELF_ITEMS.to_string())];
    match &r {
        Ref::Artist(id) => {
            let (artist, page, top, similar, playlists) = tokio::join!(
                dz.api(format!("artist/{id}"), vec![]),
                dz.gw("deezer.pageArtist", json!({"ART_ID": id, "LANG": lang})),
                dz.api(format!("artist/{id}/top"), shelf_q()),
                dz.api(format!("artist/{id}/related"), shelf_q()),
                dz.api(format!("artist/{id}/playlists"), shelf_q()),
            );
            let artist = artist?;
            if let Some(page) = ok(page) {
                biography = text_of(page.pointer("/BIO/BIO")).or_else(|| text_of(page.pointer("/BIO/RESUME")));
            }
            facts.extend(num(artist.get("nb_fan")).filter(|&n| n > 0).and_then(|n| fact(tr("Fans", "Fans"), n.to_string())));
            facts.extend(num(artist.get("nb_album")).filter(|&n| n > 0).and_then(|n| fact(tr("Albums", "Albums"), n.to_string())));
            let (top, similar, playlists) = (ok(top), ok(similar), ok(playlists));
            related.extend(shelf(tr("Top tracks", "Titres phares"), mapped(data(&top), |t| items::track(t, None))));
            related.extend(shelf(tr("Similar artists", "Artistes similaires"), mapped(data(&similar), items::artist)));
            related.extend(shelf(tr("Playlists", "Playlists"), mapped(data(&playlists), items::playlist)));
        }
        Ref::Album(id) => {
            let (album, gw_album) = tokio::join!(dz.api(format!("album/{id}"), vec![]), dz.gw("album.getData", json!({"ALB_ID": id})));
            let album = album?;
            let s = |key: &str| items::text(&album, key);
            facts.extend(s("label").and_then(|v| fact(tr("Label", "Label"), v)));
            let genres: Vec<String> = data(&album.get("genres").cloned()).iter().filter_map(|g| items::text(g, "name")).collect();
            if !genres.is_empty() {
                facts.extend(fact(tr("Genre", "Genre"), genres.join(", ")));
            }
            facts.extend(s("release_date").and_then(|v| fact(tr("Released", "Sortie"), date(&v))));
            let kind = s("record_type").map(|t| match t.as_str() {
                "album" => tr("Album", "Album"),
                "single" => tr("Single", "Single"),
                "ep" => "EP".into(),
                "compile" => tr("Compilation", "Compilation"),
                other => other.to_string(),
            });
            facts.extend(kind.and_then(|v| fact(tr("Type", "Type"), v)));
            facts.extend(public_credits(album.get("contributors")));
            if let Some(g) = ok(gw_album) {
                facts.extend(items::text(&g, "COPYRIGHT").and_then(|v| fact("©".into(), v)));
                facts.extend(items::text(&g, "PRODUCER_LINE").and_then(|v| fact("℗".into(), v)));
            }
            if let Some(artist_id) = album.get("artist").and_then(|a| items::id_at(a, "id")) {
                let others = ok(dz.api(format!("artist/{artist_id}/albums"), shelf_q()).await);
                let others: Vec<Value> =
                    data(&others).iter().filter(|a| items::id_at(a, "id").as_deref() != Some(id.as_str())).cloned().collect();
                let name = album.get("artist").and_then(|a| items::text(a, "name")).unwrap_or_default();
                let title = format!("{} {name}", tr("More by", "Plus de"));
                related.extend(shelf(title.trim().to_string(), mapped(&others, |a| items::album(a, album.get("artist")))));
            }
        }
        Ref::Track(id) => {
            let (song, track) = tokio::join!(dz.gw("song.getData", json!({"SNG_ID": id})), dz.api(format!("track/{id}"), vec![]));
            let (song, track) = (ok(song), ok(track));
            if song.is_none() && track.is_none() {
                return Err(RpcError::not_found(format!("no details for {reference}")));
            }
            match song.as_ref().and_then(|s| s.get("SNG_CONTRIBUTORS")).filter(|c| c.as_object().is_some_and(|o| !o.is_empty())) {
                Some(c) => facts.extend(credits(c)),
                None => facts.extend(public_credits(track.as_ref().and_then(|t| t.get("contributors")))),
            }
            if let Some(t) = &track {
                facts.extend(num(t.get("bpm")).filter(|&b| b > 0).and_then(|b| fact("BPM".into(), b.to_string())));
                facts.extend(items::text(t, "release_date").and_then(|v| fact(tr("Released", "Sortie"), date(&v))));
            }
        }
        _ => {}
    }
    facts.truncate(MAX_FACTS);
    let mut out = json!({});
    if let Some(text) = biography {
        out["biography"] = json!({"text": text, "source": "Deezer"});
    }
    if !related.is_empty() {
        out["related"] = related.into();
    }
    if !facts.is_empty() {
        out["facts"] = facts.into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        assert_eq!(
            plain_text("<p>Daft Punk s&rsquo;est <b>imposé</b>&nbsp;:</p><p>Line<br/>break &amp; co &#233;t&#xE9;</p>"),
            "Daft Punk s’est imposé :\n\nLine\nbreak & co été"
        );
        assert_eq!(plain_text("no markup, a & b"), "no markup, a & b");
        assert_eq!(plain_text("<script>x"), "x", "tags go, text stays text");
        assert_eq!(plain_text("a <b unclosed"), "a");
    }

    #[test]
    fn credits_become_facts() {
        let f = credits(&json!({"main_artist": ["Daft Punk"], "composer": ["Thomas Bangalter", "Guy-Manuel de Homem-Christo"],
                                "sound_designer": ["X"], "empty": []}));
        assert_eq!(f, [
            json!({"label": "Composer", "value": "Thomas Bangalter, Guy-Manuel de Homem-Christo"}),
            json!({"label": "Artist", "value": "Daft Punk"}),
            json!({"label": "Sound designer", "value": "X"}),
        ]);
        let f = public_credits(Some(&json!([{"name": "Daft Punk", "role": "Main"}, {"name": "Romanthony", "role": "Featured"},
                                             {"name": "DJ Falcon", "role": "Featured"}])));
        assert_eq!(f, [json!({"label": "Artist", "value": "Daft Punk"}), json!({"label": "Featured", "value": "Romanthony, DJ Falcon"})]);
    }
}
