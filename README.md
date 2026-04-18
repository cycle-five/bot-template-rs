# bot-template-rs

A feature-rich Discord bot template in Rust, built on
[`serenity`](https://github.com/serenity-rs/serenity) +
[`poise`](https://github.com/serenity-rs/poise) +
[`songbird`](https://github.com/serenity-rs/songbird), with optional
[`lavalink-rs`](https://gitlab.com/vicky5124/lavalink-rs) integration. Every
major capability is behind a cargo feature flag, so you can build a tiny
command-only bot or the whole kitchen sink.

## What's in the box

| Capability | Feature flag | Notes |
|---|---|---|
| Slash/prefix command scaffolding | (always on) | via poise |
| Structured logging | (always on) | tracing → console + rotating JSON files |
| Music playback (lavalink) | `music` (default) | commands route through a backend trait |
| Music playback (native / in-process) | `music-native` | songbird driver + `yt-dlp` |
| User playlists (YAML-persisted) | `playlists` | `/playlist save|load|list|delete|feature` |
| Per-user voice recording | `record` | Opus-in-Ogg writers, zipped on `/record stop` |
| Text-to-speech | `tts` | via [GnomedDev/tts-service](https://github.com/GnomedDev/tts-service) |
| Speech-to-text | `stt` | any OpenAI-compatible provider (lemonfox, OpenAI, Groq, self-hosted [Speaches](https://github.com/speaches-ai/speaches)) |
| Cross-guild voice "radio" | `radio` | single-bot, multi-guild audio bridging; no bot-to-bot transport |

Both music backends can be compiled in simultaneously; `MUSIC_BACKEND=native|lavalink`
picks at startup. TTS works with either — it takes a URL-relay path through
an embedded axum server when on lavalink (since lavalink owns the voice UDP),
and a direct bytes path on native.

## Quickstart (docker compose)

The fastest path — brings up the bot, the TTS service, and wires them
together. Assumes you have a lavalink node somewhere reachable.

```sh
git clone https://github.com/cycle-five/bot-template-rs && cd bot-template-rs
cp .env.example .env
# edit .env — at minimum DISCORD_TOKEN, LAVALINK_HOST, LAVALINK_PASSWORD
docker compose up --build
```

For TTS on the lavalink backend you also need `BOT_PUBLIC_URL` set to a URL
your lavalink node can reach (e.g. a cloudflared tunnel hostname pointed at
the bot's `:8080`). See **TTS routing** in `docs/ARCHITECTURE.md`.

## Quickstart (no docker)

```sh
cp .env.example .env
# edit .env
cargo run --release
```

For non-default feature combos:

```sh
# lavalink-only, no other bells and whistles
cargo run --release --no-default-features --features music

# native backend + tts + recording, no lavalink
cargo run --release --no-default-features --features "music-native record tts"
```

## Environment

All config is via env vars; `.env` is loaded on startup if present.
`.env.example` documents every knob. The essentials:

| Var | Required | What |
|---|---|---|
| `DISCORD_TOKEN` | yes | bot token |
| `PREFIX` | no | prefix-command prefix (default `!`) |
| `DISCORD_DEV_GUILD` | no | if set, register commands to this guild only (instant propagation, dev convenience) |
| `MUSIC_BACKEND` | no | `lavalink` or `native` when both are compiled in |
| `LAVALINK_HOST` / `LAVALINK_PASSWORD` / `LAVALINK_SSL` | lavalink builds | node coordinates |
| `TTS_SERVICE_URL` + other `TTS_*` | tts builds | tts-service endpoint + defaults |
| `STT_BASE_URL` / `STT_API_KEY` / `STT_MODEL` / `STT_LANGUAGE` | stt builds | OpenAI-compatible STT endpoint |
| `BOT_PUBLIC_URL` / `BOT_HTTP_BIND_ADDR` / `BOT_AUDIO_TTL_SECS` | tts on lavalink | public hostname for the embedded audio server |
| `RUST_LOG` | no | tracing filter |

Runtime-mutable settings live in `config/*.yaml` (guild configs, lavalink
config, tts config) and are rewritten on shutdown / on `/…/set` admin
commands. Mount `config/` as a volume in production so changes survive
container restarts.

## Command surface

Subject to feature flags:

```
/ping                        # health ping
/status                      # build info + uptime
/join [channel]              # join voice
/leave
/play <query|url>            # lavalink: YouTube search / direct URL
                             # native:   yt-dlp resolves; URL or search
/play_file                   # message context menu ("Apps → Play attachments")
/stop /pause /resume /skip /queue
/playlist save|load|list|delete|featured|feature
/record start|stop|disable|enable
/tts speak|show|set|voices
/stt transcribe|show|set     # transcribe via any OpenAI-compatible provider
                             # also: "Apps → Transcribe attachment" on a message
/radio broadcast <name>      # start a broadcast from current VC
/radio silence               # stop broadcasting
/radio tune <name>           # tune this guild's VC to a live station
/radio unplug                # stop listening
/radio stations              # list live stations
/radio enable|disable        # admin: guild opt-in
/lavalink show|set|connect   # admin: runtime lavalink config
```

## Tests

```sh
cargo test --no-default-features --features "music music-native playlists record tts"
```

## Docs

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — module layout, backend
  abstraction, TTS routing, voice recording, persistence, logging.

## License

MIT.
