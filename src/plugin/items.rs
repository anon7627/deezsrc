//! Protocol `ref`s and conversion of Deezer objects into protocol items.
//!
//! Deezer answers in two shapes: the public API's (`{"id", "title",
//! "artist": {…}}`) and the gateway's (`{"SNG_ID", "SNG_TITLE", "ART_NAME",
//! …}`, numbers often in strings). Every conversion reads both.
//!
//! Refs: `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<id>` for
//! catalogue entries; `fav`, `fav/tracks`, `fav/albums`, `fav/artists`,
//! `my/playlists`, `flow`, `charts`, `charts/<kind>`, `releases`, `genres`
//! and `genre/<id>` for sections; `radio/<track|album|artist>/<id>` (mixes)
//! and `similar/<artist id>` for related content.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};

use crate::deezer::{num, truthy};

/// Chart lists offered under `charts`.
pub const CHARTS: [&str; 4] = ["tracks", "albums", "artists", "playlists"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Favorites,
    FavTracks,
    FavAlbums,
    FavArtists,
    MyPlaylists,
    /// The account's endless personal mix.
    Flow,
    Charts,
    Chart(String),
    Releases,
    Genres,
    Genre(String),
    /// `radio/<seed kind>/<id>`: a mix of tracks close to a track, album or
    /// artist.
    Radio(RadioSeed, String),
    /// Artists close to an artist.
    Similar(String),
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

/// Deezer ids are numbers; the account's own uploads have negative ones.
pub fn valid_id(id: &str) -> bool {
    let digits = id.strip_prefix('-').unwrap_or(id);
    !digits.is_empty() && digits.len() <= 20 && digits.chars().all(|c| c.is_ascii_digit())
}

impl Ref {
    pub fn parse(s: &str) -> Option<Self> {
        let r = match s {
            "fav" => Ref::Favorites,
            "fav/tracks" => Ref::FavTracks,
            "fav/albums" => Ref::FavAlbums,
            "fav/artists" => Ref::FavArtists,
            "my/playlists" => Ref::MyPlaylists,
            "flow" => Ref::Flow,
            "charts" => Ref::Charts,
            "releases" => Ref::Releases,
            "genres" => Ref::Genres,
            _ => {
                let (kind, id) = s.split_once('/')?;
                let id = id.to_string();
                match kind {
                    "charts" if CHARTS.contains(&id.as_str()) => Ref::Chart(id),
                    "radio" => {
                        let (seed, id) = id.split_once('/')?;
                        let seed = match seed {
                            "track" => RadioSeed::Track,
                            "album" => RadioSeed::Album,
                            "artist" => RadioSeed::Artist,
                            _ => return None,
                        };
                        valid_id(id).then(|| Ref::Radio(seed, id.to_string()))?
                    }
                    _ if !valid_id(&id) => return None,
                    "track" => Ref::Track(id),
                    "album" => Ref::Album(id),
                    "artist" => Ref::Artist(id),
                    "playlist" => Ref::Playlist(id),
                    "genre" => Ref::Genre(id),
                    "similar" => Ref::Similar(id),
                    _ => return None,
                }
            }
        };
        Some(r)
    }
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Track(id) => write!(f, "track/{id}"),
            Ref::Album(id) => write!(f, "album/{id}"),
            Ref::Artist(id) => write!(f, "artist/{id}"),
            Ref::Playlist(id) => write!(f, "playlist/{id}"),
            Ref::Favorites => f.write_str("fav"),
            Ref::FavTracks => f.write_str("fav/tracks"),
            Ref::FavAlbums => f.write_str("fav/albums"),
            Ref::FavArtists => f.write_str("fav/artists"),
            Ref::MyPlaylists => f.write_str("my/playlists"),
            Ref::Flow => f.write_str("flow"),
            Ref::Charts => f.write_str("charts"),
            Ref::Chart(kind) => write!(f, "charts/{kind}"),
            Ref::Releases => f.write_str("releases"),
            Ref::Genres => f.write_str("genres"),
            Ref::Genre(id) => write!(f, "genre/{id}"),
            Ref::Radio(seed, id) => write!(f, "radio/{}/{id}", seed.as_str()),
            Ref::Similar(id) => write!(f, "similar/{id}"),
        }
    }
}

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
    pub track_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    pub playable: bool,
    pub browsable: bool,
    /// Tracks: their album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    /// Tracks and albums: their main artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
    /// In the account's favourites; absent when not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Tracks of a playlist: their entry in it (the track id: Deezer keeps
    /// a track once per playlist).
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

/// Ids of the account's favourites, per kind; `None` until read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FavoriteIds {
    pub tracks: Option<HashSet<String>>,
    pub albums: Option<HashSet<String>>,
    pub artists: Option<HashSet<String>>,
    pub playlists: Option<HashSet<String>>,
}

impl FavoriteIds {
    fn set(&mut self, kind: &str) -> Option<&mut Option<HashSet<String>>> {
        match kind {
            "track" => Some(&mut self.tracks),
            "album" => Some(&mut self.albums),
            "artist" => Some(&mut self.artists),
            "playlist" => Some(&mut self.playlists),
            _ => None,
        }
    }
}

/// The signed-in account, as far as item conversion needs it. One account
/// per plugin process, hence a global.
#[derive(Default)]
struct Account {
    user_id: Option<u64>,
    favorites: Option<FavoriteIds>,
    /// The host's language, for labels.
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

/// Deezer user id of the signed-in account.
pub fn user_id() -> Option<u64> {
    account().user_id
}

/// Deezer user id of the signed-in account (`None`: signed out).
pub fn set_user(user_id: Option<u64>) {
    let mut a = account();
    if a.user_id != user_id {
        a.favorites = None;
    }
    a.user_id = user_id;
}

/// Replace the ids of one kind (`track`, `album`, `artist`, `playlist`).
pub fn set_favorite_ids(kind: &str, ids: HashSet<String>) {
    let mut a = account();
    let favorites = a.favorites.get_or_insert_with(FavoriteIds::default);
    if let Some(set) = favorites.set(kind) {
        *set = Some(ids);
    }
}

/// After `favorites.set`.
pub fn note_favorite(kind: &str, id: &str, on: bool) {
    if let Some(Some(set)) = account().favorites.as_mut().and_then(|f| f.set(kind)) {
        if on {
            set.insert(id.to_string());
        } else {
            set.remove(id);
        }
    }
}

/// Whether `id` of `kind` is a favourite; `None` until the ids are loaded.
fn is_favorite(kind: &str, id: &str) -> Option<bool> {
    let mut a = account();
    let set = a.favorites.as_mut()?.set(kind)?.as_ref()?;
    Some(set.contains(id))
}

fn owned_by_account(owner_id: Option<u64>) -> Option<bool> {
    let user = account().user_id?;
    Some(owner_id == Some(user))
}

fn action(id: &'static str, label: String, r: Ref, kind: &'static str) -> Action {
    Action { id, label, reference: r.to_string(), kind }
}

impl Item {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("item serialises")
    }
}

/// A string (or a number) at `key`, trimmed, not empty.
pub fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An id at `key` (a number or a numeric string), `None` for 0.
pub fn id_at(v: &Value, key: &str) -> Option<String> {
    text(v, key).filter(|id| valid_id(id) && id != "0")
}

fn number(v: &Value, key: &str) -> Option<u64> {
    num(v.get(key))
}

/// Whether `v` comes from the gateway (upper-case keys).
fn is_gw(v: &Value) -> bool {
    v.get("SNG_ID").is_some() || v.get("ALB_ID").is_some() || v.get("ART_ID").is_some() || v.get("PLAYLIST_ID").is_some()
}

/// Image URL from a picture hash: `kind` is `cover`, `artist`,
/// `playlist` or `user`.
pub fn picture(kind: &str, hash: Option<String>) -> Option<String> {
    let hash = hash.filter(|h| !h.is_empty() && h.len() <= 64 && h.chars().all(|c| c.is_ascii_hexdigit()))?;
    let kind = if matches!(kind, "cover" | "artist" | "playlist" | "user") { kind } else { "cover" };
    Some(format!("https://cdn-images.dzcdn.net/images/{kind}/{hash}/500x500-000000-80-0-0.jpg"))
}

/// Largest picture of a public API object (`<prefix>_xl`, `_big`, then the
/// hash in `md5_image`).
fn public_picture(v: &Value, prefix: &str, kind: &str) -> Option<String> {
    ["xl", "big", "medium"]
        .iter()
        .find_map(|size| text(v, &format!("{prefix}_{size}")).filter(|u| u.starts_with("https://") || u.starts_with("http://")))
        .or_else(|| picture(kind, text(v, "md5_image")))
}

fn year_of(date: Option<String>) -> Option<u32> {
    date?.get(..4)?.parse().ok().filter(|&y| y > 0)
}

fn join(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<&str> = parts.iter().flatten().copied().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn ms(seconds: Option<u64>) -> Option<u64> {
    seconds.filter(|&s| s > 0).map(|s| s * 1000)
}

/// Release year of a gateway album or song.
fn gw_year(v: &Value) -> Option<u32> {
    ["ORIGINAL_RELEASE_DATE", "PHYSICAL_RELEASE_DATE", "DIGITAL_RELEASE_DATE"]
        .iter()
        .find_map(|k| year_of(text(v, k)).filter(|&y| y > 1000))
}

/// Main artist of a gateway object: `ART_NAME`/`ART_ID`, else the first of
/// `ARTISTS`.
fn gw_artist(v: &Value) -> (Option<String>, Option<String>) {
    let first = v.get("ARTISTS").and_then(Value::as_array).and_then(|a| a.first());
    let name = text(v, "ART_NAME").or_else(|| first.and_then(|a| text(a, "ART_NAME")));
    let id = id_at(v, "ART_ID").or_else(|| first.and_then(|a| id_at(a, "ART_ID")));
    (name, id)
}

fn track_actions(id: &str) -> Vec<Action> {
    vec![action("radio", tr("Track mix", "Mix de ce titre"), Ref::Radio(RadioSeed::Track, id.into()), "play")]
}

/// A track; `album` supplies what tracks listed inside an album omit.
pub fn track(v: &Value, album: Option<&Value>) -> Option<Item> {
    if is_gw(v) {
        return gw_track(v);
    }
    let id = id_at(v, "id")?;
    let album = v.get("album").filter(|a| a.is_object()).or(album);
    let album_title = album.and_then(|a| text(a, "title"));
    let album_artist = album.and_then(|a| a.get("artist")).and_then(|a| text(a, "name"));
    let artist = v.get("artist").and_then(|a| text(a, "name")).or_else(|| album_artist.clone());
    let artist_ref = v.get("artist").and_then(|a| id_at(a, "id")).or_else(|| album?.get("artist").and_then(|a| id_at(a, "id")));
    let date = text(v, "release_date").or_else(|| album.and_then(|a| text(a, "release_date")));
    Some(Item {
        reference: Ref::Track(id.clone()).to_string(),
        kind: "track",
        title: text(v, "title").unwrap_or_else(|| "?".into()),
        subtitle: join(&[artist.as_deref(), album_title.as_deref()]),
        artist,
        album: album_title,
        album_artist,
        track_no: number(v, "track_position"),
        disc_no: number(v, "disk_number"),
        year: year_of(date),
        genre: album.and_then(|a| a.pointer("/genres/data/0")).and_then(|g| text(g, "name")),
        duration_ms: ms(number(v, "duration")),
        art: album.and_then(|a| public_picture(a, "cover", "cover")).or_else(|| picture("cover", text(v, "md5_image"))),
        playable: v.get("readable").is_none_or(truthy),
        album_ref: album.and_then(|a| id_at(a, "id")).map(|a| Ref::Album(a).to_string()),
        artist_ref: artist_ref.map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("track", &id),
        actions: track_actions(&id),
        ..Default::default()
    })
}

/// Whether the gateway says the account may stream a song: some right
/// is on, or a fallback version exists.
pub fn gw_playable(v: &Value) -> bool {
    let Some(rights) = v.get("RIGHTS").and_then(Value::as_object) else { return true };
    let fallback = v.pointer("/FALLBACK/SNG_ID").is_some();
    fallback || rights.iter().any(|(k, on)| k.starts_with("STREAM_") && k.ends_with("_AVAILABLE") && truthy(on))
}

fn gw_track(v: &Value) -> Option<Item> {
    let id = id_at(v, "SNG_ID")?;
    let title = text(v, "SNG_TITLE").unwrap_or_else(|| "?".into());
    let title = match text(v, "VERSION") {
        Some(version) if !title.contains(&version) => format!("{title} {version}"),
        _ => title,
    };
    let (artist, artist_id) = gw_artist(v);
    let album = text(v, "ALB_TITLE");
    Some(Item {
        reference: Ref::Track(id.clone()).to_string(),
        kind: "track",
        title,
        subtitle: join(&[artist.as_deref(), album.as_deref()]),
        album_artist: None,
        artist,
        album,
        track_no: number(v, "TRACK_NUMBER"),
        disc_no: number(v, "DISK_NUMBER"),
        year: gw_year(v),
        duration_ms: ms(number(v, "DURATION")),
        art: picture("cover", text(v, "ALB_PICTURE")),
        playable: gw_playable(v),
        album_ref: id_at(v, "ALB_ID").map(|a| Ref::Album(a).to_string()),
        artist_ref: artist_id.map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("track", &id),
        actions: track_actions(&id),
        ..Default::default()
    })
}

fn album_actions(id: &str) -> Vec<Action> {
    vec![action("radio", tr("Album mix", "Mix de cet album"), Ref::Radio(RadioSeed::Album, id.into()), "play")]
}

/// An album; `artist` supplies the artist that an artist's album list
/// leaves out.
pub fn album(v: &Value, artist: Option<&Value>) -> Option<Item> {
    if is_gw(v) {
        return gw_album(v);
    }
    let id = id_at(v, "id")?;
    let artist_obj = v.get("artist").filter(|a| a.is_object()).or(artist);
    let artist = artist_obj.and_then(|a| text(a, "name"));
    let year = year_of(text(v, "release_date"));
    let year_s = year.map(|y| y.to_string());
    Some(Item {
        kind: "album",
        title: text(v, "title").unwrap_or_else(|| "?".into()),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        genre: v.pointer("/genres/data/0").and_then(|g| text(g, "name")),
        duration_ms: ms(number(v, "duration")),
        track_count: number(v, "nb_tracks").filter(|&n| n > 0),
        art: public_picture(v, "cover", "cover"),
        playable: v.get("available").is_none_or(truthy),
        browsable: true,
        artist_ref: artist_obj.and_then(|a| id_at(a, "id")).map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("album", &id),
        actions: album_actions(&id),
        reference: Ref::Album(id).to_string(),
        ..Default::default()
    })
}

fn gw_album(v: &Value) -> Option<Item> {
    let id = id_at(v, "ALB_ID")?;
    let (artist, artist_id) = gw_artist(v);
    let year = gw_year(v);
    let year_s = year.map(|y| y.to_string());
    let count = number(v, "NUMBER_TRACK").or_else(|| number(v, "NB_SONG")).filter(|&n| n > 0);
    Some(Item {
        kind: "album",
        title: text(v, "ALB_TITLE").unwrap_or_else(|| "?".into()),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        track_count: count,
        art: picture("cover", text(v, "ALB_PICTURE")),
        playable: true,
        browsable: true,
        artist_ref: artist_id.map(|a| Ref::Artist(a).to_string()),
        favorite: is_favorite("album", &id),
        actions: album_actions(&id),
        reference: Ref::Album(id).to_string(),
        ..Default::default()
    })
}

pub fn artist(v: &Value) -> Option<Item> {
    let (id, name, art) = if is_gw(v) {
        (id_at(v, "ART_ID")?, text(v, "ART_NAME"), picture("artist", text(v, "ART_PICTURE")))
    } else {
        (id_at(v, "id")?, text(v, "name"), public_picture(v, "picture", "artist"))
    };
    Some(Item {
        kind: "artist",
        title: name.unwrap_or_else(|| "?".into()),
        art,
        browsable: true,
        favorite: is_favorite("artist", &id),
        actions: vec![
            action("radio", tr("Artist mix", "Mix de cet artiste"), Ref::Radio(RadioSeed::Artist, id.clone()), "play"),
            action("similar", tr("Similar artists", "Artistes similaires"), Ref::Similar(id.clone()), "browse"),
        ],
        reference: Ref::Artist(id).to_string(),
        ..Default::default()
    })
}

pub fn playlist(v: &Value) -> Option<Item> {
    if is_gw(v) {
        let id = id_at(v, "PLAYLIST_ID")?;
        let kind = text(v, "PICTURE_TYPE").unwrap_or_else(|| "playlist".into());
        return Some(Item {
            kind: "playlist",
            title: text(v, "TITLE").unwrap_or_else(|| "?".into()),
            subtitle: text(v, "PARENT_USERNAME"),
            duration_ms: ms(number(v, "DURATION")),
            track_count: number(v, "NB_SONG"),
            art: picture(&kind, text(v, "PLAYLIST_PICTURE")),
            browsable: true,
            editable: owned_by_account(number(v, "PARENT_USER_ID")),
            favorite: is_favorite("playlist", &id),
            reference: Ref::Playlist(id).to_string(),
            ..Default::default()
        });
    }
    let id = id_at(v, "id")?;
    let owner = v.get("creator").or_else(|| v.get("user")).filter(|o| o.is_object());
    Some(Item {
        kind: "playlist",
        title: text(v, "title").unwrap_or_else(|| "?".into()),
        subtitle: owner.and_then(|o| text(o, "name")),
        duration_ms: ms(number(v, "duration")),
        track_count: number(v, "nb_tracks"),
        art: public_picture(v, "picture", "playlist"),
        browsable: true,
        editable: owned_by_account(owner.and_then(|o| number(o, "id"))),
        favorite: is_favorite("playlist", &id),
        reference: Ref::Playlist(id).to_string(),
        ..Default::default()
    })
}

/// A genre (`/genre` entry): a folder of its new releases.
pub fn genre(v: &Value) -> Option<Item> {
    let id = id_at(v, "id")?;
    Some(Item {
        art: public_picture(v, "picture", "misc"),
        ..folder(&Ref::Genre(id), &text(v, "name").unwrap_or_else(|| "?".into()))
    })
}

/// Flow, the account's endless mix, shown as a playlist.
pub fn flow() -> Item {
    Item {
        reference: Ref::Flow.to_string(),
        kind: "playlist",
        title: "Flow".into(),
        subtitle: Some(tr("Your endless personal mix", "Votre mix personnel sans fin")),
        browsable: true,
        ..Default::default()
    }
}

/// A section: a folder the host lists under the plugin's name.
pub fn folder(r: &Ref, title: &str) -> Item {
    Item { reference: r.to_string(), kind: "folder", title: title.to_string(), browsable: true, ..Default::default() }
}

/// `{items, total, has_more}` from a public API list (`{data, total,
/// next}`), the page at `offset` of which Deezer was asked for.
pub fn page(v: &Value, offset: u32, limit: u32, map: impl Fn(&Value) -> Option<Item>) -> Value {
    let raw = v.get("data").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let items: Vec<Value> = raw.iter().take(limit as usize).filter_map(|v| map(v).map(|i| i.to_json())).collect();
    let total = number(v, "total");
    let seen = u64::from(offset) + raw.len().min(limit as usize) as u64;
    let has_more = match total {
        Some(total) => seen < total,
        None => v.get("next").is_some_and(Value::is_string) || raw.len() > limit as usize,
    };
    let mut out = json!({"items": items, "has_more": has_more});
    if let Some(total) = total {
        out["total"] = total.into();
    }
    out
}

/// The page at `offset` of a list read whole.
pub fn slice(all: &[Value], offset: u32, limit: u32, map: impl Fn(&Value) -> Option<Item>) -> Value {
    let from = (offset as usize).min(all.len());
    let to = (from + limit as usize).min(all.len());
    let items: Vec<Value> = all[from..to].iter().filter_map(|v| map(v).map(|i| i.to_json())).collect();
    json!({"items": items, "total": all.len(), "has_more": to < all.len()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_round_trip_and_reject_junk() {
        for s in [
            "track/3135556", "track/-12", "album/302127", "artist/27", "playlist/908622995", "fav", "fav/tracks", "fav/albums",
            "fav/artists", "my/playlists", "flow", "charts", "charts/albums", "releases", "genres", "genre/132",
            "radio/track/1", "radio/album/2", "radio/artist/3", "similar/27",
        ] {
            assert_eq!(Ref::parse(s).unwrap().to_string(), s);
        }
        for s in ["", "track/", "track/1/2", "album/abc", "charts/podcasts", "radio/1", "radio/label/1", "radio/track/x", "fav/stuff",
                  "track/--1", "genre/1.5"] {
            assert_eq!(Ref::parse(s), None, "{s}");
        }
    }

    /// `GET /track/3135556` (trimmed).
    const PUBLIC_TRACK: &str = r#"{"id":3135556,"readable":true,"title":"Harder, Better, Faster, Stronger",
        "title_short":"Harder, Better, Faster, Stronger","isrc":"GBDUW0000059","duration":224,"track_position":4,
        "disk_number":1,"release_date":"2005-01-24","gain":-12.4,
        "artist":{"id":27,"name":"Daft Punk","picture_xl":"https://e-cdns-images.dzcdn.net/images/artist/f2bc/1000x1000.jpg"},
        "album":{"id":302127,"title":"Discovery","cover_xl":"https://e-cdns-images.dzcdn.net/images/cover/2e01/1000x1000.jpg",
        "md5_image":"2e018122cb56986277102d2041a592c8","release_date":"2001-03-07"},"type":"track"}"#;

    /// `song.getData` (trimmed).
    const GW_SONG: &str = r#"{"SNG_ID":"3135556","SNG_TITLE":"Harder, Better, Faster, Stronger","VERSION":"(Live)",
        "ART_ID":"27","ART_NAME":"Daft Punk","ALB_ID":"302127","ALB_TITLE":"Discovery",
        "ALB_PICTURE":"2e018122cb56986277102d2041a592c8","DURATION":"224","TRACK_NUMBER":"4","DISK_NUMBER":"1",
        "PHYSICAL_RELEASE_DATE":"2001-03-07","GAIN":"-12.4","FILESIZE_FLAC":"0","FILESIZE_MP3_320":"8963413",
        "RIGHTS":{"STREAM_ADS_AVAILABLE":true,"STREAM_SUB_AVAILABLE":true},"__TYPE__":"song"}"#;

    #[test]
    fn tracks_in_both_shapes() {
        let t = track(&serde_json::from_str(PUBLIC_TRACK).unwrap(), None).unwrap();
        assert_eq!(t.reference, "track/3135556");
        assert_eq!(t.title, "Harder, Better, Faster, Stronger");
        assert_eq!(t.subtitle.as_deref(), Some("Daft Punk · Discovery"));
        assert_eq!((t.track_no, t.disc_no, t.year, t.duration_ms), (Some(4), Some(1), Some(2005), Some(224_000)));
        assert_eq!(t.art.as_deref(), Some("https://e-cdns-images.dzcdn.net/images/cover/2e01/1000x1000.jpg"));
        assert_eq!((t.album_ref.as_deref(), t.artist_ref.as_deref()), (Some("album/302127"), Some("artist/27")));
        assert!(t.playable && !t.browsable);
        assert_eq!(t.actions[0].reference, "radio/track/3135556");

        let g = track(&serde_json::from_str(GW_SONG).unwrap(), None).unwrap();
        assert_eq!(g.title, "Harder, Better, Faster, Stronger (Live)");
        assert_eq!((g.track_no, g.year, g.duration_ms), (Some(4), Some(2001), Some(224_000)));
        assert_eq!(g.art.as_deref(), Some("https://cdn-images.dzcdn.net/images/cover/2e018122cb56986277102d2041a592c8/500x500-000000-80-0-0.jpg"));
        assert_eq!((g.album_ref.as_deref(), g.artist_ref.as_deref()), (Some("album/302127"), Some("artist/27")));
        assert!(g.playable);

        let blocked = json!({"SNG_ID": "9", "RIGHTS": {"STREAM_ADS_AVAILABLE": false, "STREAM_SUB_AVAILABLE": false}});
        assert!(!track(&blocked, None).unwrap().playable);
        let with_fallback = json!({"SNG_ID": "9", "RIGHTS": {}, "FALLBACK": {"SNG_ID": "10"}});
        assert!(track(&with_fallback, None).unwrap().playable, "the fallback plays");
        assert!(!track(&json!({"id": 1, "readable": false}), None).unwrap().playable);
        assert_eq!(picture("cover", Some("../../etc".into())), None, "only a hex hash makes a URL");
    }

    #[test]
    fn album_listing_gives_tracks_their_album() {
        let album_json = json!({"id": 302127, "title": "Discovery", "release_date": "2001-03-07", "nb_tracks": 14,
            "cover_xl": "https://x/c.jpg", "genres": {"data": [{"id": 113, "name": "Dance"}]}, "artist": {"id": 27, "name": "Daft Punk"}});
        let t = track(&json!({"id": 1, "title": "One More Time", "duration": 320, "track_position": 1, "disk_number": 1,
            "artist": {"id": 27, "name": "Daft Punk"}}), Some(&album_json)).unwrap();
        assert_eq!((t.album.as_deref(), t.art.as_deref(), t.genre.as_deref()), (Some("Discovery"), Some("https://x/c.jpg"), Some("Dance")));
        let a = album(&album_json, None).unwrap();
        assert_eq!(a.subtitle.as_deref(), Some("Daft Punk · 2001"));
        assert_eq!((a.track_count, a.browsable), (Some(14), true));

        // An artist's albums come without the artist.
        let bare = json!({"id": 5, "title": "Homework", "release_date": "1997-01-20", "cover_big": "https://x/h.jpg"});
        let a = album(&bare, Some(&json!({"id": 27, "name": "Daft Punk"}))).unwrap();
        assert_eq!((a.artist.as_deref(), a.artist_ref.as_deref(), a.art.as_deref()), (Some("Daft Punk"), Some("artist/27"), Some("https://x/h.jpg")));

        let g = album(&json!({"ALB_ID": "5", "ALB_TITLE": "Homework", "ART_NAME": "Daft Punk", "ART_ID": "27",
            "ALB_PICTURE": "abc123", "DIGITAL_RELEASE_DATE": "1997-01-20", "NUMBER_TRACK": "16"}), None).unwrap();
        assert_eq!((g.reference.as_str(), g.year, g.track_count), ("album/5", Some(1997), Some(16)));
    }

    #[test]
    fn artists_playlists_and_pages() {
        let a = artist(&json!({"id": 27, "name": "Daft Punk", "picture_xl": "https://x/a.jpg"})).unwrap();
        assert_eq!((a.title.as_str(), a.art.as_deref()), ("Daft Punk", Some("https://x/a.jpg")));
        let ids: Vec<&str> = a.actions.iter().map(|x| x.reference.as_str()).collect();
        assert_eq!(ids, ["radio/artist/27", "similar/27"]);
        let g = artist(&json!({"ART_ID": "27", "ART_NAME": "Daft Punk", "ART_PICTURE": "f2bc"})).unwrap();
        assert_eq!(g.art.as_deref(), Some("https://cdn-images.dzcdn.net/images/artist/f2bc/500x500-000000-80-0-0.jpg"));

        let p = playlist(&json!({"PLAYLIST_ID": "11", "TITLE": "Soir", "PARENT_USERNAME": "alice", "NB_SONG": "3",
            "PLAYLIST_PICTURE": "abcd", "PICTURE_TYPE": "cover"})).unwrap();
        assert_eq!((p.reference.as_str(), p.track_count, p.subtitle.as_deref()), ("playlist/11", Some(3), Some("alice")));
        assert_eq!(p.art.as_deref(), Some("https://cdn-images.dzcdn.net/images/cover/abcd/500x500-000000-80-0-0.jpg"));
        let p = playlist(&json!({"id": 1, "title": "Top", "nb_tracks": 100, "picture_xl": "https://x/p.jpg", "user": {"id": 2, "name": "Deezer"}})).unwrap();
        assert_eq!((p.subtitle.as_deref(), p.art.as_deref()), (Some("Deezer"), Some("https://x/p.jpg")));

        let list = json!({"data": [{"id": 1, "title": "a"}, {"title": "no id"}], "total": 10, "next": "https://api/…"});
        let pg = page(&list, 0, 2, |v| album(v, None));
        assert_eq!((pg["items"].as_array().unwrap().len(), &pg["total"], &pg["has_more"]), (1, &json!(10), &json!(true)));
        assert_eq!(page(&json!({}), 0, 50, |v| album(v, None)), json!({"items": [], "has_more": false}));
        let all = vec![json!({"id": 1}), json!({"id": 2}), json!({"id": 3})];
        let s = slice(&all, 1, 1, |v| album(v, None));
        assert_eq!((&s["items"][0]["ref"], &s["total"], &s["has_more"]), (&json!("album/2"), &json!(3), &json!(true)));
        assert_eq!(slice(&all, 9, 5, |v| album(v, None))["items"], json!([]));
    }

    /// Account-dependent fields; one test, since the account is global.
    #[test]
    fn favourites_and_ownership_follow_the_account() {
        let mine = json!({"PLAYLIST_ID": "1", "TITLE": "Soir", "PARENT_USER_ID": "42"});
        let theirs = json!({"id": 2, "title": "Top", "creator": {"id": 7}});
        set_user(None);
        assert_eq!(playlist(&mine).unwrap().editable, None, "unknown when signed out");
        set_user(Some(42));
        assert_eq!(playlist(&mine).unwrap().editable, Some(true));
        assert_eq!(playlist(&theirs).unwrap().editable, Some(false));

        let t = json!({"id": 77, "title": "Aria"});
        assert_eq!(track(&t, None).unwrap().favorite, None, "ids not loaded yet");
        set_favorite_ids("track", ["77".to_string()].into());
        assert_eq!(track(&t, None).unwrap().favorite, Some(true));
        assert_eq!(album(&json!({"id": 5}), None).unwrap().favorite, None, "album ids not loaded");
        set_favorite_ids("album", HashSet::new());
        assert_eq!(album(&json!({"id": 5}), None).unwrap().favorite, Some(false));
        note_favorite("track", "77", false);
        assert_eq!(track(&t, None).unwrap().favorite, Some(false));
        set_user(Some(43));
        assert_eq!(track(&t, None).unwrap().favorite, None, "another account: ids dropped");
        set_user(None);
    }

    #[test]
    fn item_json_skips_missing_fields() {
        let v = folder(&Ref::MyPlaylists, "Playlists").to_json();
        assert_eq!(v, json!({"ref": "my/playlists", "kind": "folder", "title": "Playlists", "playable": false, "browsable": true}));
    }
}
