//! `browse.root`, `browse.list`, `library.*`, `search`, `item.get`,
//! `favorites.set`, `radio.next`.
//!
//! The catalogue comes from the public API; what belongs to the account
//! (favourites, playlists, Flow) from the gateway.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::future::try_join_all;
use serde::Deserialize;
use serde_json::{json, Value};

use super::items::{self, tr, RadioSeed, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::deezer::{Deezer, SignedOut};

/// Protocol maximum page size.
const MAX_PAGE: u32 = 200;
const DEFAULT_PAGE: u32 = 50;
/// Library lists are read whole, then cut into pages; read again after this.
const LIBRARY_MAX_AGE: Duration = Duration::from_secs(60);
/// Most entries read from one library list.
const LIBRARY_MAX: u32 = 10_000;
/// Tracks per `song.getListData` request.
const SONGS_PER_REQUEST: usize = 200;
/// Tracks asked for in an artist mix.
const MIX_SIZE: u32 = 50;

fn clamp(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

pub fn parse_ref(s: &str) -> Result<Ref, RpcError> {
    Ref::parse(s).ok_or_else(|| RpcError::not_found(format!("unknown ref {s:?}")))
}

fn q(pairs: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    pairs.to_vec()
}

fn paged(offset: u32, limit: u32) -> Vec<(&'static str, String)> {
    q(&[("index", offset.to_string()), ("limit", limit.to_string())])
}

pub fn section_title(r: &Ref) -> String {
    match r {
        Ref::Favorites => tr("Favourites", "Coups de cœur"),
        Ref::FavTracks => tr("Tracks", "Titres"),
        Ref::FavAlbums => tr("Albums", "Albums"),
        Ref::FavArtists => tr("Artists", "Artistes"),
        Ref::MyPlaylists => tr("My playlists", "Mes playlists"),
        Ref::Flow => "Flow".into(),
        Ref::Charts => tr("Charts", "Classements"),
        Ref::Chart(kind) => match kind.as_str() {
            "tracks" => tr("Top tracks", "Top titres"),
            "albums" => tr("Top albums", "Top albums"),
            "artists" => tr("Top artists", "Top artistes"),
            _ => tr("Top playlists", "Top playlists"),
        },
        Ref::Releases => tr("New releases", "Nouveautés"),
        Ref::Genres => tr("Genres", "Genres"),
        Ref::Radio(..) => tr("Mix", "Mix"),
        Ref::Similar(_) => tr("Similar artists", "Artistes similaires"),
        _ => "?".into(),
    }
}

fn folder(r: Ref) -> Value {
    items::folder(&r, &section_title(&r)).to_json()
}

/// `sections` for hosts that show the plugin in their sidebar; `home`, the
/// discovery shelves, for hosts that merge the `library` lists instead
/// (favourites and playlists already reach them that way).
pub fn root() -> RpcResult {
    let mut sections: Vec<Value> = [Ref::Favorites, Ref::MyPlaylists].into_iter().map(folder).collect();
    sections.push(items::flow().to_json());
    sections.extend([Ref::Charts, Ref::Releases, Ref::Genres].into_iter().map(folder));
    let mut home = vec![items::flow().to_json(), folder(Ref::Releases)];
    home.extend(["tracks", "albums", "playlists"].map(|k| folder(Ref::Chart(k.into()))));
    Ok(json!({"sections": sections, "home": home}))
}

/// One of the account's lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Tracks,
    Albums,
    Artists,
    Playlists,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Tracks, Tab::Albums, Tab::Artists, Tab::Playlists];

    /// Item kind of its entries.
    pub fn kind(self) -> &'static str {
        match self {
            Tab::Tracks => "track",
            Tab::Albums => "album",
            Tab::Artists => "artist",
            Tab::Playlists => "playlist",
        }
    }

    fn id_key(self) -> &'static str {
        match self {
            Tab::Tracks => "SNG_ID",
            Tab::Albums => "ALB_ID",
            Tab::Artists => "ART_ID",
            Tab::Playlists => "PLAYLIST_ID",
        }
    }
}

/// The account's favourites and playlists, read whole and kept a minute:
/// the gateway lists them in one go, while the host reads them a page at
/// a time.
#[derive(Default)]
pub struct Library {
    lists: Mutex<HashMap<Tab, Read>>,
}

/// A list, and when it was read.
type Read = (Instant, Arc<Vec<Value>>);

impl Library {
    pub fn forget(&self, tab: Option<Tab>) {
        let mut lists = self.lists.lock().unwrap_or_else(|e| e.into_inner());
        match tab {
            Some(tab) => {
                lists.remove(&tab);
            }
            None => lists.clear(),
        }
    }

    /// The whole list: for tracks, entries holding only `SNG_ID`.
    pub async fn list(&self, dz: &Deezer, tab: Tab) -> Result<Arc<Vec<Value>>> {
        if let Some((at, list)) = self.lists.lock().unwrap_or_else(|e| e.into_inner()).get(&tab) {
            if at.elapsed() < LIBRARY_MAX_AGE {
                return Ok(list.clone());
            }
        }
        let list = Arc::new(read_tab(dz, tab).await?);
        // Items say whether they are favourites from these lists.
        let mut ids: HashSet<String> = list.iter().filter_map(|v| items::id_at(v, tab.id_key())).collect();
        if tab == Tab::Playlists {
            // Only the followed ones: the account's own are not "favourites".
            let me = items::user_id().map(|u| u.to_string());
            ids = list
                .iter()
                .filter(|v| items::text(v, "PARENT_USER_ID") != me)
                .filter_map(|v| items::id_at(v, "PLAYLIST_ID"))
                .collect();
        }
        items::set_favorite_ids(tab.kind(), ids);
        self.lists.lock().unwrap_or_else(|e| e.into_inner()).insert(tab, (Instant::now(), list.clone()));
        Ok(list)
    }
}

async fn read_tab(dz: &Deezer, tab: Tab) -> Result<Vec<Value>> {
    let data = |v: &Value, pointer: &str| v.pointer(pointer).and_then(Value::as_array).cloned().unwrap_or_default();
    if tab == Tab::Tracks {
        let v = dz.gw("song.getFavoriteIds", json!({"nb": LIBRARY_MAX, "start": 0, "checksum": null})).await?;
        return Ok(data(&v, "/data"));
    }
    let user = dz.user().await?;
    let name = match tab {
        Tab::Albums => "albums",
        Tab::Artists => "artists",
        _ => "playlists",
    };
    let v = dz.gw("deezer.pageProfile", json!({"USER_ID": user.id, "tab": name, "nb": LIBRARY_MAX})).await?;
    let mut list = data(&v, &format!("/TAB/{name}/data"));
    // "Loved tracks" is a playlist on Deezer; they are `library.tracks` here.
    list.retain(|p| !p.get("IS_LOVED_TRACK").is_some_and(crate::deezer::truthy));
    Ok(list)
}

/// Songs of the given ids, in their order (`song.getListData`).
async fn songs(dz: &Deezer, ids: &[String]) -> Result<Vec<Value>> {
    let batches = ids.chunks(SONGS_PER_REQUEST).map(|batch| dz.gw("song.getListData", json!({"SNG_IDS": batch})));
    let answers = try_join_all(batches).await?;
    Ok(answers.iter().flat_map(|v| v.get("data").and_then(Value::as_array).cloned().unwrap_or_default()).collect())
}

/// A page of one of the account's lists.
async fn tab_page(dz: &Deezer, library: &Library, tab: Tab, offset: u32, limit: u32) -> RpcResult {
    let list = library.list(dz, tab).await?;
    match tab {
        Tab::Tracks => {
            let from = (offset as usize).min(list.len());
            let to = (from + limit as usize).min(list.len());
            let ids: Vec<String> = list[from..to].iter().filter_map(|v| items::id_at(v, "SNG_ID")).collect();
            let songs = songs(dz, &ids).await?;
            let tracks: Vec<Value> = songs.iter().filter_map(|s| items::track(s, None)).map(|i| i.to_json()).collect();
            Ok(json!({"items": tracks, "total": list.len(), "has_more": to < list.len()}))
        }
        Tab::Albums => Ok(items::slice(&list, offset, limit, |v| items::album(v, None))),
        Tab::Artists => Ok(items::slice(&list, offset, limit, items::artist)),
        Tab::Playlists => Ok(items::slice(&list, offset, limit, items::playlist)),
    }
}

#[derive(Deserialize)]
pub struct PageParams {
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// `library.albums`, `.artists`, `.tracks`, `.playlists`.
pub async fn library(dz: &Deezer, lib: &Library, method: &str, p: PageParams) -> RpcResult {
    let tab = match method {
        "library.tracks" => Tab::Tracks,
        "library.albums" => Tab::Albums,
        "library.artists" => Tab::Artists,
        "library.playlists" => Tab::Playlists,
        _ => return Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    };
    tab_page(dz, lib, tab, p.offset, clamp(p.limit)).await
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// Flow: tracks picked for the account, new ones at each call.
pub async fn flow(dz: &Deezer) -> Result<Vec<Value>> {
    let user = dz.user().await?;
    let v = dz.gw("radio.getUserRadio", json!({"USER_ID": user.id})).await?;
    Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// The radio of an artist (public API).
async fn artist_radio(dz: &Deezer, artist_id: &str) -> Result<Vec<Value>> {
    let v = dz.api(format!("artist/{artist_id}/radio"), q(&[("limit", MIX_SIZE.to_string())])).await?;
    Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// A mix from a track (the gateway's track mix, else the artist's radio),
/// an album (its artist's) or an artist.
pub async fn mix(dz: &Deezer, seed: RadioSeed, id: &str) -> Result<Vec<Value>> {
    match seed {
        RadioSeed::Artist => artist_radio(dz, id).await,
        RadioSeed::Album => {
            let album = dz.api(format!("album/{id}"), vec![]).await?;
            let artist = album.get("artist").and_then(|a| items::id_at(a, "id"));
            artist_radio(dz, &artist.ok_or_else(|| anyhow::anyhow!("album {id} has no artist"))?).await
        }
        RadioSeed::Track => {
            let args = json!({"SNG_ID": id, "START_WITH_INPUT_TRACK": "false"});
            match dz.gw("song.getSearchTrackMix", args).await {
                Ok(v) if v.get("data").and_then(Value::as_array).is_some_and(|d| !d.is_empty()) => {
                    Ok(v["data"].as_array().cloned().unwrap_or_default())
                }
                Err(e) if e.chain().any(|c| c.downcast_ref::<SignedOut>().is_some()) => Err(e),
                other => {
                    if let Err(e) = other {
                        tracing::debug!("track mix {id}: {e:#}; the artist's radio instead");
                    }
                    let track = dz.api(format!("track/{id}"), vec![]).await?;
                    let artist = track.get("artist").and_then(|a| items::id_at(a, "id"));
                    artist_radio(dz, &artist.ok_or_else(|| anyhow::anyhow!("track {id} has no artist"))?).await
                }
            }
        }
    }
}

pub async fn list(dz: &Deezer, lib: &Library, p: ListParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let page = match parse_ref(&p.reference)? {
        Ref::Favorites => {
            let items: Vec<Value> = [Ref::FavTracks, Ref::FavAlbums, Ref::FavArtists].into_iter().map(folder).collect();
            json!({"items": items, "total": 3, "has_more": false})
        }
        Ref::FavTracks => return tab_page(dz, lib, Tab::Tracks, offset, limit).await,
        Ref::FavAlbums => return tab_page(dz, lib, Tab::Albums, offset, limit).await,
        Ref::FavArtists => return tab_page(dz, lib, Tab::Artists, offset, limit).await,
        Ref::MyPlaylists => return tab_page(dz, lib, Tab::Playlists, offset, limit).await,
        Ref::Flow => {
            // New tracks at each call: one page, never more.
            let tracks = if offset == 0 { flow(dz).await? } else { vec![] };
            let mut page = items::slice(&tracks, 0, limit, |t| items::track(t, None));
            page["has_more"] = false.into();
            page
        }
        Ref::Charts => {
            let items: Vec<Value> = items::CHARTS.iter().map(|k| folder(Ref::Chart(k.to_string()))).collect();
            json!({"items": items, "total": items.len(), "has_more": false})
        }
        Ref::Chart(kind) => {
            let v = dz.api(format!("chart/0/{kind}"), paged(offset, limit)).await?;
            match kind.as_str() {
                "tracks" => items::page(&v, offset, limit, |t| items::track(t, None)),
                "albums" => items::page(&v, offset, limit, |a| items::album(a, None)),
                "artists" => items::page(&v, offset, limit, items::artist),
                _ => items::page(&v, offset, limit, items::playlist),
            }
        }
        Ref::Releases => {
            let v = dz.api("editorial/0/releases", paged(offset, limit)).await?;
            items::page(&v, offset, limit, |a| items::album(a, None))
        }
        Ref::Genres => {
            let v = dz.api("genre", vec![]).await?;
            // Genre 0 is "All".
            let genres: Vec<Value> = v
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|g| items::id_at(g, "id").is_some())
                .cloned()
                .collect();
            items::slice(&genres, offset, limit, items::genre)
        }
        Ref::Genre(id) => {
            let v = dz.api(format!("editorial/{id}/releases"), paged(offset, limit)).await?;
            items::page(&v, offset, limit, |a| items::album(a, None))
        }
        Ref::Album(id) => {
            let (album, tracks) =
                tokio::try_join!(dz.api(format!("album/{id}"), vec![]), dz.api(format!("album/{id}/tracks"), paged(offset, limit)))?;
            items::page(&tracks, offset, limit, |t| items::track(t, Some(&album)))
        }
        Ref::Artist(id) => {
            let (artist, albums) =
                tokio::try_join!(dz.api(format!("artist/{id}"), vec![]), dz.api(format!("artist/{id}/albums"), paged(offset, limit)))?;
            items::page(&albums, offset, limit, |a| items::album(a, Some(&artist)))
        }
        Ref::Playlist(id) => {
            let v = dz.gw("playlist.getSongs", json!({"PLAYLIST_ID": id, "start": offset, "nb": limit})).await?;
            let data = v.get("data").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            // A gateway that ignored `start` sends the whole list.
            let skip = if data.len() > limit as usize { offset as usize } else { 0 };
            let items: Vec<Value> = data
                .iter()
                .skip(skip)
                .take(limit as usize)
                .filter_map(|t| {
                    let mut item = items::track(t, None)?;
                    item.entry_id = items::id_at(t, "SNG_ID");
                    Some(item.to_json())
                })
                .collect();
            let total = crate::deezer::num(v.get("total")).unwrap_or((offset as usize + data.len()) as u64);
            let seen = u64::from(offset) + data.len().saturating_sub(skip).min(limit as usize) as u64;
            json!({"items": items, "total": total, "has_more": seen < total})
        }
        Ref::Radio(seed, id) => {
            let tracks = mix(dz, seed, &id).await?;
            items::slice(&tracks, offset, limit, |t| items::track(t, None))
        }
        Ref::Similar(id) => {
            let v = dz.api(format!("artist/{id}/related"), paged(offset, limit)).await?;
            items::page(&v, offset, limit, items::artist)
        }
        Ref::Track(_) => return Err(RpcError::invalid_params("a track is not browsable")),
    };
    Ok(page)
}

#[derive(Deserialize)]
pub struct RadioNextParams {
    seed: String,
    #[serde(default)]
    exclude: Vec<String>,
    limit: Option<u32>,
}

/// `radio.next {seed, exclude, limit}`: playable tracks close to the seed
/// (a track, album, artist, mix or Flow), without those in `exclude`.
pub async fn radio_next(dz: &Deezer, p: RadioNextParams) -> RpcResult {
    let seed = parse_ref(&p.seed)?;
    let (tracks, seed_track) = match &seed {
        Ref::Track(id) => (mix(dz, RadioSeed::Track, id).await?, Some(p.seed.as_str())),
        Ref::Album(id) => (mix(dz, RadioSeed::Album, id).await?, None),
        Ref::Artist(id) => (mix(dz, RadioSeed::Artist, id).await?, None),
        Ref::Radio(kind, id) => (mix(dz, *kind, id).await?, None),
        Ref::Flow => (flow(dz).await?, None),
        _ => return Err(RpcError::invalid_params(format!("{} cannot seed a radio", p.seed))),
    };
    let limit = p.limit.unwrap_or(20).clamp(1, 50) as usize;
    let exclude: HashSet<&str> = p.exclude.iter().map(String::as_str).collect();
    let items: Vec<Value> = tracks
        .iter()
        .filter_map(|t| items::track(t, None))
        .filter(|t| t.playable && !exclude.contains(t.reference.as_str()) && Some(t.reference.as_str()) != seed_track)
        .take(limit)
        .map(|t| t.to_json())
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

const SEARCH_KINDS: [&str; 4] = ["track", "album", "artist", "playlist"];

pub async fn search(dz: &Deezer, p: SearchParams) -> RpcResult {
    let query = p.query.trim();
    if query.is_empty() {
        return Err(RpcError::invalid_params("empty query"));
    }
    let kinds: Vec<&str> = match &p.kinds {
        Some(kinds) => kinds.iter().map(String::as_str).filter(|k| SEARCH_KINDS.contains(k)).collect(),
        None => SEARCH_KINDS.to_vec(),
    };
    let limit = clamp(p.limit);
    let offset = p.offset;
    let requests = kinds.iter().map(|&kind| {
        let mut query_params = paged(offset, limit);
        query_params.push(("q", query.to_string()));
        async move {
            let v = dz.api(format!("search/{kind}"), query_params).await?;
            let mut group = match kind {
                "track" => items::page(&v, offset, limit, |t| items::track(t, None)),
                "album" => items::page(&v, offset, limit, |a| items::album(a, None)),
                "artist" => items::page(&v, offset, limit, items::artist),
                _ => items::page(&v, offset, limit, items::playlist),
            };
            group["kind"] = kind.into();
            anyhow::Ok(group)
        }
    });
    let groups = try_join_all(requests).await?;
    Ok(json!({"groups": groups}))
}

pub async fn get(dz: &Deezer, reference: &str) -> RpcResult {
    let r = parse_ref(reference)?;
    let item = match &r {
        Ref::Track(id) => items::track(&dz.api(format!("track/{id}"), vec![]).await?, None),
        Ref::Album(id) => items::album(&dz.api(format!("album/{id}"), vec![]).await?, None),
        Ref::Artist(id) => items::artist(&dz.api(format!("artist/{id}"), vec![]).await?),
        Ref::Playlist(id) => items::playlist(&dz.gw("playlist.getData", json!({"PLAYLIST_ID": id})).await?),
        Ref::Genre(id) => items::genre(&dz.api(format!("genre/{id}"), vec![]).await?),
        Ref::Flow => Some(items::flow()),
        _ => return Ok(folder(r)),
    };
    item.map(|i| i.to_json()).ok_or_else(|| RpcError::not_found(format!("{reference}: unreadable answer")))
}

pub async fn set_favorite(dz: &Deezer, lib: &Library, reference: &str, on: bool) -> RpcResult {
    let (tab, method, args, id) = match parse_ref(reference)? {
        Ref::Track(id) => {
            (Tab::Tracks, if on { "favorite_song.add" } else { "favorite_song.remove" }, json!({"SNG_ID": id}), id)
        }
        Ref::Album(id) => (Tab::Albums, if on { "album.addFavorite" } else { "album.deleteFavorite" }, json!({"ALB_ID": id}), id),
        Ref::Artist(id) => (Tab::Artists, if on { "artist.addFavorite" } else { "artist.deleteFavorite" }, json!({"ART_ID": id}), id),
        Ref::Playlist(id) => {
            let args = if on { json!({"PARENT_PLAYLIST_ID": id}) } else { json!({"PLAYLIST_ID": id}) };
            (Tab::Playlists, if on { "playlist.addFavorite" } else { "playlist.deleteFavorite" }, args, id)
        }
        _ => return Err(RpcError::invalid_params(format!("{reference} cannot be a favourite"))),
    };
    dz.gw(method, args).await?;
    lib.forget(Some(tab));
    items::note_favorite(tab.kind(), &id, on);
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deezer::Endpoints;

    /// Against Deezer: `DEEZSRC_ARL=<cookie> cargo test live_account --
    /// --ignored --nocapture`. Reads the account's lists, Flow, a search, a
    /// track mix and lyrics; prints counts only.
    #[tokio::test]
    #[ignore]
    async fn live_account() {
        let arl = std::env::var("DEEZSRC_ARL").expect("DEEZSRC_ARL");
        let dz = Deezer::new(Endpoints::new(None, None, None)).with_arl(&arl);
        let user = dz.user().await.unwrap();
        items::set_user(Some(user.id));
        let lib = Library::default();
        for tab in Tab::ALL {
            let page = tab_page(&dz, &lib, tab, 0, 5).await.unwrap();
            println!("{}s: {} in all, first page {}", tab.kind(), page["total"], page["items"].as_array().unwrap().len());
        }
        let flow = flow(&dz).await.unwrap();
        println!("Flow: {} tracks", flow.len());
        let found = dz.api("search/track", q(&[("q", "daft punk".into()), ("limit", "1".into())])).await.unwrap();
        let track = found.pointer("/data/0/id").map(|v| v.to_string()).expect("a track");
        let mix = mix(&dz, RadioSeed::Track, &track).await.unwrap();
        println!("track {track}: mix of {} tracks", mix.len());
        match super::super::lyrics::get(&dz, &format!("track/{track}")).await {
            Ok(l) => println!("lyrics: synced {}, plain {}", l.get("synced").is_some(), l.get("plain").is_some()),
            Err(e) => println!("lyrics: {} {}", e.code, e.message),
        }
    }
}
