//! `playlists.*` (capability `playlist_edit`): edits of the account's own
//! playlists, through the gateway as the web player makes them:
//!
//! | Method | Gateway method | Arguments |
//! |---|---|---|
//! | `playlists.create {name, description?, public?}` | `playlist.create` | `title`, `description`, `status` (0 public, 1 private), `songs: []` |
//! | `playlists.rename {ref, name}` | `playlist.update` | `PLAYLIST_ID`, `title`, the current `description` and `status` |
//! | `playlists.delete {ref}` | `playlist.delete` | `PLAYLIST_ID` |
//! | `playlists.add {ref, items}` | `playlist.addSongs` | `PLAYLIST_ID`, `songs: [[id, 0]…]`, `offset: -1` (the end) |
//! | `playlists.remove {ref, entries}` | `playlist.deleteSongs` | `PLAYLIST_ID`, `songs: [[id, 0]…]` |
//! | `playlists.move {ref, entry, to}` | `playlist.deleteSongs`, then `playlist.addSongs` | the entry taken out, then put back with `offset: to` |
//!
//! Deezer keeps a track once per playlist, so an entry is named by its
//! track id. A move takes the entry out and puts it back at `to`, which is
//! exactly the protocol's definition of `to`; the track's "added on" date
//! becomes the date of the move.
//!
//! Every edit of an existing playlist first checks that the account owns
//! it: a followed playlist is never touched.

use serde::Deserialize;
use serde_json::{json, Value};

use super::catalog::{Library, Tab};
use super::items::{self, valid_id, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::deezer::{num, Deezer};

const MAX_NAME: usize = 200;
const MAX_DESCRIPTION: usize = 2000;
/// Tracks per request.
const MAX_TRACKS: usize = 500;
/// Tracks read per page when looking for an entry.
const SCAN_PAGE: u64 = 500;

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

fn entry_id(s: &str) -> Result<String, RpcError> {
    valid_id(s).then(|| s.to_string()).ok_or_else(|| invalid(format!("{s:?} is not a playlist entry")))
}

fn songs(ids: &[String]) -> Value {
    ids.iter().map(|id| json!([id, 0])).collect()
}

/// The playlist's id and data, once checked to be the account's own.
async fn owned(dz: &Deezer, p: &Params) -> Result<(String, Value), RpcError> {
    let reference = p.reference.as_deref().ok_or_else(|| invalid("ref is needed"))?;
    let Some(Ref::Playlist(id)) = Ref::parse(reference) else {
        return Err(invalid(format!("{reference} is not a playlist")));
    };
    let user = dz.user().await?;
    let playlist = dz.gw("playlist.getData", json!({"PLAYLIST_ID": id})).await?;
    if num(playlist.get("PARENT_USER_ID")) != Some(user.id) {
        return Err(invalid(format!("{reference} belongs to another account")));
    }
    Ok((id, playlist))
}

/// Position (0-based) of `entry` in the playlist, and the playlist's length.
async fn entry_index(dz: &Deezer, playlist_id: &str, entry: &str) -> Result<(usize, usize), RpcError> {
    let mut start = 0u64;
    loop {
        let page = dz.gw("playlist.getSongs", json!({"PLAYLIST_ID": playlist_id, "start": start, "nb": SCAN_PAGE})).await?;
        let songs = page.get("data").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        // A gateway that ignored `start` sends the whole list from the top.
        let served_from = if songs.len() as u64 > SCAN_PAGE { 0 } else { start };
        let total = num(page.get("total")).unwrap_or(0).max(served_from + songs.len() as u64);
        if let Some(i) = songs.iter().position(|s| items::id_at(s, "SNG_ID").as_deref() == Some(entry)) {
            return Ok((served_from as usize + i, total as usize));
        }
        let next = served_from + songs.len() as u64;
        if songs.is_empty() || next >= total {
            return Err(RpcError::not_found(format!("no entry {entry} in playlist {playlist_id}")));
        }
        start = next;
    }
}

pub async fn edit(dz: &Deezer, lib: &Library, method: &str, p: Params) -> RpcResult {
    let result = edit_inner(dz, method, p).await;
    lib.forget(Some(Tab::Playlists));
    result
}

async fn edit_inner(dz: &Deezer, method: &str, p: Params) -> RpcResult {
    match method {
        "playlists.create" => {
            let description: String =
                p.description.as_deref().map(str::trim).unwrap_or("").chars().take(MAX_DESCRIPTION).collect();
            let status = if p.public.unwrap_or(false) { 0 } else { 1 };
            let args = json!({"title": name(&p)?, "description": description, "status": status, "songs": []});
            let created = dz.gw("playlist.create", args).await?;
            // The answer is the new id, as a number or a string.
            let id = match &created {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                other => items::id_at(other, "PLAYLIST_ID").unwrap_or_default(),
            };
            if !valid_id(&id) {
                return Err(RpcError::new(super::rpc::INTERNAL, "playlist.create: unreadable answer"));
            }
            let data = dz.gw("playlist.getData", json!({"PLAYLIST_ID": id})).await?;
            items::playlist(&data)
                .map(|i| i.to_json())
                .ok_or_else(|| RpcError::new(super::rpc::INTERNAL, "playlist.getData: unreadable answer"))
        }
        "playlists.rename" => {
            let title = name(&p)?;
            let (id, data) = owned(dz, &p).await?;
            let mut args = json!({"PLAYLIST_ID": id, "title": title});
            // Sent back as they are, so that the rename changes nothing else.
            if let Some(d) = data.get("DESCRIPTION").and_then(Value::as_str) {
                args["description"] = d.into();
            }
            if let Some(s) = num(data.get("STATUS")) {
                args["status"] = s.into();
            }
            dz.gw("playlist.update", args).await?;
            Ok(Value::Null)
        }
        "playlists.delete" => {
            let (id, _) = owned(dz, &p).await?;
            dz.gw("playlist.delete", json!({"PLAYLIST_ID": id})).await?;
            Ok(Value::Null)
        }
        "playlists.add" => {
            let tracks: Vec<String> = p
                .items
                .iter()
                .map(|r| match Ref::parse(r) {
                    Some(Ref::Track(id)) => Ok(id),
                    _ => Err(invalid(format!("{r} is not a track"))),
                })
                .collect::<Result<_, _>>()?;
            if tracks.is_empty() || tracks.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} tracks at a time")));
            }
            let (id, _) = owned(dz, &p).await?;
            dz.gw("playlist.addSongs", json!({"PLAYLIST_ID": id, "songs": songs(&tracks), "offset": -1})).await?;
            Ok(Value::Null)
        }
        "playlists.remove" => {
            let entries: Vec<String> = p.entries.iter().map(|e| entry_id(e)).collect::<Result<_, _>>()?;
            if entries.is_empty() || entries.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} entries at a time")));
            }
            let (id, _) = owned(dz, &p).await?;
            dz.gw("playlist.deleteSongs", json!({"PLAYLIST_ID": id, "songs": songs(&entries)})).await?;
            Ok(Value::Null)
        }
        "playlists.move" => {
            let entry = entry_id(p.entry.as_deref().ok_or_else(|| invalid("entry is needed"))?)?;
            let to = p.to.ok_or_else(|| invalid("to is needed"))? as usize;
            let (id, _) = owned(dz, &p).await?;
            let (from, len) = entry_index(dz, &id, &entry).await?;
            // Past the end: last.
            let to = to.min(len.saturating_sub(1));
            if from == to {
                return Ok(Value::Null);
            }
            let song = songs(std::slice::from_ref(&entry));
            dz.gw("playlist.deleteSongs", json!({"PLAYLIST_ID": id, "songs": song})).await?;
            let offset: i64 = if to + 1 >= len { -1 } else { to as i64 };
            if let Err(e) = dz.gw("playlist.addSongs", json!({"PLAYLIST_ID": id, "songs": song, "offset": offset})).await {
                // Never lose the track: put it back at the end.
                tracing::warn!("playlist move: putting entry {entry} back failed ({e:#}); appending it");
                dz.gw("playlist.addSongs", json!({"PLAYLIST_ID": id, "songs": song, "offset": -1})).await?;
                return Err(e.into());
            }
            Ok(Value::Null)
        }
        _ => Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_track_ids() {
        assert_eq!(entry_id("3135556").unwrap(), "3135556");
        assert!(entry_id("").is_err() && entry_id("1,2").is_err() && entry_id("abc").is_err());
        assert_eq!(songs(&["1".into(), "2".into()]), json!([["1", 0], ["2", 0]]));
    }

    #[test]
    fn names_are_trimmed_and_required() {
        let p = |n: &str| Params {
            reference: None, name: Some(n.into()), description: None, public: None,
            items: vec![], entries: vec![], entry: None, to: None,
        };
        assert_eq!(name(&p("  Soir  ")).unwrap(), "Soir");
        assert!(name(&p("   ")).is_err());
    }
}
