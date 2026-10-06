# spconnect: how the plugin works

spconnect is a source plugin (protocol v1): a process started by the host
player, speaking JSON-RPC 2.0 over stdin/stdout, one JSON message per line.
Logs go to stderr. It is built on librespot as a library
(`librespot-core`, `librespot-metadata`, `librespot-audio`'s pieces): the
session, its access tokens, metadata, audio keys, CDN addresses and the
dealer come from there. librespot's player and its Connect logic
(`librespot-playback`, `librespot-connect`'s Spirc) are not used, since
they decode to PCM and own the queue: here the host decodes and plays.

```
host ──(JSON-RPC over stdin/stdout)──► spconnect plugin
                                         ├─ auth.*        → accounts.spotify.com (OAuth, PKCE)
                                         ├─ browse.*, search, library.*, item.*, playlists.*
                                         │                → Web API (api.spotify.com/v1), session services
                                         ├─ track.resolve → metadata, audio key, CDN address
                                         ├─ playback.*    → event service (play reports)
                                         └─ player.*  ◄── Spotify Connect (dealer, connect-state)
host ──(HTTP)──► 127.0.0.1 relay ──(HTTP, ranges)──► Spotify CDN
                 (AES-128-CTR decrypted on the fly, Ogg Vorbis unchanged)
```

Advertised capabilities: `auth`, `browse`, `search`, `resolve`,
`favorites`, `reporting`, `remote_control`, `library`, `lyrics`, `radio`,
`details`, `playlist_edit`.

The plugin never writes the account name, the user id, the e-mail address
or a token to its log (stderr), nor to the account it reports to the host:
hosts keep both, in their logs and diagnostic reports. librespot logs the
account name when a session opens, and tokens at trace level: those
targets are capped (`librespot_core::session=warn`, `login5`, `token`,
`dealer`… at `info`) whatever `RUST_LOG` asks. The account shows its
display name and its subscription.

## Start-up

`initialize` provides:
- `data_dir`: credentials, device id, unsent play reports;
- `output`: what the output plays natively. Spotify streams 44.1 kHz only:
  a bit-perfect output whose rates leave 44.1 kHz out gets `unavailable`
  for every track;
- `locale`: the host's interface language, for every label the plugin
  writes (French, else English). On `locale.changed {locale}` the plugin
  switches and declares its settings again (`settings.declared`);
- `host.name`: Spotify Connect device name, "<host.name> (<machine name>)";
- `settings`: the values stored for the plugin's settings (below).

## Settings

Declared in the `initialize` result, labelled in the host's language. All
apply without restarting the plugin (`settings.changed`).

| Key | Type | Default | Effect |
|---|---|---|---|
| `quality` | choice `high` / `normal` | `high` | Ogg Vorbis 320 kbps (Premium), else 160 kbps first |
| `report_playback` | bool | `true` | Play reports (below) |
| `connect` | bool | `true` | The player appears as a Spotify Connect device |

## Sign-in (`auth.*`)

| Method | Implementation |
|---|---|
| `auth.status` | `credentials.json`, then `GET /me`: `signed_in`; `expired` when Spotify refuses the credentials or the refresh token; `signed_out` without credentials. A network failure keeps `signed_in`. |
| `auth.begin` | OAuth authorisation code with PKCE (S256), client id of librespot's desktop client, redirect `http://127.0.0.1:<port>/login` on a free port, a random `state`. Answers `expects_input: true`. The loopback listener waits 15 minutes; a redirect whose `state` matches completes the sign-in and sends `auth.changed`. |
| `auth.complete` | Takes the code from the pasted address (or the bare code), trades it for tokens at `/api/token` with the PKCE verifier, opens a session with the access token, and stores the reusable credentials the session hands out |
| `auth.sign_out` | Closes the session, leaves Spotify Connect, deletes `credentials.json` |

`credentials.json` (mode 600) holds the session's reusable credentials
(librespot's "stored credentials") and the sign-in's refresh token. The
access point refreshes the former as sessions open; the latter is a
fallback for the Web API. When a request is refused for authentication
mid-session, the plugin sends `auth.changed {state: "expired"}` once.

## Tokens and the Web API (`webapi.rs`)

Web API requests carry, in this order of preference:
1. the session's login5 token;
2. an access token from the sign-in's refresh token (`/api/token`);
3. the session's older token service (keymaster), with the plugin's scopes.

A `401` renews the token once (from another source if the same one comes
back) and sends the request again. A `429` becomes `rate_limited` with
`Retry-After` (capped at an hour by the protocol). Pages hold at most 50
items: a protocol page of up to 200 takes several requests.

The Web API changed for third-party clients in late 2024 and early 2026
(editorial playlists, recommendations, related artists, new releases and
some batch endpoints withdrawn; playlist `tracks` renamed `items`; generic
library endpoints). spconnect uses the classic endpoints, and where one
answers `403`, `404`, `405`, `410` or `501`:

| Need | Fallback |
|---|---|
| A playlist's tracks | `/playlists/{id}/items`, then the session's playlist service (`playlist/v2`), then track metadata |
| Several tracks at once (`/tracks?ids=`) | the session's metadata service, one track each, 8 at once |
| Library state (`/me/tracks/contains`…) | `/me/library/contains?uris=` |
| Save, remove (`PUT /me/tracks`…) | `PUT`/`DELETE /me/library?uris=` |
| Radio | session first: a track's "inspired by" mix, an album's or artist's autoplay context; then `/recommendations` |
| Related artists | the session's artist metadata |
| New releases, Made for you | section left out (checked once an hour) |

## Catalogue

| Ref | Source |
|---|---|
| `library` | folder: `liked`, `my/albums`, `my/artists` |
| `liked` | `/me/tracks` |
| `my/albums` | `/me/albums` |
| `my/artists` | `/me/following?type=artist` (cursor paging; the cursor of every 50th artist is kept so pages asked in order cost one call) |
| `my/playlists` | `/me/playlists` |
| `made-for-you` | the playlists of `/me/playlists` owned by Spotify (Discover Weekly, Release Radar, Daily Mix… the ones the account follows) |
| `recent` | `/me/player/recently-played` (the last 50 plays, each track once) |
| `top/tracks`, `top/artists` | `/me/top/tracks`, `/me/top/artists` |
| `new-releases` | `/browse/new-releases` |
| `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<id>` | `/tracks`, `/albums` (+ `/tracks`), `/artists/{id}/albums`, `/playlists/{id}/tracks` |
| `radio/<track|album|artist>/<id>` | see Radio |
| `related/<artist id>` | related artists |

`browse.root`:
- `sections`: Library · My playlists · Made for you · Recently played ·
  Your top tracks · Your top artists · New releases;
- `home`: Recently played · Made for you · top tracks · top artists · New
  releases.

Sections the API does not serve are left out of both.

**Item fields:**
- `format`: `{codec: "vorbis", sample_rate: 44100}` (informative);
- `album_ref`, `artist_ref`: from the track's album and first artist;
- `favorite`: in the library. Items of the library lists are; others are
  asked by fifties (`/me/tracks/contains`, `/me/albums/contains`,
  `/me/following/contains`) and remembered for 10 minutes;
  `favorites.set` updates them at once. Absent when unknown;
- `entry_id` (tracks of a playlist): `<position>:<track id>`;
- `editable` (playlists): the owner's id is the account's;
- `actions`: radio from a track, album or artist; related artists of an
  album's artist or of an artist;
- `playable: false` when Spotify says `is_playable: false` or names a
  restriction.

`search` asks `/search` once for all the kinds requested; Spotify's
`null` entries are skipped. `favorites.set` takes tracks, albums, artists
and playlists (follow).

## Library (`library.*`)

`library.albums`, `.artists`, `.tracks`, `.playlists` are `my/albums`,
`my/artists`, `liked` and `my/playlists`.

## Playlist edits (`playlists.*`)

| Method | Request |
|---|---|
| `playlists.create {name, description?, public?}` | `POST /me/playlists` (`/users/{id}/playlists` if missing) |
| `playlists.rename {ref, name}` | `PUT /playlists/{id}` |
| `playlists.delete {ref}` | `DELETE /playlists/{id}/followers` (Spotify has no deletion: the owner unfollows), else `DELETE /me/library` |
| `playlists.add {ref, items}` | `POST /playlists/{id}/tracks` `{uris}`, by hundreds |
| `playlists.remove {ref, entries}` | `DELETE /playlists/{id}/tracks` `{tracks: [{uri, positions}], snapshot_id}` |
| `playlists.move {ref, entry, to}` | `PUT /playlists/{id}/tracks` `{range_start, insert_before, range_length: 1, snapshot_id}` |

Where `/tracks` answers 404, 405 or 410, the request goes to `/items`.
Every edit of an existing playlist first reads it and refuses (`-32602`)
unless its owner is the account. Spotify checks that each position holds
the track named, so an entry that moved meanwhile is refused rather than
the wrong track removed. A move to `to` (final position, 0-based) sends
`insert_before = to` upwards, `to + 1` downwards; past the end means last.

## Details (`item.details`)

| Ref | Content |
|---|---|
| `artist/<id>` | biography (session metadata, HTML stripped); top tracks (`/artists/{id}/top-tracks`, else the session's per-country list); related artists |
| `album/<id>` | label, release date, tracks, genres, ℗ and ©; more by the artist |
| `track/<id>` | credits from the apps' credits page (`track-credits-view`), one fact per person and role |

## Lyrics (`lyrics.get`)

The session's lyrics service, `color-lyrics/v2/track/<id>`: `LINE_SYNCED`
lines become `synced` (`♪` lines become empty), `UNSYNCED` ones `plain`.
No lyrics (404): `not_found`, remembered for 6 hours. Without a session:
`not_found`, so the host goes on with its own sources.

## Resolution (`track.resolve`)

1. **Metadata** (session, `extended-metadata`): files, availability,
   alternatives. A track not available here (region, not released yet) is
   relinked to the first alternative that is, as the apps do; else
   `unavailable` with the reason ("not available in your country").
2. **File**: `quality=high`: Ogg Vorbis 320, 160, 96 kbps in that order;
   `normal`: 160, 96, 320. No Ogg Vorbis file: `unavailable`. MP3 and AAC
   files are not used.
3. **Audio key** (`audiokey.rs`): asked up to 3 times, 1 s apart, on a
   refusal or a timeout. Spotify intermittently refuses keys it hands out
   a moment later (`error audio key 0 1`). After the third refusal:
   `unavailable`, "Spotify refused the audio key of this track 3 times in a
   row… it may play again later".
4. **CDN address** (`storage-resolve`), then the first kilobyte of the file:
   its size (`Content-Range`), a check that the Ogg stream starts after
   Spotify's header (a wrong key shows there), and the normalisation data.
5. **Relay** registration.

Answer:
- `url`: `http://127.0.0.1:<port>/<token>/<track id>.ogg`;
- `delivery: "proxied"`;
- `format: {codec: "vorbis", sample_rate: 44100, channels: 2}`;
- `duration_ms` from the metadata;
- `replaygain: {track_gain, track_peak, album_gain, album_peak}` from the
  normalisation data Spotify puts in the file's header (four little-endian
  `f32` at offset 144), when present and sane;
- `expires_at`: 6 hours, minus a minute.

### The relay (`plugin/relay.rs`)

Spotify's files are AES-128-CTR encrypted with the audio key, a fixed IV,
and a 128-bit big-endian counter running from the first byte of the file.
The first 0xa7 bytes, decrypted, are Spotify's header; the Ogg file
follows.

- `Content-Length` = file size − 0xa7, `Accept-Ranges: bytes`;
- `Range: bytes=a-b` → one CDN request for `a+0xa7 … b+0xa7`, decrypted as
  it streams (the counter is set to that offset: CTR decrypts from
  anywhere), `206` with `Content-Range`; `bytes=-n`, open ranges and `HEAD`
  too; an unsatisfiable range answers `416`;
- tokens are random (UUID v4) and forgotten after 6 hours or once 32
  streams are registered; an unknown token answers `410`, and the host
  resolves the track again;
- a CDN address that expires or is refused (`403`, `404`, `410`) is asked
  for again through the session, once per request;
- nothing is cached or written to disk; CDN addresses are never logged.

## Play reports (`reporting`)

The capability is always declared; the `report_playback` setting decides
whether anything is sent.

- `playback.started {ref}` notes the start (and the file played, from the
  resolution: a relinked track is reported as the alternative played).
- `playback.ended {ref, listened_ms, reason}` queues a play: `ms_played`
  (at most the track's length; under a second is not reported),
  `reason_end` = `trackdone` (ended), `fwdbtn` (skipped), `endplay`
  (stopped); `reason_start` = `trackdone` after a play that ended,
  `clickrow` otherwise.
- Queued plays are sent, 20 at a time, to the event service
  (`gabo-receiver-service/v3/events`) as `RawCoreStream` events in a
  `PublishEventsRequest`, with the context fragments the desktop client
  sends (client id, installation id, time, SDK). They wait in
  `data_dir/play-reports.json` (500 at most) until sent. Turning the
  setting off, or signing out, drops them.

The service answers which events it refused: final refusals are logged
(`play report refused by Spotify (reason …)`), transient ones kept for
later. **Limits**: these events are not documented; their fields follow
the message definitions shipped with librespot (taken from the desktop
client), not traffic of current clients. Whether Spotify counts them
towards royalties, "recently played" or recommendations is not verified.

With Spotify Connect on, play reports also make a Spotify track the user
starts in the player itself show as playing on this device in the Spotify
apps (see below), as the desktop app shows what it plays.

## Spotify Connect (`connect/`)

**Presence.** Once signed in (at `initialize` or after a sign-in), with
the `connect` setting on, the plugin opens the session's dealer, waits
for its connection id, and announces the device (`PUT
connect-state/v1/devices/<device id>`, reason `NEW_DEVICE`): a computer
named "<host.name> (<machine name>)" that can play tracks, takes
transfers, commands and volume. Its device id is stable (`device.json`).
It leaves (`DELETE` of its state) on `auth.sign_out`, when the setting is
turned off, and on `shutdown`. When the session dies, it joins again with a
new one (5 s, then doubling up to 5 minutes).

**Commands** (`commands.rs` decides, `mod.rs` acts):

| Command | Host requests |
|---|---|
| `play` (context URI, or a list of tracks; `skip_to` uid/URI/index, `seek_to`, `initially_paused`, shuffle and repeat overrides) | context resolved (`context-resolve`, pages followed, 1000 tracks at most) into a queue; `player.play {items, start}` with up to 20 tracks before and 100 after the current one; `player.seek` and `player.pause` once it has started |
| `transfer` | the same, from the other device's state: context, current track (or the queue's first when it was playing the queue), position moved on by the time the transfer took, paused or not, options, queued tracks |
| `pause`, `resume`, `seek_to` | `player.pause`, `player.resume` (or playing the queue again when idle), `player.seek` |
| `skip_next`, `skip_prev` | `player.next`, `player.previous`; `skip_next` to a named track plays the queue from it |
| `add_to_queue` | `player.enqueue {at: "next"}` |
| `set_queue` (the apps' queue editor) | the upcoming tracks replaced; the host gets the queue again from the current track, at its position |
| `set_shuffling_context` | the upcoming tracks shuffled (or put back in order); same as above |
| `set_repeating_context`, `set_repeating_track`, `set_options` | stored and reported; repeat-context plays the queue again from the start when the host reaches its end |
| volume (`connect/volume`) | `player.set_volume` (Spotify's 0–65535 → percent) |
| `update_context` | accepted, ignored |

Each command is answered (`success` or failure) to the app that sent it,
and the state is sent at once.

**From the host.** `player.state` moves the queue along (the entry
playing is the one nearest after the current one in what the host holds),
gives position, duration, status and volume; the state is sent to
Spotify when the track, the status or the volume changes, or the position
jumps (a seek), grouped over 300 ms. When the host nears the end of what it
holds, 50 more tracks are enqueued at the end. `player.taken_over` (the
user played something else in the player) makes the device inactive
(`PUT …/inactive`). When another device becomes the active one, the host
is paused and the device goes inactive.

**Protocol v1 limits:**
- **No way to remove a queued item.** Editing the queue or shuffling hands
  the host the queue again from the current track: a short gap.
- **Queue order.** "Add to queue" puts the track right after the current
  one (the host's "play next"), not after the tracks queued before it.
- **Repeat-one** is reported to the apps but the host's own repeat
  setting decides.
- **Early `player.taken_over`**: one received within 3 s of the plugin's
  own `player.play` is ignored: some hosts report it while they replace
  the queue.

## Errors

| Cause | Protocol code |
|---|---|
| Not signed in; credentials or refresh token refused; HTTP 401 after renewal | `auth_required` (-32001) |
| HTTP 404, 410; unknown `ref`; no lyrics | `not_found` (-32002) |
| HTTP 403; track not available (region, no Ogg file, audio key refused); no session; output without 44.1 kHz | `unavailable` (-32003) |
| HTTP 429 | `rate_limited` (-32004), `data.retry_after` from `Retry-After` (30 s otherwise) |
| HTTP 5xx, connection error or timeout | `network` (-32005) |
| Edit of a playlist the account does not own, bad params | `-32602` |

librespot's own errors map by kind: `NotFound` → `not_found`,
`Unauthenticated` → `auth_required`, `PermissionDenied`, `Unavailable`,
`FailedPrecondition` → `unavailable`, `ResourceExhausted` →
`rate_limited`, `DeadlineExceeded`, `Aborted` → `network`.

## Tests

`tests/plugin.rs` runs `spconnect plugin --api-base <mock>/v1
--accounts-base <mock> --no-session` (hidden options) against a mock Web
API and accounts service. `--no-session` keeps the plugin off Spotify's
access points: the Web API is used with the OAuth token.

Covered:
- handshake, capabilities and settings, `signed_out`;
- sign-in by paste and through the loopback redirect (PKCE checked by the
  mock, forged `state` ignored), `credentials.json` mode 600, sign-out, a
  revoked refresh token (`expired`);
- browse root (a withheld section left out), library lists (cursor paging
  of followed artists), album, artist and playlist pages, search groups,
  `item.get`, favourite states and `favorites.set` (with the generic
  endpoint as fallback), radio (recommendations), album details;
- playlist edits, request bodies included, and refusal on a playlist the
  account does not own;
- errors: rate limiting with `retry_after`, unknown refs, withheld
  playlists, resolve and lyrics without a session, token renewal and
  `auth.changed {expired}`;
- the log never holds the account id, e-mail, code or a token.

Unit tests cover the relay (CTR decryption at arbitrary offsets, header
skip, range parsing, a whole relay against a fake CDN), audio-key retries,
item mapping from recorded answers, Connect command mapping (play,
transfer, queue edits, options), queue windows, the device state,
lyrics and credits conversion, play report events (protobuf encoding,
refusal parsing), settings, PKCE.

Not covered without Spotify: the access point, login5, audio keys, the
CDN, the dealer and connect-state, the event service. `live_resolve`
(`#[ignore]`, see the README) checks resolution and the relay against
Spotify with a signed-in data directory.

## Risks

- **Terms of use**: spconnect uses Spotify's private services through
  librespot; Spotify may block it or restrict the accounts that use it.
- **librespot**: login5, audio keys or the access point protocol change
  from time to time (audio key refusals, login5 outages); fixes come with
  new librespot versions.
- **Web API**: shared client id, rate limits, endpoints withdrawn for
  third-party clients; the fallbacks above cover the known ones.
- **Play reports**: best effort, unverified (see above).
- **Spotify Connect**: the state messages follow librespot-connect's;
  Spotify's apps may expect fields it does not send.
