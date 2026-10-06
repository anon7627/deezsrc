# deezsrc

**Deezer** source plugin for players that implement the source plugin
protocol v1 (separate process, JSON-RPC over stdin/stdout). The player starts
it and hands it everything Deezer-related:

- **account sign-in**, with the `arl` session cookie of deezer.com, copied
  from your browser once you are signed in there (your password never goes
  through deezsrc);
- **catalogue**: search, favourite tracks, albums, artists and playlists
  (with their state on every item), Flow, charts, new releases, genres;
  track, album and artist mixes, similar artists; artist biographies,
  album and track credits; lyrics, synced when Deezer has them;
- **your playlists**: create, rename, delete, add, remove and move tracks
  (only playlists you own);
- **playback**: FLAC (16 bits / 44.1 kHz) or MP3, the best your
  subscription, the setting and your DAC allow. Deezer streams are
  encrypted: deezsrc decrypts them on this computer and hands the player
  the original FLAC or MP3 file, unchanged, through a local relay that
  honours seeking (`Range`). Nothing is decoded or re-encoded;
- **play reports** (setting, on by default): plays are reported to Deezer
  as its web player does, so they count for the artists, your history and
  your recommendations.

There is no Deezer Connect: the player cannot be controlled from the Deezer
apps.

Implementation details: [docs/plugin.md](docs/plugin.md).

> Unofficial Deezer client. It uses Deezer's private web API outside its
> terms of use. A Deezer subscription is required, and FLAC needs a plan
> that includes it (HiFi or equivalent). Deezer may break it or restrict
> the accounts that use it. Use at your own risk. deezsrc is a streaming
> client only: it has no download or export feature.

## Installation

```sh
cargo install --path .         # → ~/.cargo/bin/deezsrc
```

No system library is needed: TLS is rustls.

Releases are built by GitHub Actions (`.github/workflows/release.yml`), never
on a developer's machine. Pushing a `vX.Y.Z` tag (annotated: its message
becomes the release notes) runs the tests, builds static Linux binaries for
x86_64 and aarch64, checks that no build path is left in them, and publishes
the release with their SHA-256. The tag must match the version in
`Cargo.toml`.

Then declare the plugin in your player's configuration, with the absolute
path of the binary and the `plugin` argument:

```toml
[[plugins]]
id = "deezer"
command = "/path/to/.cargo/bin/deezsrc"
args = ["plugin"]
enabled = true
```

## Usage

1. In the player's plugin settings: Deezer → **Sign in**. The Deezer login
   page opens in your browser. Sign in there as usual.
2. Copy the `arl` cookie:
   - open the browser's developer tools (F12);
   - Firefox: **Storage** tab; Chrome, Edge: **Application** tab; then
     **Cookies** → `https://www.deezer.com`;
   - copy the value of `arl`, a long string of digits and letters (192
     characters).
3. Paste it into the player's field. The value alone, `arl=…`, or a whole
   `Cookie:` header all work. deezsrc checks it with Deezer before keeping
   it.
4. The Deezer catalogue shows up in the player (browsing and search).

The `arl` opens your account like a password would: never share it. It
lasts for months, but signing out of Deezer everywhere, or changing your
password, ends it; the player then shows the account as expired, and you
sign in again the same way.

Quality: FLAC when your plan includes it, the setting allows it and your
output plays 16 bits / 44.1 kHz as is; MP3 320 kbps, then 128 kbps,
otherwise. Settings (in the player's plugin dialog):

| Setting | Default | Effect |
|---|---|---|
| Best quality | FLAC | FLAC, MP3 320 kbps or MP3 128 kbps at most |
| Report plays to Deezer | on | Listen reports, as the web player sends them |

## Data

In the directories the player gives the plugin (`data_dir`, `cache_dir`):

| File | Contents |
|---|---|
| `data_dir/account.json` | The `arl` cookie, nothing else, mode `600` |
| `data_dir/play-reports.json` | Play reports not sent yet (only while some wait) |

`account.json` gives access to your account. Signing out deletes it.
deezsrc never writes the `arl`, the session tokens, signed stream URLs or
your e-mail address to its log.

## Troubleshooting

The plugin logs to stderr, which the player copies into its own log. More
detail: `RUST_LOG=deezsrc=debug` in the player's environment.

- **The pasted value is refused** ("Deezer does not accept this arl"):
  copy it again, whole, from a browser tab where you are signed in.
- **The account shows as expired**: the `arl` was revoked (sign-out
  everywhere, new password) or expired. Sign in again.
- **A track is "not available"**: Deezer refuses it for your country or
  your plan; the log gives Deezer's reason (`no sufficient rights…`).
  When Deezer offers another version of the track, deezsrc plays that one
  instead.
- **MP3 instead of FLAC**: your plan has no FLAC, the setting is lower,
  or the output does not take 44.1 kHz / 16 bits as is.
- **Check against Deezer itself**: `DEEZSRC_ARL=… cargo test live_account
  -- --ignored --nocapture` reads your lists, Flow, a mix and lyrics;
  `DEEZSRC_ARL=… DEEZSRC_TRACK=<id> DEEZSRC_OUT=/tmp/t.flac cargo test
  live_stream -- --ignored --nocapture` plays a track through the relay
  into a file (then `flac -t /tmp/t.flac`).

## Tests

```sh
cargo test                                                   # unit tests + fake host against a mock Deezer
DEEZSRC_ARL=… cargo test live_account -- --ignored --nocapture
DEEZSRC_ARL=… DEEZSRC_TRACK=<id> DEEZSRC_OUT=<file> cargo test live_stream -- --ignored --nocapture
```

The live tests need a Deezer account and are never run by CI.

## Architecture

| Module | Role |
|---|---|
| `plugin/mod.rs` | JSON-RPC loop, `initialize`/`shutdown`, `auth.*`, notifications |
| `plugin/rpc.rs` | Line-based JSON-RPC 2.0, error codes |
| `plugin/catalog.rs`, `plugin/items.rs` | `browse.*`, `library.*`, `search`, `item.get`, `favorites.set`, `radio.next`; Deezer answers → items |
| `plugin/playlists.rs` | `playlists.*` |
| `plugin/details.rs`, `plugin/lyrics.rs` | `item.details`, `lyrics.get` |
| `plugin/resolve.rs` | `track.resolve`: format matched to the plan and the output, fallback versions |
| `plugin/relay.rs`, `crypto.rs` | Local relay: CDN ranges decrypted on the fly (Blowfish-CBC, every third 2048-byte chunk) |
| `plugin/report.rs`, `plugin/settings.rs` | Play reports; declared settings |
| `deezer.rs` | Web gateway (session, `api_token`, license token), public API, media service |
| `account.rs` | `arl` extraction from what the user pastes, storage |
