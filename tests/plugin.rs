//! Fake plugin host: runs `deezsrc plugin` against a mock Deezer (gateway,
//! public API, media service and CDN, all on one local server) and checks
//! the JSON-RPC exchanges.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// The account's session cookie, as the mock gateway knows it.
const ARL: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90\
                   a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
/// A well-formed cookie the gateway no longer accepts.
const STALE_ARL: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const EMAIL: &str = "alice@example.org";
const LICENSE: &str = "license-token-xyz";
const SID: &str = "sid-0042";

/// Streams as Deezer serves them: encrypted per 2048-byte chunk.
mod files {
    use blowfish::cipher::block_padding::NoPadding;
    use blowfish::cipher::{BlockEncryptMut, KeyIvInit};
    use md5::Digest;

    /// The plain file of a song: a FLAC header, then bytes that differ
    /// per song, and a last chunk shorter than 2048 bytes.
    pub fn plain(song: &str) -> Vec<u8> {
        let seed: usize = song.parse().unwrap();
        let mut f = b"fLaC\0\0\0\x22".to_vec();
        f.extend((0..2048 * 6 + 777).map(|i| ((i * 31 + seed * 7) % 256) as u8));
        f
    }

    fn key(song: &str) -> [u8; 16] {
        let hex = format!("{:x}", md5::Md5::digest(song.as_bytes()));
        let h = hex.as_bytes();
        let secret = b"g4el58wc0zvf9na1";
        std::array::from_fn(|i| h[i] ^ h[i + 16] ^ secret[i])
    }

    pub fn encrypted(song: &str) -> Vec<u8> {
        let mut f = plain(song);
        for (i, chunk) in f.chunks_mut(2048).enumerate() {
            if chunk.len() == 2048 && i % 3 == 0 {
                cbc::Encryptor::<blowfish::Blowfish>::new_from_slices(&key(song), &[0, 1, 2, 3, 4, 5, 6, 7])
                    .unwrap()
                    .encrypt_padded_mut::<NoPadding>(chunk, 2048)
                    .unwrap();
            }
        }
        f
    }
}

#[derive(Default)]
struct Mock {
    /// `gw:<method>`, `api:<path>`, `media:get_url`, `cdn:<file>`, with
    /// the arguments, query, body or range.
    calls: Mutex<Vec<(String, Value)>>,
    /// Bumped to make the gateway refuse the current `api_token`.
    token_gen: AtomicU64,
    /// Whether the subscription includes FLAC.
    no_lossless: AtomicBool,
    /// Refuse the next `get_url` as a whole (an expired license token).
    refuse_license: AtomicBool,
}

type Shared = Arc<Mock>;

impl Mock {
    fn record(&self, key: String, v: Value) {
        self.calls.lock().unwrap().push((key, v));
    }

    fn calls(&self, key: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
    }

    fn count(&self, key: &str) -> usize {
        self.calls(key).len()
    }

    fn token(&self) -> String {
        format!("api-token-{}", self.token_gen.load(Ordering::SeqCst))
    }
}

fn song(id: &str) -> Option<Value> {
    let size = files::plain(id).len().to_string();
    let base = |title: &str| {
        json!({"SNG_ID": id, "SNG_TITLE": title, "ART_ID": "20", "ART_NAME": "Glenn Gould", "ALB_ID": "10",
               "ALB_TITLE": "Goldberg Variations", "ALB_PICTURE": "2e018122cb56986277102d2041a592c8", "DURATION": "185",
               "TRACK_NUMBER": "1", "DISK_NUMBER": "1", "GAIN": "-12.4", "TRACK_TOKEN": format!("tt-{id}"),
               "FILESIZE_FLAC": size, "FILESIZE_MP3_320": size, "FILESIZE_MP3_128": size,
               "RIGHTS": {"STREAM_ADS_AVAILABLE": true, "STREAM_SUB_AVAILABLE": true},
               "SNG_CONTRIBUTORS": {"main_artist": ["Glenn Gould"], "composer": ["Johann Sebastian Bach"]}})
    };
    Some(match id {
        "1" => base("Aria"),
        "4" => base("Aria (2nd version)"),
        // Not streamable here, but another version of it is.
        "3" => {
            let mut s = base("Aria (blocked)");
            s["RIGHTS"] = json!({"STREAM_ADS_AVAILABLE": false, "STREAM_SUB_AVAILABLE": false});
            s["FALLBACK"] = json!({"SNG_ID": "4", "SNG_TITLE": "Aria (2nd version)"});
            s
        }
        // Not streamable at all.
        "5" => {
            let mut s = base("Gone");
            s["RIGHTS"] = json!({"STREAM_ADS_AVAILABLE": false, "STREAM_SUB_AVAILABLE": false});
            s
        }
        // No file sizes: the relay asks the CDN.
        "6" => {
            let mut s = base("Unsized");
            for k in ["FILESIZE_FLAC", "FILESIZE_MP3_320", "FILESIZE_MP3_128"] {
                s.as_object_mut().unwrap().remove(k);
            }
            s
        }
        _ => return None,
    })
}

fn public_track() -> Value {
    json!({"id": 1, "readable": true, "title": "Aria", "duration": 185, "track_position": 1, "disk_number": 1,
           "release_date": "1982-01-01", "bpm": 0,
           "artist": {"id": 20, "name": "Glenn Gould"},
           "album": {"id": 10, "title": "Goldberg Variations", "cover_xl": "https://img/10.jpg", "release_date": "1982-01-01"},
           "contributors": [{"id": 20, "name": "Glenn Gould", "role": "Main"}]})
}

fn public_album() -> Value {
    json!({"id": 10, "title": "Goldberg Variations", "cover_xl": "https://img/10.jpg", "nb_tracks": 2, "duration": 245,
           "release_date": "1982-01-01", "record_type": "album", "label": "Sony Classical",
           "genres": {"data": [{"id": 98, "name": "Classique"}]}, "artist": {"id": 20, "name": "Glenn Gould"},
           "contributors": [{"id": 20, "name": "Glenn Gould", "role": "Main"}]})
}

fn gw_error(kind: &str, message: &str) -> Response {
    Json(json!({"error": {kind: message}, "results": {}})).into_response()
}

fn gw_ok(results: Value) -> Response {
    Json(json!({"error": [], "results": results})).into_response()
}

fn cookies(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .unwrap_or("")
        .split(';')
        .filter_map(|kv| kv.trim().split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn gateway(mock: &Mock, q: &HashMap<String, String>, headers: &HeaderMap, body: &str) -> Response {
    let method = q.get("method").cloned().unwrap_or_default();
    let args: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    mock.record(format!("gw:{method}"), args.clone());
    let jar = cookies(headers);
    let signed_in = jar.get("arl").map(String::as_str) == Some(ARL);
    if method == "deezer.getUserData" {
        assert_eq!(q.get("api_token").map(String::as_str), Some("null"));
        if !signed_in {
            return gw_ok(json!({"USER": {"USER_ID": 0}, "checkForm": "nope"}));
        }
        let lossless = !mock.no_lossless.load(Ordering::SeqCst);
        let results = json!({"checkForm": mock.token(), "OFFER_NAME": "Deezer Premium", "USER": {
            "USER_ID": 1, "BLOG_NAME": "alice", "EMAIL": EMAIL,
            "OPTIONS": {"license_token": LICENSE, "web_lossless": lossless, "mobile_lossless": lossless, "web_hq": true}}});
        return ([(header::SET_COOKIE, format!("sid={SID}; path=/; domain=.deezer.com; HttpOnly"))], Json(json!({"error": [], "results": results})))
            .into_response();
    }
    if !signed_in || jar.get("sid").map(String::as_str) != Some(SID) || q.get("api_token") != Some(&mock.token()) {
        return gw_error("VALID_TOKEN_REQUIRED", "Invalid CSRF token");
    }
    let arg = |k: &str| match &args[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let playlist = |id: &str| -> Option<Value> {
        let (title, owner) = match id {
            "100" => ("Soir", "1"),
            "101" => ("Nouvelle", "1"),
            "200" => ("Top Classique", "9"),
            _ => return None,
        };
        Some(json!({"PLAYLIST_ID": id, "TITLE": title, "PARENT_USER_ID": owner, "PARENT_USERNAME": if owner == "1" { "alice" } else { "Deezer" },
                    "NB_SONG": "2", "PLAYLIST_PICTURE": "abcdef", "PICTURE_TYPE": "playlist", "STATUS": 1,
                    "DESCRIPTION": "Pour le soir"}))
    };
    let results = match method.as_str() {
        "song.getData" => match song(&arg("SNG_ID")) {
            Some(s) => s,
            None => return gw_error("DATA_ERROR", "song not found"),
        },
        "song.getListData" => {
            let ids: Vec<String> = args["SNG_IDS"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            json!({"data": ids.iter().filter_map(|id| song(id)).collect::<Vec<_>>(), "count": ids.len()})
        }
        "song.getFavoriteIds" => json!({"data": [{"SNG_ID": 1, "DATE_ADD": 1_700_000_000}], "total": 1}),
        "deezer.pageProfile" => {
            let tab = arg("tab");
            let data = match tab.as_str() {
                "albums" => json!([{"ALB_ID": "10", "ALB_TITLE": "Goldberg Variations", "ART_ID": "20", "ART_NAME": "Glenn Gould",
                                    "ALB_PICTURE": "2e018122cb56986277102d2041a592c8", "PHYSICAL_RELEASE_DATE": "1982-01-01",
                                    "NUMBER_TRACK": "2"}]),
                "artists" => json!([{"ART_ID": "20", "ART_NAME": "Glenn Gould", "ART_PICTURE": "f2bc"}]),
                "playlists" => json!([playlist("100"), playlist("200"),
                    {"PLAYLIST_ID": "300", "TITLE": "Loved tracks", "PARENT_USER_ID": "1", "IS_LOVED_TRACK": true}]),
                _ => json!([]),
            };
            json!({"TAB": {tab: {"data": data, "total": data.as_array().unwrap().len()}}})
        }
        "playlist.getData" => match playlist(&arg("PLAYLIST_ID")) {
            Some(p) => p,
            None => return gw_error("DATA_ERROR", "playlist not found"),
        },
        "playlist.getSongs" => json!({"data": [song("1"), song("4")], "total": 2}),
        "playlist.create" => json!(101),
        "playlist.update" | "playlist.delete" | "playlist.addSongs" | "playlist.deleteSongs" | "favorite_song.add"
        | "favorite_song.remove" | "album.addFavorite" | "album.deleteFavorite" | "artist.addFavorite"
        | "artist.deleteFavorite" | "playlist.addFavorite" | "playlist.deleteFavorite" | "log.listen" => json!(true),
        "radio.getUserRadio" => json!({"data": [song("1"), song("4")]}),
        "song.getSearchTrackMix" => json!({"data": [song("1"), song("4"), song("5")]}),
        "song.getLyrics" if arg("SNG_ID") == "1" => json!({"LYRICS_ID": "7", "LYRICS_TEXT": "Aria\r\nda capo",
            "LYRICS_SYNC_JSON": [{"lrc_timestamp": "[00:00.00]", "milliseconds": "0", "line": "Aria"},
                                 {"lrc_timestamp": "[00:04.00]", "milliseconds": "4000", "line": "da capo"}]}),
        "song.getLyrics" => return gw_error("DATA_ERROR", "No lyrics"),
        "deezer.pageArtist" => json!({"DATA": {"ART_ID": "20"}, "BIO": {"BIO": "<p>Canadian pianist&nbsp;&amp; writer.</p>", "SOURCE": "x"}}),
        "album.getData" => json!({"ALB_ID": "10", "COPYRIGHT": "1982 Sony"}),
        _ => return gw_error("GATEWAY_ERROR", "unknown method"),
    };
    gw_ok(results)
}

fn public_api(path: &str, q: &HashMap<String, String>) -> Response {
    let list = |data: Value| {
        let n = data.as_array().unwrap().len();
        json!({"data": data, "total": n})
    };
    let body = match path {
        "search/track" if q.get("q").map(String::as_str) == Some("quota") => {
            json!({"error": {"type": "Exception", "message": "Quota limit exceeded", "code": 4}})
        }
        "search/track" if q.get("q").map(String::as_str) == Some("flood") => {
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "7")], "slow down").into_response();
        }
        "search/track" => list(json!([public_track()])),
        "search/album" => list(json!([public_album()])),
        "search/artist" => list(json!([{"id": 20, "name": "Glenn Gould", "picture_xl": "https://img/gould.jpg"}])),
        "search/playlist" => list(json!([{"id": 200, "title": "Top Classique", "nb_tracks": 2, "picture_xl": "https://img/p.jpg",
                                          "user": {"id": 9, "name": "Deezer"}}])),
        "track/1" => public_track(),
        "album/10" => public_album(),
        "album/10/tracks" => json!({"data": [
            {"id": 1, "title": "Aria", "duration": 185, "track_position": 1, "disk_number": 1, "readable": true, "artist": {"id": 20, "name": "Glenn Gould"}},
            {"id": 2, "title": "Variation 1", "duration": 60, "track_position": 2, "disk_number": 1, "readable": false, "artist": {"id": 20, "name": "Glenn Gould"}}
        ], "total": 2}),
        "artist/20" => json!({"id": 20, "name": "Glenn Gould", "picture_xl": "https://img/gould.jpg", "nb_album": 80, "nb_fan": 12345}),
        "artist/20/albums" => json!({"data": [{"id": 10, "title": "Goldberg Variations", "cover_xl": "https://img/10.jpg",
            "release_date": "1982-01-01"}, {"id": 11, "title": "Partitas", "release_date": "1957-01-01"}], "total": 80,
            "next": "https://api.deezer.com/artist/20/albums?index=2"}),
        "artist/20/top" => list(json!([public_track()])),
        "artist/20/related" => list(json!([{"id": 21, "name": "Sviatoslav Richter", "picture_xl": "https://img/r.jpg"}])),
        "artist/20/playlists" => list(json!([])),
        "artist/20/radio" => list(json!([public_track(), {"id": 7, "title": "Prelude", "readable": true, "artist": {"id": 21, "name": "Sviatoslav Richter"}}])),
        "chart/0/albums" => list(json!([public_album()])),
        "chart/0/tracks" => list(json!([public_track()])),
        // Ten albums, no total, `index` and `limit` ignored.
        "editorial/0/selection" | "editorial/132/selection" => json!({"data": [public_album()]}),
        "genre" => list(json!([{"id": 0, "name": "Tous"}, {"id": 132, "name": "Pop", "picture_xl": "https://img/pop.jpg"}])),
        "genre/132" => json!({"id": 132, "name": "Pop", "picture_xl": "https://img/pop.jpg"}),
        _ => json!({"error": {"type": "DataException", "message": "no data", "code": 800}}),
    };
    Json(body).into_response()
}

fn media(mock: &Mock, headers: &HeaderMap, body: &str) -> Response {
    let req: Value = serde_json::from_str(body).unwrap();
    mock.record("media:get_url".into(), req.clone());
    assert_eq!(req["license_token"], LICENSE, "the account's license token");
    if mock.refuse_license.swap(false, Ordering::SeqCst) {
        return Json(json!({"errors": [{"code": 1000, "message": "Invalid license token"}]})).into_response();
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
    let token = req["track_tokens"][0].as_str().unwrap_or_default();
    let id = token.strip_prefix("tt-").unwrap_or_default();
    if matches!(id, "3" | "5") {
        return Json(json!({"data": [{"errors": [{"code": 2002, "message": "Track token has no sufficient rights on requested media"}]}]}))
            .into_response();
    }
    let format = req["media"][0]["formats"][0]["format"].as_str().unwrap().to_string();
    let ext = if format == "FLAC" { "flac" } else { "mp3" };
    Json(json!({"data": [{"media": [{"media_type": "FULL", "cipher": {"type": "BF_CBC_STRIPE"}, "format": format,
        "sources": [{"url": format!("http://{host}/cdn/{id}.{ext}?hmac=secret-signature"), "provider": "ak"}],
        "nbf": 1, "exp": 4_000_000_000i64}]}]}))
    .into_response()
}

/// The CDN: encrypted files, `Range` honoured.
fn cdn(mock: &Mock, file: &str, headers: &HeaderMap) -> Response {
    let range = headers.get(header::RANGE).and_then(|r| r.to_str().ok()).map(str::to_string);
    mock.record(format!("cdn:{file}"), json!(range));
    let id = file.split('.').next().unwrap();
    let data = files::encrypted(id);
    let Some(range) = range else { return data.into_response() };
    let (a, b) = range.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
    let a: usize = a.parse().unwrap();
    let b: usize = if b.is_empty() { data.len() - 1 } else { b.parse::<usize>().unwrap().min(data.len() - 1) };
    (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_RANGE, format!("bytes {a}-{b}/{}", data.len()))], data[a..=b].to_vec())
        .into_response()
}

async fn handler(
    State(mock): State<Shared>,
    UrlPath(path): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if path == "www/ajax/gw-light.php" {
        return gateway(&mock, &q, &headers, &body);
    }
    if path == "media/v1/get_url" {
        return media(&mock, &headers, &body);
    }
    if let Some(file) = path.strip_prefix("cdn/") {
        return cdn(&mock, file, &headers);
    }
    if let Some(api) = path.strip_prefix("api/") {
        mock.record(format!("api:{api}"), json!(q));
        return public_api(api, &q);
    }
    (StatusCode::NOT_FOUND, "no such endpoint").into_response()
}

async fn start_mock() -> (String, Shared) {
    let mock = Shared::default();
    let app = Router::new().route("/{*path}", any(handler)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

struct Host {
    child: Child,
    /// The plugin's stderr (its log), kept to check what it writes there.
    log: PathBuf,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    notifications: Vec<Value>,
}

impl Host {
    fn spawn(base: &str) -> Self {
        let log = std::env::temp_dir().join(format!("deezsrc-plugin-log-{}", uuid::Uuid::new_v4()));
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_deezsrc"))
            .args(["plugin", "--gw-base", &format!("{base}/www"), "--api-base", &format!("{base}/api")])
            .args(["--media-base", &format!("{base}/media")])
            .env("RUST_LOG", "deezsrc=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Host { child, log, stdin, lines, next_id: 0, notifications: vec![] }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Nothing secret in the log: no cookie, token, e-mail or signed URL.
    fn assert_clean_log(&self) {
        let log = self.logs();
        for secret in [ARL, STALE_ARL, EMAIL, LICENSE, SID, "api-token-", "tt-1", "secret-signature"] {
            assert!(!log.contains(secret), "{secret} reached the log:\n{log}");
        }
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params})).await;
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("plugin answered in time")
            .unwrap()
            .expect("plugin still running");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// Whole response (`result` or `error`) to one request.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id") == Some(&json!(id)) {
                assert_eq!(msg["jsonrpc"], "2.0");
                return msg;
            }
            assert!(msg.get("method").is_some(), "unexpected message {msg}");
            self.notifications.push(msg);
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg.get("error").is_none(), "{method} failed: {msg}");
        msg["result"].clone()
    }

    async fn err(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg["error"]["code"].is_i64(), "{method} should fail: {msg}");
        msg["error"].clone()
    }

    async fn err_code(&mut self, method: &str, params: Value) -> i64 {
        self.err(method, params).await["code"].as_i64().unwrap()
    }

    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self.notifications.iter().position(|n| n["method"] == method) {
            return self.notifications.remove(i);
        }
        loop {
            let msg = self.read().await;
            if msg["method"] == method {
                return msg;
            }
            self.notifications.push(msg);
        }
    }

    async fn initialize(&mut self, data_dir: &Path, output: Value) -> Value {
        self.ok(
            "initialize",
            json!({"protocol": 1, "host": {"name": "fake", "version": "0"}, "data_dir": data_dir,
                   "cache_dir": data_dir.join("cache"), "locale": "fr-FR", "output": output}),
        )
        .await
    }

    async fn shutdown(mut self) {
        self.assert_clean_log();
        assert_eq!(self.ok("shutdown", Value::Null).await, Value::Null);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.unwrap().unwrap();
        assert!(status.success());
    }
}

fn data_dir(arl: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deezsrc-plugin-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(arl) = arl {
        let path = dir.join("account.json");
        std::fs::write(&path, json!({"arl": arl}).to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    dir
}

fn refs(list: &Value) -> Vec<String> {
    list.as_array().unwrap().iter().map(|i| i["ref"].as_str().unwrap().to_string()).collect()
}

/// Formats asked for by the last `get_url`.
fn last_formats(mock: &Mock) -> Vec<String> {
    let req = mock.calls("media:get_url").pop().expect("a get_url request");
    req["media"][0]["formats"].as_array().unwrap().iter().map(|f| f["format"].as_str().unwrap().to_string()).collect()
}

async fn fetch(url: &str, range: Option<&str>) -> (u16, HeaderMap, Vec<u8>) {
    let mut req = reqwest::Client::new().get(url);
    if let Some(r) = range {
        req = req.header("range", r);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        headers.insert(k.clone(), v.clone());
    }
    (status, headers, resp.bytes().await.unwrap().to_vec())
}

#[tokio::test]
async fn catalogue_browse_and_library() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(ARL));
    let mut host = Host::spawn(&base);
    let init = host.initialize(&dir, json!({"device": "hw:1,0", "max_rate": 96000, "max_bits": 24})).await;
    assert_eq!(init["protocol"], 1);
    assert_eq!(init["plugin"]["id"], "deezer");
    for cap in ["auth", "browse", "search", "resolve", "favorites", "reporting", "library", "lyrics", "radio", "details", "playlist_edit"] {
        assert_eq!(init["capabilities"][cap], true, "{cap}");
    }
    assert_eq!(init["capabilities"]["remote_control"], false);

    let status = host.ok("auth.status", json!({})).await;
    assert_eq!(status, json!({"state": "signed_in", "account": {"display_name": "alice", "detail": "Deezer Premium"}}));

    let root = host.ok("browse.root", json!({})).await;
    assert_eq!(refs(&root["sections"]), ["fav", "my/playlists", "flow", "charts", "releases", "genres"]);
    assert_eq!(root["sections"][0]["title"], "Coups de cœur", "labels in the host's language");
    assert_eq!(refs(&root["home"]), ["flow", "releases", "charts/tracks", "charts/albums", "charts/playlists"]);

    // Library lists: the account's favourites and playlists.
    let tracks = host.ok("library.tracks", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(refs(&tracks["items"]), ["track/1"]);
    assert_eq!((tracks["total"].as_u64(), tracks["has_more"].as_bool()), (Some(1), Some(false)));
    assert_eq!(tracks["items"][0]["album_ref"], "album/10");
    assert_eq!(tracks["items"][0]["art"], "https://cdn-images.dzcdn.net/images/cover/2e018122cb56986277102d2041a592c8/500x500-000000-80-0-0.jpg");
    let albums = host.ok("library.albums", json!({"offset": 0, "limit": 200})).await;
    let a = &albums["items"][0];
    assert_eq!((a["ref"].as_str(), a["artist"].as_str(), a["year"].as_u64(), a["track_count"].as_u64()),
               (Some("album/10"), Some("Glenn Gould"), Some(1982), Some(2)));
    let artists = host.ok("library.artists", json!({})).await;
    assert_eq!(artists["items"][0]["title"], "Glenn Gould");
    assert_eq!(artists["items"][0]["actions"][0], json!({"id": "radio", "label": "Mix de cet artiste", "ref": "radio/artist/20", "kind": "play"}));
    let playlists = host.ok("library.playlists", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(refs(&playlists["items"]), ["playlist/100", "playlist/200"], "loved tracks are library.tracks");
    assert_eq!((playlists["items"][0]["editable"].as_bool(), playlists["items"][1]["editable"].as_bool()), (Some(true), Some(false)));
    // Pages are cut out of the list read once.
    let page = host.ok("library.playlists", json!({"offset": 1, "limit": 1})).await;
    assert_eq!((refs(&page["items"]), page["total"].as_u64(), page["has_more"].as_bool()), (vec!["playlist/200".to_string()], Some(2), Some(false)));
    assert_eq!(mock.calls("gw:deezer.pageProfile").iter().filter(|a| a["tab"] == "playlists").count(), 1, "read once");
    assert_eq!(host.ok("browse.list", json!({"ref": "my/playlists"})).await["items"][0]["title"], "Soir");
    let fav = host.ok("browse.list", json!({"ref": "fav"})).await;
    assert_eq!(refs(&fav["items"]), ["fav/tracks", "fav/albums", "fav/artists"]);
    assert_eq!(host.ok("browse.list", json!({"ref": "fav/albums"})).await["items"][0]["ref"], "album/10");

    // Search: one request per kind, in the order asked.
    let all = host.ok("search", json!({"query": "gould"})).await;
    let kinds: Vec<&str> = all["groups"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["track", "album", "artist", "playlist"]);
    let found = host.ok("search", json!({"query": "gould", "kinds": ["album", "track"], "offset": 0, "limit": 10})).await;
    assert_eq!(found["groups"][0]["items"][0]["subtitle"], "Glenn Gould · 1982");
    assert_eq!(found["groups"][1]["items"][0]["ref"], "track/1");
    let q = mock.calls("api:search/album").pop().unwrap();
    assert_eq!((q["q"].as_str(), q["index"].as_str(), q["limit"].as_str()), (Some("gould"), Some("0"), Some("10")));

    // Catalogue pages.
    let album = host.ok("browse.list", json!({"ref": "album/10", "offset": 0, "limit": 50})).await;
    assert_eq!(album["total"], 2);
    let t = &album["items"][1];
    assert_eq!((t["album"].as_str(), t["art"].as_str(), t["playable"].as_bool()), (Some("Goldberg Variations"), Some("https://img/10.jpg"), Some(false)));
    let artist = host.ok("browse.list", json!({"ref": "artist/20", "offset": 0, "limit": 2})).await;
    assert_eq!(artist["items"][1]["artist"], "Glenn Gould", "the artist's albums carry the artist");
    assert_eq!((artist["total"].as_u64(), artist["has_more"].as_bool()), (Some(80), Some(true)));
    let pl = host.ok("browse.list", json!({"ref": "playlist/100"})).await;
    assert_eq!((pl["items"][0]["entry_id"].as_str(), pl["items"][1]["entry_id"].as_str()), (Some("1"), Some("4")));
    assert_eq!(pl["total"], 2);
    let charts = host.ok("browse.list", json!({"ref": "charts"})).await;
    assert_eq!(refs(&charts["items"]), ["charts/tracks", "charts/albums", "charts/artists", "charts/playlists"]);
    assert_eq!(host.ok("browse.list", json!({"ref": "charts/albums"})).await["items"][0]["ref"], "album/10");
    let releases = host.ok("browse.list", json!({"ref": "releases"})).await;
    assert_eq!((refs(&releases["items"]), releases["has_more"].as_bool()), (vec!["album/10".to_string()], Some(false)));
    assert_eq!(host.ok("browse.list", json!({"ref": "releases", "offset": 50})).await["items"], json!([]), "one short list");
    let genres = host.ok("browse.list", json!({"ref": "genres"})).await;
    assert_eq!(genres["items"], json!([{"ref": "genre/132", "kind": "folder", "title": "Pop", "art": "https://img/pop.jpg",
        "playable": false, "browsable": true}]), "not genre 0, \"All\"");
    assert_eq!(host.ok("browse.list", json!({"ref": "genre/132"})).await["items"][0]["ref"], "album/10");
    let flow = host.ok("browse.list", json!({"ref": "flow"})).await;
    assert_eq!((refs(&flow["items"]), flow["has_more"].as_bool()), (vec!["track/1".to_string(), "track/4".to_string()], Some(false)));
    assert_eq!(mock.calls("gw:radio.getUserRadio")[0]["USER_ID"], 1);
    let similar = host.ok("browse.list", json!({"ref": "similar/20"})).await;
    assert_eq!(similar["items"][0]["title"], "Sviatoslav Richter");
    assert_eq!(host.err_code("browse.list", json!({"ref": "track/1"})).await, -32602);
    assert_eq!(host.err_code("browse.list", json!({"ref": "album/999"})).await, -32002);
    assert_eq!(host.err_code("browse.list", json!({"ref": "nonsense"})).await, -32002);

    // item.get, with the favourite state once the ids are read.
    let mut fav = Value::Null;
    for _ in 0..100 {
        fav = host.ok("item.get", json!({"ref": "track/1"})).await["favorite"].clone();
        if !fav.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fav, true, "a favourite track says so");
    assert_eq!(host.ok("item.get", json!({"ref": "album/10"})).await["favorite"], true);
    assert_eq!(host.ok("favorites.set", json!({"ref": "album/10", "on": false})).await, Value::Null);
    assert_eq!(mock.calls("gw:album.deleteFavorite"), [json!({"ALB_ID": "10"})]);
    assert_eq!(host.ok("item.get", json!({"ref": "album/10"})).await["favorite"], false, "updated at once");
    host.ok("favorites.set", json!({"ref": "track/1", "on": true})).await;
    assert_eq!(mock.calls("gw:favorite_song.add"), [json!({"SNG_ID": "1"})]);
    host.ok("favorites.set", json!({"ref": "playlist/200", "on": true})).await;
    assert_eq!(mock.calls("gw:playlist.addFavorite"), [json!({"PARENT_PLAYLIST_ID": "200"})]);
    assert_eq!(host.err_code("favorites.set", json!({"ref": "genre/132", "on": true})).await, -32602);
    let p = host.ok("item.get", json!({"ref": "playlist/200"})).await;
    assert_eq!((p["kind"].as_str(), p["editable"].as_bool(), p["subtitle"].as_str()), (Some("playlist"), Some(false), Some("Deezer")));
    assert_eq!(host.ok("item.get", json!({"ref": "flow"})).await["title"], "Flow");
    assert_eq!(host.ok("item.get", json!({"ref": "charts"})).await["kind"], "folder");

    // Mixes and continuous playback.
    let mix = host.ok("browse.list", json!({"ref": "radio/track/1"})).await;
    assert_eq!(refs(&mix["items"]), ["track/1", "track/4", "track/5"]);
    let next = host.ok("radio.next", json!({"seed": "track/1", "exclude": [], "limit": 10})).await;
    assert_eq!(refs(&next["items"]), ["track/4"], "not the seed, not an unplayable track");
    let next = host.ok("radio.next", json!({"seed": "album/10", "exclude": ["track/1"], "limit": 10})).await;
    assert_eq!(refs(&next["items"]), ["track/7"], "the album artist's radio");
    let next = host.ok("radio.next", json!({"seed": "flow", "exclude": ["track/4"]})).await;
    assert_eq!(refs(&next["items"]), ["track/1"]);
    assert_eq!(host.err_code("radio.next", json!({"seed": "playlist/100"})).await, -32602);

    // Details, plain text, in the host's language.
    let d = host.ok("item.details", json!({"ref": "artist/20"})).await;
    assert_eq!(d["biography"], json!({"text": "Canadian pianist & writer.", "source": "Deezer"}));
    let shelves: Vec<&str> = d["related"].as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(shelves, ["Titres phares", "Artistes similaires"], "empty shelves left out");
    assert_eq!(mock.calls("gw:deezer.pageArtist")[0]["LANG"], "fr");
    let d = host.ok("item.details", json!({"ref": "album/10"})).await;
    let facts: Vec<(&str, &str)> = d["facts"].as_array().unwrap().iter().map(|f| (f["label"].as_str().unwrap(), f["value"].as_str().unwrap())).collect();
    assert_eq!(facts, [("Label", "Sony Classical"), ("Genre", "Classique"), ("Sortie", "01/01/1982"), ("Type", "Album"),
                       ("Artiste", "Glenn Gould"), ("©", "1982 Sony")]);
    assert_eq!(d["related"][0]["title"], "Plus de Glenn Gould");
    assert_eq!(refs(&d["related"][0]["items"]), ["album/11"], "not the album itself");
    let d = host.ok("item.details", json!({"ref": "track/1"})).await;
    assert_eq!(d["facts"][0], json!({"label": "Compositeur", "value": "Johann Sebastian Bach"}));
    assert_eq!(host.ok("item.details", json!({"ref": "charts"})).await, json!({}));

    // Lyrics: synced lines; none for another track, remembered.
    let lyrics = host.ok("lyrics.get", json!({"ref": "track/1"})).await;
    assert_eq!(lyrics["synced"], json!([{"time_ms": 0, "text": "Aria"}, {"time_ms": 4000, "text": "da capo"}]));
    assert_eq!(lyrics["plain"], "Aria\nda capo");
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/4"})).await, -32002);
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/4"})).await, -32002);
    assert_eq!(mock.calls("gw:song.getLyrics").iter().filter(|a| a["SNG_ID"] == "4").count(), 1, "no lyrics: remembered");

    // Rate limits: the public API's quota error and HTTP 429.
    let quota = host.err("search", json!({"query": "quota", "kinds": ["track"]})).await;
    assert_eq!((quota["code"].as_i64(), quota["data"]["retry_after"].as_u64()), (Some(-32004), Some(5)));
    let flood = host.err("search", json!({"query": "flood", "kinds": ["track"]})).await;
    assert_eq!((flood["code"].as_i64(), flood["data"]["retry_after"].as_u64()), (Some(-32004), Some(7)));

    // The gateway refuses the api token: the session is opened again, once.
    let opened = mock.count("gw:deezer.getUserData");
    mock.token_gen.fetch_add(1, Ordering::SeqCst);
    assert_eq!(host.ok("browse.list", json!({"ref": "playlist/100"})).await["total"], 2);
    assert_eq!(mock.count("gw:deezer.getUserData"), opened + 1);

    assert_eq!(host.err_code("search", json!({"nope": 1})).await, -32602);
    assert_eq!(host.err_code("search", json!({"query": "  "})).await, -32602);
    assert_eq!(host.err_code("player.fly", json!({})).await, -32601);
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resolve_and_relay() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(ARL));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({"max_rate": 192000, "max_bits": 24, "rates": [44100, 48000, 96000, 192000]})).await;

    let r = host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(last_formats(&mock), ["FLAC", "MP3_320", "MP3_128"], "best first");
    assert_eq!(mock.calls("media:get_url")[0]["track_tokens"], json!(["tt-1"]));
    assert_eq!(r["delivery"], "proxied");
    assert_eq!(r["format"], json!({"sample_rate": 44100, "bits": 16, "channels": 2, "codec": "flac"}));
    assert_eq!(r["duration_ms"], 185_000);
    assert_eq!(r["replaygain"], json!({"track_gain": -6.0}));
    assert_eq!(r["expires_at"], 4_000_000_000i64 - 60);
    let url = r["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/1.flac"), "{url}");
    let token = url.split('/').nth(3).unwrap();
    assert_eq!(token.len(), 32, "an unguessable token");

    // The whole file: the original, decrypted.
    let plain = files::plain("1");
    let (status, headers, body) = fetch(&url, None).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-length"], plain.len().to_string().as_str());
    assert_eq!((headers["accept-ranges"].to_str().unwrap(), headers["content-type"].to_str().unwrap()), ("bytes", "audio/flac"));
    assert!(body == plain, "decrypted file differs");

    // Ranges at arbitrary offsets: inside an encrypted chunk, across
    // chunks, to the end, the last bytes.
    let n = plain.len();
    for (a, b) in [(0, 0), (5, 2100), (2047, 2048), (3 * 2048 + 11, 5 * 2048 + 99), (6 * 2048 - 1, n - 1), (n - 3, n - 1)] {
        let (status, headers, body) = fetch(&url, Some(&format!("bytes={a}-{b}"))).await;
        assert_eq!(status, 206, "bytes {a}-{b}");
        assert_eq!(headers["content-range"], format!("bytes {a}-{b}/{n}").as_str());
        assert_eq!(headers["content-length"], (b - a + 1).to_string().as_str());
        assert!(body == plain[a..=b], "bytes {a}-{b} differ");
    }
    let (status, _, body) = fetch(&url, Some("bytes=10000-")).await;
    assert!(status == 206 && body == plain[10000..]);
    let (status, _, body) = fetch(&url, Some("bytes=-100")).await;
    assert!(status == 206 && body == plain[n - 100..]);
    assert_eq!(fetch(&url, Some(&format!("bytes={n}-"))).await.0, 416);
    // The CDN is asked for whole chunks only.
    let ranges: Vec<String> = mock.calls("cdn:1.flac").iter().filter_map(|r| r.as_str().map(str::to_string)).collect();
    assert!(ranges.contains(&"bytes=0-2047".to_string()) && ranges.contains(&"bytes=6144-12287".to_string()), "{ranges:?}");
    let head = reqwest::Client::new().head(&url).send().await.unwrap();
    assert_eq!((head.status().as_u16(), head.headers()["content-length"].to_str().unwrap()), (200, n.to_string().as_str()));
    let gone = url.replace(token, "00000000000000000000000000000000");
    assert_eq!(fetch(&gone, None).await.0, 410);

    // Refused here, but another version of the track plays.
    let r = host.ok("track.resolve", json!({"ref": "track/3", "purpose": "play"})).await;
    assert_eq!(mock.calls("media:get_url").iter().map(|c| c["track_tokens"][0].as_str().unwrap().to_string()).collect::<Vec<_>>(),
               ["tt-1", "tt-3", "tt-4"]);
    let (_, _, body) = fetch(r["url"].as_str().unwrap(), None).await;
    assert!(body == files::plain("4"), "the fallback's file, with the fallback's key");
    assert!(r["url"].as_str().unwrap().ends_with("/4.flac"));

    // Not streamable at all: unavailable, with Deezer's reason.
    let refused = host.err("track.resolve", json!({"ref": "track/5", "purpose": "play"})).await;
    assert_eq!(refused["code"], -32003);
    assert!(refused["message"].as_str().unwrap().contains("no sufficient rights"), "{refused}");
    assert_eq!(refused["data"]["errors"][0]["code"], 2002);
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/999"})).await, -32002);
    assert_eq!(host.err_code("track.resolve", json!({"ref": "album/10"})).await, -32002);

    // No size in the song data: the CDN is asked for one byte.
    let r = host.ok("track.resolve", json!({"ref": "track/6", "purpose": "preload"})).await;
    assert_eq!(mock.calls("cdn:6.flac"), [json!("bytes=0-0")]);
    let (_, headers, body) = fetch(r["url"].as_str().unwrap(), None).await;
    assert_eq!(headers["content-length"], files::plain("6").len().to_string().as_str());
    assert!(body == files::plain("6"));

    // Setting: MP3 at most.
    host.notify("settings.changed", json!({"settings": {"quality": "mp3_320", "report_playback": true}})).await;
    let r = host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(last_formats(&mock), ["MP3_320", "MP3_128"]);
    assert_eq!(r["format"], json!({"sample_rate": 44100, "channels": 2, "codec": "mp3"}), "no bit depth for MP3");
    let (_, headers, body) = fetch(r["url"].as_str().unwrap(), None).await;
    assert_eq!(headers["content-type"], "audio/mpeg");
    assert!(body == files::plain("1"));
    host.notify("settings.changed", json!({"settings": {"quality": "flac"}})).await;

    // A subscription without FLAC.
    mock.no_lossless.store(true, Ordering::SeqCst);
    mock.token_gen.fetch_add(1, Ordering::SeqCst); // the next call opens the session again
    host.ok("browse.list", json!({"ref": "playlist/100"})).await;
    host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(last_formats(&mock), ["MP3_320", "MP3_128"], "no FLAC without the plan");
    mock.no_lossless.store(false, Ordering::SeqCst);
    mock.token_gen.fetch_add(1, Ordering::SeqCst);

    // The license token refused: a new session, then the track plays.
    host.ok("browse.list", json!({"ref": "playlist/100"})).await; // the session opened again after the bump above
    mock.refuse_license.store(true, Ordering::SeqCst);
    let opened = mock.count("gw:deezer.getUserData");
    host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(mock.count("gw:deezer.getUserData"), opened + 1);

    // An output without 44.1 kHz: nothing fits.
    host.notify("output.changed", json!({"output": {"max_rate": 48000, "rates": [48000]}})).await;
    let e = host.err("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(e["code"], -32003, "{e}");
    // A 16-bit output still plays FLAC.
    host.notify("output.changed", json!({"max_rate": 48000, "max_bits": 16})).await;
    host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    assert_eq!(last_formats(&mock), ["FLAC", "MP3_320", "MP3_128"]);

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sign_in_and_out() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(None);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_eq!(host.err_code("search", json!({"query": "x"})).await, -32001);
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/1"})).await, -32001);

    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["url"], format!("{base}/www/login"));
    assert_eq!(begin["expects_input"], true);
    assert!(begin["instructions"].as_str().unwrap().contains("arl"));

    // Not an arl, then one Deezer refuses.
    assert_eq!(host.err_code("auth.complete", json!({"input": "https://www.deezer.com/login"})).await, -32602);
    let refused = host.err("auth.complete", json!({"input": STALE_ARL})).await;
    assert_eq!(refused["code"], -32602);
    assert!(refused["message"].as_str().unwrap().contains("does not accept"));
    assert!(!dir.join("account.json").exists());

    // Pasted as a cookie header.
    let status = host.ok("auth.complete", json!({"input": format!("  dzr_uniq_id=x; arl={ARL}; sid=y \n")})).await;
    assert_eq!(status, json!({"state": "signed_in", "account": {"display_name": "alice", "detail": "Deezer Premium"}}));
    let path = dir.join("account.json");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved, json!({"arl": ARL}), "only the cookie is kept");
    assert_eq!(host.ok("search", json!({"query": "x", "kinds": ["artist"]})).await["groups"][0]["kind"], "artist");

    assert_eq!(host.ok("auth.sign_out", json!({})).await["state"], "signed_out");
    assert!(!path.exists());
    assert_eq!(host.err_code("item.get", json!({"ref": "track/1"})).await, -32001);
    assert!(mock.count("gw:deezer.getUserData") >= 2);

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refused_cookie_reports_expiry() {
    let (base, _mock) = start_mock().await;
    let dir = data_dir(Some(STALE_ARL));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "expired"}));
    assert_eq!(host.err_code("browse.list", json!({"ref": "fav/tracks"})).await, -32001);
    assert_eq!(host.notification("auth.changed").await["params"], json!({"state": "expired"}));
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn playlist_edits() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(ARL));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    let created = host.ok("playlists.create", json!({"name": "  Nouvelle  ", "description": "Pour le soir", "public": true})).await;
    assert_eq!((created["ref"].as_str(), created["editable"].as_bool()), (Some("playlist/101"), Some(true)));
    assert_eq!(mock.calls("gw:playlist.create"), [json!({"title": "Nouvelle", "description": "Pour le soir", "status": 0, "songs": []})]);
    assert_eq!(host.err_code("playlists.create", json!({"name": " "})).await, -32602);

    host.ok("playlists.rename", json!({"ref": "playlist/100", "name": "Soirée"})).await;
    assert_eq!(mock.calls("gw:playlist.update"), [json!({"PLAYLIST_ID": "100", "title": "Soirée", "description": "Pour le soir", "status": 1})],
               "description and status kept");
    host.ok("playlists.add", json!({"ref": "playlist/100", "items": ["track/6", "track/7"]})).await;
    assert_eq!(mock.calls("gw:playlist.addSongs"), [json!({"PLAYLIST_ID": "100", "songs": [["6", 0], ["7", 0]], "offset": -1})]);
    assert_eq!(host.err_code("playlists.add", json!({"ref": "playlist/100", "items": ["album/10"]})).await, -32602);
    host.ok("playlists.remove", json!({"ref": "playlist/100", "entries": ["1"]})).await;
    assert_eq!(mock.calls("gw:playlist.deleteSongs"), [json!({"PLAYLIST_ID": "100", "songs": [["1", 0]]})]);

    // [1, 4]: 4 to the top is taken out, then put back at 0.
    mock.calls.lock().unwrap().clear();
    host.ok("playlists.move", json!({"ref": "playlist/100", "entry": "4", "to": 0})).await;
    assert_eq!(mock.calls("gw:playlist.deleteSongs"), [json!({"PLAYLIST_ID": "100", "songs": [["4", 0]]})]);
    assert_eq!(mock.calls("gw:playlist.addSongs"), [json!({"PLAYLIST_ID": "100", "songs": [["4", 0]], "offset": 0})]);
    // 1 past the end: last.
    host.ok("playlists.move", json!({"ref": "playlist/100", "entry": "1", "to": 9})).await;
    assert_eq!(mock.calls("gw:playlist.addSongs")[1]["offset"], -1);
    mock.calls.lock().unwrap().clear();
    host.ok("playlists.move", json!({"ref": "playlist/100", "entry": "1", "to": 0})).await;
    assert_eq!(mock.count("gw:playlist.deleteSongs"), 0, "already there: nothing sent");
    assert_eq!(host.err_code("playlists.move", json!({"ref": "playlist/100", "entry": "999", "to": 0})).await, -32002);
    host.ok("playlists.delete", json!({"ref": "playlist/100"})).await;
    assert_eq!(mock.calls("gw:playlist.delete"), [json!({"PLAYLIST_ID": "100"})]);

    // A followed playlist is never edited.
    mock.calls.lock().unwrap().clear();
    for (method, extra) in [("playlists.rename", json!({"name": "x"})), ("playlists.delete", json!({})),
                            ("playlists.remove", json!({"entries": ["1"]})), ("playlists.add", json!({"items": ["track/1"]})),
                            ("playlists.move", json!({"entry": "1", "to": 1}))] {
        let mut p = json!({"ref": "playlist/200"});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert_eq!(host.err_code(method, p).await, -32602, "{method}");
    }
    let wrote = mock.calls.lock().unwrap().iter().any(|(k, _)| k.starts_with("gw:playlist.") && k != "gw:playlist.getData");
    assert!(!wrote, "nothing written to a playlist the account does not own");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_and_play_reports() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(Some(ARL));
    let mut host = Host::spawn(&base);
    let init = host.initialize(&dir, json!({})).await;
    let declared: Vec<(&str, &str, Value)> = init["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["key"].as_str().unwrap(), s["label"].as_str().unwrap(), s["default"].clone()))
        .collect();
    assert_eq!(declared, [("quality", "Qualité maximale", json!("flac")), ("report_playback", "Signaler les écoutes à Deezer", json!(true))]);

    let wait_for = |key: &'static str, mock: Shared| async move {
        for _ in 0..100 {
            if let Some(v) = mock.calls(key).pop() {
                return Some(v);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    };

    // Reports on by default: the song streamed, its format, the time listened.
    host.ok("track.resolve", json!({"ref": "track/3", "purpose": "play"})).await;
    host.notify("playback.started", json!({"ref": "track/3"})).await;
    host.notify("playback.progress", json!({"ref": "track/3", "pos_ms": 30_000})).await;
    host.notify("playback.ended", json!({"ref": "track/3", "listened_ms": 120_500, "reason": "ended"})).await;
    let listen = wait_for("gw:log.listen", mock.clone()).await.expect("listen reported");
    let p = &listen["params"];
    assert_eq!(p["media"], json!({"id": "4", "type": "song", "format": "FLAC"}), "the fallback version was streamed");
    assert_eq!(p["lt"], 120);
    assert!(p["ts_listen"].as_i64().unwrap() <= p["timestamp"].as_i64().unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!dir.join("play-reports.json").exists(), "sent, so not kept");

    // The host's language changes: the settings are declared again in it.
    host.notify("locale.changed", json!({"locale": "en-GB"})).await;
    let declared = host.notification("settings.declared").await;
    assert_eq!(declared["params"]["settings"][1]["label"], "Report plays to Deezer");

    // Turned off: nothing more goes out.
    mock.calls.lock().unwrap().clear();
    host.notify("settings.changed", json!({"settings": {"report_playback": false}})).await;
    host.ok("track.resolve", json!({"ref": "track/1", "purpose": "play"})).await;
    host.notify("playback.started", json!({"ref": "track/1"})).await;
    host.notify("playback.ended", json!({"ref": "track/1", "listened_ms": 60_000, "reason": "ended"})).await;
    host.ok("auth.status", json!({})).await; // a round trip after the notifications
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.count("gw:log.listen"), 0, "reports turned off");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
