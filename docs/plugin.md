# deezsrc: how the plugin works

deezsrc is a source plugin (protocol v1): a process started by the host
player, speaking JSON-RPC 2.0 over stdin/stdout, one JSON message per line.
Logs go to stderr. The plugin never touches the signal: it decrypts Deezer's
streams and serves the original FLAC or MP3 bytes from a local relay, which
the host fetches and decodes itself.

```
host ──(JSON-RPC over stdin/stdout)──► deezsrc plugin
                                         ├─ auth.*        → arl cookie, gateway deezer.getUserData
                                         ├─ browse.*      → public API (catalogue), gateway (account)
                                         ├─ search        → public API search/<kind>
                                         ├─ track.resolve → gateway song.getData, media get_url
                                         └─ relay ◄── CDN (encrypted), decrypted on the fly
host ──(HTTP, 127.0.0.1)──► relay      (original codec, unchanged)
```

Advertised capabilities: `auth`, `browse`, `search`, `resolve`,
`favorites`, `reporting`, `library`, `lyrics`, `radio`, `details`,
`playlist_edit`. `remote_control` is `false`: there is no Deezer Connect.

## Deezer's three services

| Service | Root | Used for | Credentials |
|---|---|---|---|
| Web gateway | `www.deezer.com/ajax/gw-light.php?method=&input=3&api_version=1.0&api_token=` (POST, JSON arguments) | session, favourites, playlists, Flow, track tokens, lyrics, biographies, play reports | `arl` and `sid` cookies, `api_token` |
| Public API | `api.deezer.com` (GET) | albums, artists, tracks, search, charts, editorial, genres, mixes | none |
| Media service | `media.deezer.com/v1/get_url` (POST) | stream URLs | license token, track token |

**Session.** `deezer.getUserData` (with `api_token=null`) answers the
account (`USER.USER_ID`, 0 when the `arl` is not accepted), its public name
(`BLOG_NAME`), the subscription (`OFFER_NAME`), `checkForm` (the
`api_token` of later calls, tied to the `sid` cookie set in the same
answer) and `USER.OPTIONS`: `license_token`, `web_lossless` /
`mobile_lossless` / `lossless` (FLAC allowed), `web_hq` / `mobile_hq` /
`hq` (MP3 320 allowed).
The session is opened on first use and again after an hour. When the
gateway refuses the `api_token` (`VALID_TOKEN_REQUIRED`, or a
`GATEWAY_ERROR` about the token), the session is opened again and the call
retried once.

**Gateway errors** come as `{"error": {"KIND": "message"}}` in an HTTP 200
answer: `DATA_ERROR` means the item does not exist.

**Public API errors** come as `{"error": {"type", "message", "code"}}` in
an HTTP 200 answer: code 4 is the quota (about 50 requests per 5 s), code
800 a missing item.

The plugin never logs the `arl`, the `api_token`, the license token, track
tokens, URLs with their query (CDN URLs are signed) or the account's
e-mail address, which `deezer.getUserData` also returns. Transport errors
are logged without their URL.

## Start-up

`initialize` provides:
- `data_dir`: the `arl` (`account.json`), unsent play reports;
- `output`: what the output plays natively;
- `locale`: the host's interface language, for every label the plugin
  writes and for biographies (French, else English). On
  `locale.changed {locale}` the plugin switches and declares its settings
  again (`settings.declared`) in the new language;
- `settings`: the values stored for the plugin's settings (below).

When an `arl` is stored, the session is opened in the background, then the
favourite lists are read and the play reports left over are sent.

## Settings

Declared in the `initialize` result (`settings`), labelled in the host's
language. Both apply without restarting the plugin (`settings.changed`).

| Key | Type | Default | Effect |
|---|---|---|---|
| `quality` | choice: `flac`, `mp3_320`, `mp3_128` | `flac` | Best format asked for (see Resolution) |
| `report_playback` | bool | `true` | Report plays to Deezer (below) |

Values of unknown keys, or of the wrong type, are ignored; missing keys
take their default.

`output.changed` updates the output in use; the object is accepted on its
own or wrapped in `{output: …}`.

## Sign-in (`auth.*`)

| Method | Implementation |
|---|---|
| `auth.status` | The session (`deezer.getUserData`): `signed_in` with `{display_name: BLOG_NAME, detail: OFFER_NAME}`; `expired` when the stored `arl` is refused; `signed_out` without one |
| `auth.begin` | `{url: "https://www.deezer.com/login", instructions, expects_input: true}`. The instructions (host's language) say how to copy the `arl` cookie from the browser's developer tools |
| `auth.complete {input}` | Finds the `arl` in the input, opens a session with it, and saves it to `account.json` (mode `600`) only if Deezer accepts it |
| `auth.sign_out` | Deletes `account.json`, forgets the session, the lists and the unsent reports |

`auth.complete` accepts the bare value, `arl=…`, a whole `Cookie:` header
or a row copied from the browser's cookie table (tab-separated). The value
must be 64 to 1024 ASCII letters and digits (Deezer's are 192 hex
characters); otherwise, or when Deezer refuses it, the answer is
`-32602` with a message for the user. Only the `arl` is stored: the
account's name and subscription are read again from Deezer.

When Deezer refuses the `arl` mid-session (`USER_ID` 0, or a token still
refused after a new session), requests answer `auth_required` and the
plugin sends `auth.changed {state: "expired"}` once.

## Catalogue

| Need | Source |
|---|---|
| Search | public `search/track`, `search/album`, `search/artist`, `search/playlist` (`q`, `index`, `limit`), one request per kind, at once |
| Album and its tracks | public `album/<id>`, `album/<id>/tracks` |
| Artist and their albums | public `artist/<id>`, `artist/<id>/albums` |
| Track | public `track/<id>` |
| Playlist and its tracks | gateway `playlist.getData`, `playlist.getSongs` (`start`, `nb`), which also read private playlists |
| Charts | public `chart/0/tracks`, `albums`, `artists`, `playlists` |
| New releases | public `editorial/0/selection` (new albums picked by Deezer's editors: one short list, `index` and `limit` ignored, cut into pages here; `editorial/<id>/releases` answers nothing any more) |
| Genres, their new releases | public `genre` (genre 0, "All", left out), `editorial/<id>/selection` |
| Similar artists | public `artist/<id>/related` |
| Flow | gateway `radio.getUserRadio {USER_ID}` |
| Track mix | gateway `song.getSearchTrackMix {SNG_ID}`; if it fails or is empty, the radio of the track's artist |
| Artist mix | public `artist/<id>/radio` |
| Album mix | the radio of the album's artist |
| Favourite tracks | gateway `song.getFavoriteIds`, then `song.getListData` for the page's ids |
| Favourite albums, artists; playlists | gateway `deezer.pageProfile {USER_ID, tab, nb}` → `TAB.<tab>.data` |
| Add or remove a favourite | gateway `favorite_song.add`/`remove {SNG_ID}`, `album.addFavorite`/`deleteFavorite {ALB_ID}`, `artist.addFavorite`/`deleteFavorite {ART_ID}`, `playlist.addFavorite {PARENT_PLAYLIST_ID}`/`deleteFavorite {PLAYLIST_ID}` |

Protocol refs:
- catalogue entries: `track/<id>`, `album/<id>`, `artist/<id>`,
  `playlist/<id>` (ids are numbers; the account's own uploads have negative
  ones);
- sections: `fav` (a folder holding the next three), `fav/tracks`,
  `fav/albums`, `fav/artists`, `my/playlists`, `flow` (a playlist item),
  `charts` (a folder holding `charts/tracks`, `charts/albums`,
  `charts/artists`, `charts/playlists`), `releases`, `genres` (one
  `genre/<id>` folder of new releases per genre);
- related content: `radio/track/<id>`, `radio/album/<id>`,
  `radio/artist/<id>` (mixes), `similar/<artist id>`.

`browse.root` returns:
- `sections`, for hosts that show the plugin in their sidebar: Favourites ·
  My playlists · Flow · Charts · New releases · Genres;
- `home`, for hosts that merge the library lists: Flow · New releases ·
  Top tracks · Top albums · Top playlists.

**Two shapes.** The public API answers `{"id", "title", "artist": {…},
"album": {…}}`; the gateway `{"SNG_ID", "SNG_TITLE", "VERSION", "ART_NAME",
"ALB_PICTURE", …}`, numbers often in strings. `items.rs` reads both.
Pictures come as URLs from the public API (`cover_xl`, `picture_xl`…), as
hashes from the gateway; a hash (hex only) becomes
`https://cdn-images.dzcdn.net/images/<cover|artist|playlist>/<hash>/500x500-000000-80-0-0.jpg`.

**Item fields beyond the basics:**
- `album_ref`, `artist_ref` (tracks, albums): from the album and the main
  artist;
- `playable` (tracks): the public API's `readable`; for the gateway, some
  `RIGHTS.STREAM_*_AVAILABLE` on, or a `FALLBACK` version;
- `favorite`: whether the track, album, artist or playlist is among the
  account's favourites. The ids come from the library lists, read in the
  background at start-up and sign-in, again after 10 minutes (and when the
  host reads them), and updated at once by `favorites.set`. Absent until
  they are read. A playlist the account owns is not a "favourite";
- `entry_id` (tracks of a playlist): the track id, see Playlist edits;
- `editable` (playlists): the account owns it (`PARENT_USER_ID`, or the
  public API's `creator.id`, is its user id);
- `actions`, labelled in the host's language: on a track, its mix; on an
  album, its mix; on an artist, their mix and similar artists. `play`
  actions point to a `radio/…` ref, `browse` actions to a folder.

**Flow** is a playlist item whose `browse.list` gives new tracks at each
call: one page, `has_more: false`.

**Continuous playback (`radio.next {seed, exclude, limit}`).** The seed is
a track, album, artist, mix or `flow`. The plugin reads the seed's mix and
answers `{items}`: playable tracks only, without the seed track and without
those in `exclude`, at most `limit` (20 by default, 50 at most). `-32602`
for any other seed.

## Library (`library.*`)

| Method | Source | Items |
|---|---|---|
| `library.tracks` | `song.getFavoriteIds`, then `song.getListData` (200 ids per request) | favourite tracks |
| `library.albums` | `deezer.pageProfile` tab `albums` | favourite albums (`browse.list` gives their tracks) |
| `library.artists` | `deezer.pageProfile` tab `artists` | favourite artists (`browse.list` gives their albums) |
| `library.playlists` | `deezer.pageProfile` tab `playlists` | playlists the user owns or follows; "Loved tracks" (`IS_LOVED_TRACK`) is left out, its tracks being `library.tracks` |

The gateway gives each list whole (10 000 entries at most): it is read
once, kept a minute, and cut into the pages the host asks for (`{offset,
limit}`, 200 at most, `{items, total, has_more}`). `favorites.set` and
playlist edits forget the list they change.

`favorites.set` accepts tracks, albums, artists and playlists; anything
else is refused (`-32602`).

## Playlist edits (`playlists.*`)

Edits of the account's own playlists, through the gateway:

| Method | Gateway method | Arguments |
|---|---|---|
| `playlists.create {name, description?, public?}` | `playlist.create` | `title`, `description`, `status` (0 public, 1 private), `songs: []`; the answer is the new id, then `playlist.getData` gives the item |
| `playlists.rename {ref, name}` | `playlist.update` | `PLAYLIST_ID`, `title`, and the current `description` and `status`, so that nothing else changes |
| `playlists.delete {ref}` | `playlist.delete` | `PLAYLIST_ID` |
| `playlists.add {ref, items}` | `playlist.addSongs` | `PLAYLIST_ID`, `songs: [[id, 0]…]`, `offset: -1` (the end) |
| `playlists.remove {ref, entries}` | `playlist.deleteSongs` | `PLAYLIST_ID`, `songs: [[id, 0]…]` |
| `playlists.move {ref, entry, to}` | `playlist.deleteSongs`, then `playlist.addSongs` | the entry taken out, then put back with `offset: to` (`-1` when it goes last) |

- **Ownership check.** Before editing an existing playlist, the plugin reads
  it (`playlist.getData`) and refuses (`-32602`) unless its
  `PARENT_USER_ID` is the account's: a followed playlist is never touched.
- **Entries.** Deezer keeps a track once per playlist, so `entry_id` is the
  track id. `entries` and `entry` are ids (digits).
- `items` must be track refs, 1 to 500. Names are trimmed (200 characters
  at most) and required.
- **Moves.** The protocol's `to` is the position once the entry is out,
  which is what taking it out and putting it back at `to` gives. The plugin
  first finds the entry's position (reading the playlist, 500 tracks a
  page): nothing is sent when it is already there, a `to` past the end
  puts it last, an unknown entry answers `not_found` (-32002). If putting
  it back fails, the plugin appends it, so the track is never lost. The
  track's "added on" date becomes the date of the move.

## Details (`item.details`)

`{biography?: {text, source}, related?: [{title, items}], facts?: [{label,
value}]}`, titles and labels in the host's language, empty shelves left
out, 20 items per shelf. Each source is optional: what Deezer does not
answer is left out.

| Ref | Source | Content |
|---|---|---|
| `artist/<id>` | public `artist/<id>`, `/top`, `/related`, `/playlists`; gateway `deezer.pageArtist {ART_ID, LANG}` | biography (`BIO.BIO`, else `BIO.RESUME`); fans, albums; top tracks, similar artists, playlists |
| `album/<id>` | public `album/<id>`, `artist/<id>/albums`; gateway `album.getData` | label, genres, release date, type, contributors by role, © and ℗; more albums of the artist |
| `track/<id>` | gateway `song.getData` (`SNG_CONTRIBUTORS`, by role), else public `track/<id>` (`contributors`); public `track/<id>` | credits by role, BPM, release date |

Other refs answer `{}`. Deezer texts may hold HTML: tags are dropped, line
breaks kept and entities decoded, so the host gets plain text.

## Lyrics (`lyrics.get`)

Gateway `song.getLyrics {SNG_ID}`:
- `LYRICS_SYNC_JSON` (`[{milliseconds, line}]`) → `synced: [{time_ms,
  text}]`; blank lines without a time keep the previous line's;
- `LYRICS_TEXT` → `plain`.

A track without lyrics (`DATA_ERROR`) or an unreadable answer is
`not_found` (-32002), remembered for 6 hours so that the gateway is not
asked again meanwhile. Signed out, offline or rate limited answer their own
codes, so that the host asks again later. Deezer may move lyrics elsewhere;
the plugin then answers `not_found` and the host goes on with its other
sources.

## Resolution (`track.resolve`)

1. **Song.** Gateway `song.getData {SNG_ID}`: `TRACK_TOKEN`,
   `FILESIZE_FLAC`, `FILESIZE_MP3_320`, `FILESIZE_MP3_128`, `DURATION`,
   `GAIN`, `FALLBACK`. An unknown song is `not_found` (-32002).
2. **Formats**, best first, all of them at once:
   - at most the `quality` setting;
   - FLAC only if the plan allows it (`lossless`) and the output takes
     44.1 kHz / 16 bits as is; MP3 320 only if the plan allows it (`hq`);
     MP3 if the output takes 44.1 kHz;
   - FLAC and MP3 320 only when the song has such a file (a size other
     than 0); MP3 128 always.
   - None left: `unavailable` (-32003), saying why.
3. **URL.** `media.deezer.com/v1/get_url`:

   ```json
   {"license_token": "…", "track_tokens": ["…"],
    "media": [{"type": "FULL", "formats": [{"cipher": "BF_CBC_STRIPE", "format": "FLAC"},
                                           {"cipher": "BF_CBC_STRIPE", "format": "MP3_320"}, …]}]}
   ```

   answers `data[0].media[0]`: `format` (the first one Deezer has),
   `sources[0].url` (CDN), `exp` (unix seconds). Or `data[0].errors`
   (`[{code, message}]`, e.g. 2002 "Track token has no sufficient rights
   on requested media"): Deezer refuses the track, for the region or the
   plan.
   An answer with `errors` but no `data` refuses the request as a whole
   (an expired license token, say): the session is opened again and the
   request sent once more.
4. **Fallback.** A refused song with a `FALLBACK.SNG_ID` (another version
   of the same track, e.g. another release) is tried the same way; the
   relay then serves that song, with its own key. Still refused:
   `unavailable` (-32003) with Deezer's reason, and `data.errors`.
5. **Size.** `FILESIZE_<format>`; when missing, a one-byte range request to
   the CDN (`Content-Range: bytes 0-0/<size>`).
6. **Relay.** The CDN URL is registered with the local relay, which answers
   a `127.0.0.1` URL.

**Response:**
- `url`: `http://127.0.0.1:<port>/<token>/<song id>.<flac|mp3>`, the token
  32 random hex characters;
- `delivery: "proxied"`;
- `format`: `{sample_rate: 44100, bits: 16, channels: 2, codec: "flac"}`,
  or `{sample_rate: 44100, channels: 2, codec: "mp3"}` (MP3 has no bit
  depth);
- `duration_ms` from `DURATION`;
- `replaygain: {track_gain}` from `GAIN`, as open-source Deezer clients
  convert it: `-(GAIN + 18.4)` dB; left out when `GAIN` is missing or 0;
- `expires_at`: `exp` minus 60 s, or 10 minutes without it.

## Decryption and the relay

Deezer streams (`BF_CBC_STRIPE`) are cut into 2048-byte chunks. Chunks 0,
3, 6… are encrypted with Blowfish-CBC, IV `00 01 02 03 04 05 06 07`, no
padding, each chunk on its own; the others, and a last chunk shorter than
2048 bytes, are in clear. The key depends on the song id: with `h` the
lowercase hex MD5 of the id and `S` a public constant of the web player,
`key[i] = h[i] ^ h[i+16] ^ S[i]`, 16 bytes. This is the scheme open-source
Deezer clients use. Decryption keeps the size: the file served is exactly
as long as the CDN's.

The relay (`http://127.0.0.1:<port>/`, started with the first stream):
- answers `200` with `Content-Length` and `Accept-Ranges: bytes`, or `206`
  with `Content-Range` for `Range: bytes=a-b`, `a-` or `-n`; `416` for a
  range past the end; `HEAD` without touching the CDN;
- asks the CDN for whole chunks only (from the chunk holding `a` to the one
  holding `b`), decrypts them as they arrive and drops what lies outside
  the range, so any offset can be served at once;
- copes with a CDN that ignores `Range` (`200`): the bytes before the
  range are decrypted and dropped;
- answers `410 Gone` for an unknown or expired token, and when the CDN
  refuses the URL (401, 403, 404, 410), so that the host resolves the track
  again; `502` for other CDN failures;
- keeps 64 streams at most, the oldest dropped first.

The bytes served are the original FLAC or MP3 file: nothing is decoded,
resampled or re-encoded.

## Play reports (`reporting`)

The capability is always declared; the `report_playback` setting decides
whether anything is sent. `playback.started {ref}` notes the start of a
track resolved by the plugin; `playback.ended {ref, listened_ms}` queues a
report, sent through the gateway method `log.listen`:

```json
{"params": {"media": {"id": "<song id>", "type": "song", "format": "FLAC"},
            "ts_listen": <start>, "timestamp": <end>, "lt": <seconds listened>,
            "type": 0, "stat": {"seek": 0, "pause": 0, "sync": 1},
            "ctxt": {"t": "deezsrc", "id": "<song id>"}, "dev": {"t": 0, "v": "<version>"},
            "ls": [], "payload": {}, "stream_id": "<uuid>"}}
```

The song is the one streamed (a fallback version, maybe), in the format
picked at resolution; `lt` is at most the track's length; nothing is sent
when nothing was listened. The payload is modelled on the web player's
listen logs; it has not been checked against Deezer's own records. Unsent
reports wait in `data_dir/play-reports.json` (500 at most) and go out after
the next play or at the next start. Turning the setting off, or signing
out, drops them.

## Errors

| Deezer answer | Protocol code |
|---|---|
| `arl` refused (`USER_ID` 0, token still refused after a new session, `NEED_USER_AUTH_REQUIRED`) | `auth_required` (-32001) |
| Gateway `DATA_ERROR`, public API code 800, HTTP 404, unknown `ref` | `not_found` (-32002) |
| `get_url` refusal, no format fitting the plan and the output, HTTP 403 | `unavailable` (-32003); `data.errors` holds Deezer's codes |
| Public API code 4 (quota) | `rate_limited` (-32004), `data.retry_after` 5 |
| HTTP 429 | `rate_limited` (-32004), `data.retry_after` from `Retry-After` (30 s otherwise) |
| HTTP 5xx, connection error or timeout | `network` (-32005) |

## Tests

`tests/plugin.rs` runs `deezsrc plugin --gw-base <mock>/www --api-base
<mock>/api --media-base <mock>/media` (hidden options) against a mock
Deezer: gateway (with the `arl` and `sid` cookies, the `api_token`, and
`deezer.getUserData`), public API, media service and CDN, which serves
files encrypted as Deezer does and honours `Range`.

Covered:
- handshake, capabilities, settings and their translation;
- `auth.*`: sign-in by pasting a cookie header, a value Deezer refuses,
  something that is not an `arl`, sign-out, an expired stored `arl`
  (`auth.changed`);
- a refused `api_token`: the session is opened again once;
- catalogue, `library.*` (pages cut out of lists read once), `home`
  shelves, Flow, charts, genres, mixes, `radio.next`, details, lyrics,
  favourites and their state on items;
- `track.resolve`: formats asked for per setting, plan and output; the
  file through the relay, whole and in ranges at arbitrary offsets (inside
  an encrypted chunk, across chunks, the short last chunk), compared with
  the plain file; `HEAD`, `416`, an unknown token (`410`); a fallback
  version; a refused track; a song without sizes; a refused license token;
  an output without 44.1 kHz;
- playlist edits, and refusal on a playlist the account does not own;
- play reports (on, then turned off);
- rate limits: the quota error and HTTP 429;
- the log never holds the `arl`, the `sid`, the `api_token`, the license
  token, a track token, a signed URL or the e-mail address.

Unit tests cover the key derivation (against an independent
implementation), chunk decryption against a reference Blowfish (OpenSSL),
decryption from any chunk boundary for arbitrary ranges, format choice,
item mapping from both answer shapes, lyrics, and error mapping.

Against Deezer itself (ignored by default, need an account):

```sh
DEEZSRC_ARL=… cargo test live_account -- --ignored --nocapture
DEEZSRC_ARL=… DEEZSRC_TRACK=<id> DEEZSRC_OUT=/tmp/t.flac \
  cargo test live_stream -- --ignored --nocapture && flac -t /tmp/t.flac
```

## Risks

- **Private API**: the gateway methods, `get_url` and the stream scheme are
  Deezer's own, undocumented, and may change without notice. The mock
  follows what open-source Deezer clients do; the plugin has not been
  checked against Deezer in this repository's tests.
- **Less certain parts**: the track mix (`song.getSearchTrackMix`, with the
  artist's radio as a fallback), lyrics (`song.getLyrics`), the play report
  payload, `playlist.update` with only the fields to change, and moves made
  of a removal and an insertion.
- **Lifetime of the `arl` and of stream URLs**: not measured; an expired
  URL leads the host to resolve again.
- **Terms of use**: deezsrc uses Deezer's private API outside its terms;
  Deezer may block it or restrict the accounts that use it.
