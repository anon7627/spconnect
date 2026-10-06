# spconnect

Unofficial **Spotify** source plugin for players that implement the source
plugin protocol v1 (separate process, JSON-RPC over stdin/stdout). The
player starts it and hands it everything Spotify-related:

- **account sign-in**, through the official Spotify page (your password
  never goes through spconnect);
- **catalogue**: search, your library (liked songs, saved albums, followed
  artists, playlists, with the saved state on every item), recently played,
  your top tracks and artists, the Spotify playlists made for you; radio
  from a track, album or artist, related artists; artist biographies, album
  facts and track credits; synced lyrics;
- **your playlists**: create, rename, delete, add, remove and move tracks
  (only playlists you own);
- **playback**: each track becomes a local address serving its Ogg Vorbis
  file, decrypted by spconnect on this computer and handed to the player
  unchanged. The player decodes it itself; spconnect never decodes,
  resamples or re-encodes anything;
- **play reports** (setting, on by default): what you play is reported to
  Spotify, and shows as playing on this device in the Spotify apps;
- **Spotify Connect** (setting, on by default): the player shows up as a
  device in the Spotify apps (phone, desktop), wherever it is, and can be
  controlled from them.

Implementation details: [docs/plugin.md](docs/plugin.md).

> Unofficial Spotify client, built on [librespot](https://github.com/librespot-org/librespot).
> It uses Spotify's services outside their terms of use. A Spotify
> **Premium** account is required. Spotify may break it at any time, or
> restrict the accounts that use it. Use at your own risk.

**Quality**: Ogg Vorbis, 320 kbps at most (setting "Streaming quality":
High = 320 kbps, Normal = 160 kbps). Spotify's lossless streams are not
available through this path. Spotify streams at 44.1 kHz only: an output
that is bit-perfect and does not take 44.1 kHz gets no Spotify tracks.

## Installation

```sh
cargo install --path .         # → ~/.cargo/bin/spconnect
```

Build dependencies: a Rust toolchain and a C compiler (for the TLS
library). No OpenSSL: TLS is rustls.

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
id = "spotify"
command = "/path/to/.cargo/bin/spconnect"
args = ["plugin"]
enabled = true
```

## Usage

1. In the player's plugin settings: Spotify → **Sign in**. The Spotify page
   opens in your browser.
   - Browser on the same machine: sign-in completes by itself.
   - Browser elsewhere (phone…): the browser ends on a page that does not
     load. Copy its full address (it starts with `http://127.0.0.1` and
     contains `code=`) and paste it into the player.
2. The Spotify catalogue shows up in the player (browsing and search).
3. In a Spotify app: devices icon → **<player name> (<machine name>)**.
   What you start from the app plays in the player. If you start something
   else directly in the player, the app sees the device go idle.

## Data

In the directories the player gives the plugin (`data_dir`, `cache_dir`):

| File | Contents |
|---|---|
| `data_dir/credentials.json` | Reusable Spotify credentials and the sign-in's refresh token (never the password), mode `600` |
| `data_dir/device.json` | Device id (Spotify Connect, sessions) |
| `data_dir/play-reports.json` | Plays not reported yet (only while some wait) |

`credentials.json` gives access to your account, like a cookie. Signing out
deletes it. Nothing is written to `cache_dir`: audio is never stored.

## Troubleshooting

The plugin logs to stderr, which the player copies into its own log. More
detail: `RUST_LOG=spconnect=debug` in the player's environment. The log
never holds the account name, the user id or a token, whatever `RUST_LOG`
says.

- **`audio key refused` in the log, tracks skipped as unavailable**:
  Spotify's servers intermittently refuse the decryption key of a track
  (`error audio key 0 1` in librespot's words). spconnect asks three times,
  a second apart, before giving up on the track; playing it again later
  usually works. If it happens for every track, the account may be
  restricted, or Spotify has changed something librespot has not caught up
  with yet.
- **`rate limited` errors, empty pages**: the Web API limits requests per
  client. spconnect passes `Retry-After` on to the player, which backs off.
  Sections the Web API no longer serves to this client (new releases,
  editorial playlists, recommendations, related artists) are left out or
  taken from the session's own services.
- **The device does not show up in the apps**: look for `Spotify Connect:
  device announced` in the log, and check the "Show the player in Spotify
  Connect" setting. The apps must be signed in to the same account.
- **"Spotify Free accounts are not supported"** at sign-in: playback
  through librespot needs Spotify Premium. The account is checked before
  any session is opened, and signed out if it is not Premium.
- **`stored credentials refused`, the player shows the account as expired**:
  sign in again.

## Tests

```sh
cargo test                       # unit tests + fake host against a mock Web API
```

Parts that need a live Spotify session (access point, audio keys, CDN,
dealer) are not reached by these tests. Against Spotify, with the data
directory of a signed-in plugin:

```sh
SPCONNECT_DATA=/path/to/data_dir SPCONNECT_TRACK=<track id> SPCONNECT_OUT=/tmp/t.ogg \
  cargo test live_resolve -- --ignored --nocapture
```

resolves the track, downloads it through the relay and writes the Ogg file
(`ogginfo /tmp/t.ogg` checks it).

## Architecture

| Module | Role |
|---|---|
| `plugin/mod.rs` | JSON-RPC loop, `initialize`/`shutdown`, `auth.*`, settings, Spotify Connect start-up |
| `plugin/rpc.rs` | Line-based JSON-RPC 2.0, requests to the host, error codes |
| `plugin/catalog.rs`, `plugin/items.rs` | `browse.*`, `library.*`, `search`, `item.get`, `favorites.set`, `radio.next`; Web API answers → items |
| `plugin/playlists.rs`, `plugin/details.rs`, `plugin/lyrics.rs` | Playlist edits, `item.details`, `lyrics.get` |
| `plugin/resolve.rs`, `plugin/relay.rs` | `track.resolve`: file choice, relinking; local relay decrypting AES-128-CTR on the fly, with ranges |
| `plugin/report.rs` | Play reports to Spotify's event service |
| `audiokey.rs` | Audio keys, retried |
| `connect/` | Spotify Connect device: dealer commands → `player.*`, `player.state` → connect-state |
| `session.rs` | OAuth (PKCE, loopback redirect), stored credentials, librespot session, Web API tokens |
| `probe.rs` | First connection in a child process: free accounts are refused without ending the plugin |
| `webapi.rs` | Web API client: token renewal, rate limits |
