# Architecture

How the pieces fit together, and why.

## Module layout

```
src/
├── main.rs               # entry point, framework setup, HTTP server spawn
├── data.rs               # Data struct — per-guild configs + Arc handles
├── handlers.rs           # serenity EventHandler
├── logging.rs            # tracing init, per-command timing
├── reply.rs              # tracked-message helpers (live "now playing" slot)
├── commands.rs           # /ping
├── status.rs             # /status — build info + uptime
├── music.rs              # backend-agnostic music commands
├── music_backend.rs      # MusicBackend trait + Track + BackendKind
├── lavalink.rs           # LavalinkBackend impl + /lavalink admin commands
├── native_backend.rs     # NativeBackend impl (songbird driver + yt-dlp)
├── playlist.rs           # PlaylistStore trait + YAML impl + /playlist
├── record.rs             # voice capture → per-user Ogg → zip
├── tts.rs                # TtsClient + /tts commands
├── stt.rs                # SttBackend trait + HTTP impl + /stt commands
├── radio.rs              # Cross-guild voice bridging ("radio")
└── audio_http.rs         # AudioStore + axum server for URL-relay to lavalink
```

Every voice/music module is gated behind a feature flag (see `Cargo.toml`).
The default build is `music + lavalink`.

## Feature tree

```
voice            = dep:songbird                                      # base voice
music-core       = (empty)                                           # trait + commands

lavalink         = voice + dep:lavalink-rs
native           = voice + songbird/{driver, builtin-queue, tungstenite}
                         + dep:reqwest + dep:symphonia

music            = music-core + lavalink + dep:serde_json
music-native     = music-core + native + dep:serde_json
playlists        = music-core
voice-recv       = voice + songbird/{driver, receive, tungstenite}
record           = voice-recv + dep:ogg + dep:zip
tts              = native + dep:axum
stt              = record + dep:reqwest + dep:serde_json
radio            = voice-recv + songbird/builtin-queue
```

`music` and `music-native` are composable. With both on, `MUSIC_BACKEND`
picks the active backend at startup.

## The `MusicBackend` trait

`Data::music` is `Arc<dyn MusicBackend>`. All `/play`, `/skip`, etc. go
through the trait; command code never names a concrete backend.

```rust
trait MusicBackend: Send + Sync + 'static {
    fn kind(&self) -> BackendKind;
    async fn on_ready(&self, ctx, user_id) -> Result<(), Error>;
    async fn ensure_joined(&self, ctx, guild, channel) -> Result<bool, Error>;
    async fn leave(&self, ctx, guild) -> Result<(), Error>;
    async fn play(&self, guild, query, requester) -> Result<PlayResult, Error>;
    async fn play_url(&self, guild, url, requester) -> Result<PlayResult, Error>;
    async fn skip/stop/pause/resume(&self, guild) -> …;
    async fn now_playing/queue_snapshot(&self, guild) -> …;
}
```

`kind()` lets commands branch for backend-specific behavior without
downcasting (see **TTS routing** below). `play_url` is separate from `play`
because `play` routes through yt-dlp on the native backend, which is wrong
for raw audio URLs — `play_url` uses songbird's `HttpRequest` input source
instead.

### Lavalink backend

`LavalinkBackend` owns a `LavalinkClient` and the current `LavalinkConfig`.
It bridges songbird's gateway session to the lavalink Java node:

1. `songbird.join_gateway(guild, channel)` — songbird establishes the
   voice gateway session and returns `ConnectionInfo` (endpoint, token,
   session id).
2. We manually convert that `ConnectionInfo` to lavalink-rs's own
   `ConnectionInfo` type and call `create_player_context`.
3. From then on, the lavalink node owns the voice UDP. Songbird is
   effectively idle on that call.

Step 2 is a workaround: lavalink-rs 0.15 ships with a `songbird` feature
that expects songbird 0.5's types, but we need songbird 0.6 for DAVE
(Discord's mandatory E2EE). We disable lavalink-rs's `songbird` feature
and copy the three fields manually.

Track metadata round-trips via `user_data` so the requester's Discord ID
survives through lavalink.

### Native backend

`NativeBackend` plays in-process through songbird's driver. Resolution
goes through `songbird::input::YoutubeDl`, which shells out to the
`yt-dlp` binary via the processes PATH.

Songbird's queue only exposes event hooks, not metadata slots, so we
maintain a parallel `DashMap<GuildId, GuildMeta>` for `now_playing` and
`queue_snapshot`. Sync is driven by a single `TrackEvent::End` handler
attached per-track (`AdvanceOnEnd`) — when songbird's driver advances,
our metadata advances. `skip`/`stop` mutate songbird's queue and allow
the same handler flow update our mirror rather than racing it.

## TTS routing

The hard constraint: **Discord only allows one voice UDP session per
guild**. When lavalink owns that session, songbird can't push bytes
through its local mixer, so the "fetch bytes, `enqueue_input`" pattern
used in `tts::speak`'s native path would silently drop the audio.

To play TTS through lavalink we instead hand lavalink a URL it can fetch
itself:

```
   tts-service          Bot                      Lavalink node
   (HTTP svc)           (this bot)               (remote)
       │                   │                         │
       │ 1. GET /tts       │                         │
       │◀──────────────────│                         │
       │ bytes + ct        │                         │
       │──────────────────▶│                         │
       │                   │ 2. AudioStore::put ──┐  │
       │                   │                      │  │
       │                   │ 3. play_url(url)     │  │
       │                   │─────────────────────────▶
       │                   │                      │  │ 4. GET /audio/:id
       │                   │◀──────────────────────────
       │                   │ 5. audio/wav bytes   │  │
       │                   │──────────────────────────▶
       │                   │                      │  │ 6. decode + mix + send
```

The URL the bot hands to lavalink is built from `BOT_PUBLIC_URL`. In
production that's a tunnel (cloudflared, ngrok) or reverse-proxy hostname
pointed at the bot's HTTP layer; lavalink's HTTP source pulls from it,
decodes, and mixes into the voice session it already owns.

`AudioStore` (in `src/audio_http.rs`) is a `DashMap<String, AudioEntry>`
with a 60s TTL and a background sweeper. An embedded axum server on
`BOT_HTTP_BIND_ADDR` (default `0.0.0.0:8080`) exposes `GET /audio/{id}`
and a `/health` endpoint. The server only starts if `BOT_PUBLIC_URL` is
set. Without it there's nothing for remote pullers to reach, and there's
no point in wasting the CPU time and burning the port.

On the native backend, TTS stays on the simpler bytes path: synthesize,
`enqueue_input(Input::from(bytes))` — no HTTP server needed.

The branch lives in `tts::speak`, keyed on `ctx.data().music.kind()`.

## Speech-to-text

Gated by the `stt` feature. The trait abstraction (`SttBackend`) gives us a
seam to add non-OpenAI-shape providers later, but for now there's one impl:
`HttpSttBackend`, which speaks the OpenAI `/v1/audio/transcriptions` wire
contract. That single implementation covers:

- **API providers**: OpenAI, [lemonfox.ai](https://lemonfox.ai), Groq,
  Fireworks, DeepInfra — all speak the same multipart format.
- **Self-hosted**: [Speaches](https://github.com/speaches-ai/speaches)
  (`ghcr.io/speaches-ai/speaches`) — runs `faster-whisper` behind the same
  OpenAI contract. Bundled as an optional compose service under the `stt`
  profile.

Switching providers is an `STT_BASE_URL` / `STT_API_KEY` / `STT_MODEL`
change, not a rebuild. Config persists to `config/stt.yaml` and is
runtime-mutable via `/stt set`.

`/stt transcribe` accepts any Discord-supported audio attachment
(wav/mp3/ogg-opus/m4a/flac/webm — exactly the formats Whisper accepts, and
conveniently a superset of what `/record` produces). Long transcripts get
truncated-inline with a `.txt` attachment for the full text, so Discord's
2000-char content limit doesn't clip real conversations. The
`transcribe_message` context-menu variant (`Apps → Transcribe attachment`)
provides the same flow for messages already in the channel.

## Radio (cross-guild voice bridging)

Gated by the `radio` feature. The goal is "bot A's voice channel can be
heard in bot B's voice channel" when both channels live on the same bot
instance (one bot in many guilds). No bot-to-bot transport — audio flows
over Discord's voice servers on both legs, so the bot never exposes a
public voice endpoint and no IP leaks.

Side note on decode mode: songbird's default `DecodeMode::Decrypt` only
returns RTP payload on `VoiceTick` (what `/record` wants). Radio needs
PCM samples, so when the `radio` feature is compiled in, `main.rs`
registers songbird with `DecodeMode::Decode` instead. `/record` keeps
working because it pulls from the raw packet, which is still populated
under either mode.

### Pipeline

```
 guild A VC users
       │
       ▼
 songbird driver (DecodeMode::Decode)
       │  per-SSRC decoded PCM (i16 interleaved stereo @ 48 kHz)
       ▼
 BroadcastHandler (per-guild source call)
       │  mix all speakers → one 20 ms PCM frame
       ▼
 tokio::sync::broadcast<Arc<[i16]>>     ← RadioStation.tx
       │ fan-out to N listeners
       ▼
 forwarder task (per listening guild)
       │  i16 → f32le conversion
       ▼
 tokio::io::duplex (small buffer — low latency)
       ▼
 PcmStream (AsyncRead + AsyncSeek + AsyncMediaSource)
       ▼
 AsyncAdapterStream (async → sync MediaSource bridge)
       ▼
 RawAdapter (tells songbird "this is 48 kHz stereo f32le PCM")
       ▼
 Input → songbird Call (guild B) → Discord voice server → guild B VC users
```

Each broadcast is named; destination guilds subscribe by name via
`/radio tune <name>`. Subscriptions are keyed per-guild (one tuning per
guild at a time) and tracked in `Data::radio_subscriptions`. Dropping a
subscription aborts its forwarder task via `JoinHandle::abort`, which
closes the duplex pipe on drop; songbird's track ends, the mixer stops
feeding bytes to guild B. Clean shutdown, no zombies.

### Privacy & opt-in

Broadcasting requires `GuildConfig.radio_broadcast_enabled` (set by
admin-only `/radio enable`). The default is `false`, hard opt-in: no
guild can accidentally start re-broadcasting its voice channel. The
broadcast-start command posts a public announcement so users in the
source channel aren't surprised. Tuning in is unrestricted — from
Discord's perspective, a listener bot playing remote audio is
indistinguishable from `/play`.

### Limits

- Intentional: single-bot only. Bridging between two separately-operated
  bots (different Discord apps) would need a WebSocket transport with
  mutual auth. The `broadcast::Sender`/`Receiver` boundary is the seam
  where such a transport would plug in if someone wants it later.
- One concurrent tuning per listener guild. Switching stations
  implicitly unplugs the previous one.
- No DTX / silence compression: we always forward a full 20 ms frame
  even when no one is speaking. Keeps the listener pipeline fed at
  exactly 50 Hz — gaps would show up as audible gaps.
- Two encode hops (Discord → decode in bot, bot → re-encode on send).
  Quality loss is the same as any bot that re-encodes audio; not
  noticeable in practice over voice bandwidth.

## Voice recording

Gated by the `record` feature (pulls `voice-recv`, which needs songbird's
driver + receive features). When `/record start` runs:

- Songbird installs a `RecorderHandler` on the call for `VoiceTick` and
  `SpeakingStateUpdate` events.
- `VoiceTick` fires per-SSRC with 20ms Opus frames. Each SSRC gets its own
  `OpusOggWriter` (wrapping the `ogg` crate's `PacketWriter`), which emits
  a per-file OpusHead + OpusTags header pair and appends each frame as an
  Ogg packet with a 960-sample granule step.
- `SpeakingStateUpdate` maps SSRC → Discord user id as speakers join.
- `/record stop` closes the writers (each writer's `Drop` emits an EOS
  packet), then runs `zip_session_files` on a `spawn_blocking` pool. The
  resulting zip is uploaded to the original channel if under Discord's
  attachment size limit, otherwise it's saved to disk.

Guilds opt out via `/record disable` (persisted on `GuildConfig`); the
check runs in `/record start`.

## Playlists

`PlaylistStore` is a trait, so the YAML impl (`YamlPlaylistStore`) can be
swapped for SQLite or Redis without touching command code. Files live at
`config/playlists/{owner_id}__{slug}.yaml` — owner in the filename so the
list command can filter by owner without reading all files.

Tracks round-trip through `music_backend::Track`, so playlists persist
title/author/URI even when a future backend can't re-resolve the URI. The
`featured` flag is a per-playlist marker the bot respects across resaves.

## The reply layer

`reply::send` with a `Reply` builder is the canonical way to answer a
command. Live bot-sent messages are tracked per `(GuildId, Slot)` in
`Data::tracked_messages`, so repeatedly calling `/play` replaces the
previous "now playing" embed rather than stacking new messages. Slots:
`NowPlaying`, `QueueView`, `Status`, `BotStatus`.

For prefix commands, `delete_invoker(true)` removes the user's `!play …`
message after the bot responds, keeping the channel clean.

## Persistence

`Data::load()` reads `config/bot_config.yaml`, `config/lavalink.yaml`,
`config/tts.yaml` on startup. `Data::save()` rewrites them on `Ctrl+C` and
on every admin `/…/set`. Missing files fall back to defaults from env +
hard-coded defaults.

Playlist files live under `config/playlists/` and are written individually
per-save rather than batched, so concurrent writes don't contend.

## Logging

`logging::init` wires three subscribers:

- Console (human-readable, `RUST_LOG` filter).
- `logs/commands.json` — one JSON line per `pre_command`/`post_command`
  hook, with per-invocation timing tracked via a
  `DashMap<CommandInvocationId, Instant>` keyed on `ctx.id()` (not thread
  id — the framework moves futures across threads).
- `logs/events.json` — serenity event lifecycle.

All targets are `bot_template_rs::*`, so the `RUST_LOG` filter can tune
each subsystem independently (e.g. `lavalink_rs=info,serenity=warn`).

## Docker

`Dockerfile` is multi-stage with `cargo-chef` for cached dependency
compilation. The runtime stage installs `yt-dlp` + `ffmpeg` + `tini` — both
are used by the native music backend (songbird's `YoutubeDl` input shells
out to `yt-dlp` and pipes through `ffmpeg`).

`docker-compose.yml` runs the bot alongside a `gnomeddev/tts-service`
container (layered via `Dockerfile.tts` to bake in a dummy gcloud
service-account JSON — tts-service initializes every mode at startup and
panics without credentials, even for eSpeak-only deployments).

Port `8080` on the bot container is published so an outside cloudflared or
reverse proxy can reach the audio HTTP layer.
