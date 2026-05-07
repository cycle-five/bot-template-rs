# Testing

Audit of what's tested, what could be tested, and what genuinely can't be
without infrastructure beyond the test harness. Snapshot at 0.1.1.

## Test counts (post-0.1.1)

| Build | Tests | Notes |
|---|---|---|
| `cargo test` (default features) | 30 | Lavalink + music-core + voice + record + tts + stt + radio + playlists |
| `cargo test --no-default-features --features music-native` | 23 | Native backend exclusive paths |
| `cargo test --all-features` | 71 | Everything; kitchen sink |

Test density per file:

```
12  src/lavalink.rs
10  src/playlist.rs
 7  src/stt.rs
 6  src/audio_http.rs
 6  src/record.rs
 5  src/music.rs
 5  src/native_backend.rs
 4  src/data.rs
 4  src/tts.rs
 3  src/radio.rs
 2  src/commands.rs
 2  src/logging.rs
 2  src/music_backend.rs
 2  src/status.rs
 1  src/handlers.rs
 0  src/main.rs
 0  src/reply.rs
```

## What we test today

**Pure logic (well-covered):**
- Parsers: `parse_timestamp`, `clean_track_title`, `strip_timestamps`,
  `should_fallback_to_search`, `youtube_video_id`
- Conversions: `Track` ↔ YAML, `apply_perm`, `track_line`, `format_duration`
- Config: `GuildConfig` defaults + serialization, `LavalinkConfig` defaults,
  `TtsConfig` / `SttConfig` round-trips
- File I/O: `playlist::path_for` collision-resistance, `audio_http` TTL
  expiry, OggOpus writer produces a valid stream, recording session
  filename safety
- Constructors: `NativeBackend::new` + lazy guild meta creation,
  `LavalinkBackend::new`, `Reply` (untested! — see gaps)

**Command-shape assertions:**
Every `mod tests` includes a `*_commands_defined` test that verifies the
poise command names + flags exist as expected. These are compile-time-ish
guarantees against accidental rename / removal — cheap and worth keeping.

**Trait conformance:**
- `MusicBackend: Send + Sync + 'static` (dyn-compat assertion)
- `Handler: EventHandler` (compile-time check)

**Async state:**
- `tokio::test`s for `Data::new`, `NativeBackend` guild meta, `audio_http`
  TTL behavior under concurrent get/insert.

## Gaps — pure-function tests we should add

These are pure functions with no I/O, no dependencies on Discord state, and
significant logic. Adding tests for these is high ROI per LOC.

| Function | File | Why it matters |
|---|---|---|
| `filter_state_to_lavalink` | lavalink.rs | Speed/pitch/bass-boost translation; off-by-one EQ band ranges, neutral-state collapse |
| `title_from_url` | native_backend.rs | Filename inference for URL-only tracks; edge cases around query strings, fragments, no extension |
| `is_audio_attachment` | music.rs | Extension dispatch; case sensitivity, missing extension |
| `Reply::*` builder | reply.rs | Builder fluency, default state |
| `FilterState::neutral` / `is_neutral` | music_backend.rs | Trivial but locks the contract |
| `LoopChoice → LoopMode` | music.rs | Trivial mapping; locks the choice surface |

This audit added unit tests for the first three (the larger ones) — see
`src/lavalink.rs::tests`, `src/native_backend.rs::tests`, `src/music.rs::tests`.

## Gaps — trickier but still feasible

**HTTP-mock-based tests** (would add `wiremock` or `mockito` dep):
- `LavalinkBackend::resolve_youtube_title` — oEmbed JSON parsing under 200,
  404, malformed JSON, missing fields
- `lyrics()` — lrclib responses (success, instrumental, 404, malformed)
- `HttpSttBackend::transcribe` — stt provider responses
- TTS service responses
- `audio_http::serve` end-to-end against a known store (the existing
  `audio_http` tests stub the store and don't go through axum's router)

These would catch parsing regressions cheaply. Recommend adding `wiremock`
to dev-deps in 0.2.x and using it for the HTTP-fetch paths above.

**Backend state tests** (no mocks needed):
- `LavalinkSharedState` — loop_modes / history / filters operations.
  Currently only exercised end-to-end. A direct unit test would verify
  the per-guild eviction / 50-entry cap behavior.
- `NativeBackend::shuffle / jump / move_track / remove_at` against a
  guild meta with a stub queue (no songbird involvement). Currently we
  test `apply_perm` in isolation and assume the wrappers compose
  correctly. Direct tests would catch off-by-one errors in the index
  translations between metadata indices and songbird's `q[1+i]` space.

## Gaps — what genuinely can't be unit-tested

These need either a live Discord gateway, a live lavalink node, or a real
voice connection. Not impossible to test, but they fall into the
**integration testing** bucket.

| Surface | Why it can't be unit-tested | Suggested approach |
|---|---|---|
| Slash command invocation | poise's `Context` constructs only inside an actual interaction-receiving framework; you can't synthesize one in a unit test | Manual / human-in-the-loop QA; or [serenity-test-bench](https://github.com/serenity-rs/serenity/tree/next/examples) style fake gateway (heavy) |
| Voice channel join/leave | Songbird gateway dance requires a real Discord WebSocket session | Manual QA; could test `Songbird::join` indirectly with a stub VoiceManager but the value is low |
| Audio playback (lavalink + native) | End-to-end audio requires real Discord voice servers + opus encode → decode | Manual QA in a test guild |
| Recording end-to-end | Same — needs real RTP packets from real users speaking | Manual QA; the unit tests cover the Ogg/Opus muxer and the cancel flag, which are the parts where bugs would live |
| Lavalink REST/WS | Could be done with a testcontainers lavalink, but the dep is heavy and lavalink is an external service | Spin up [`testcontainers-rs`](https://docs.rs/testcontainers) lavalink in CI for an integration suite gated behind `--features integration-tests`. Out of scope for 0.1.x |
| Component button interactions | Requires actual `InteractionCreate` events from Discord | Manual QA. When 0.2.2 lands buttons, fake the interaction shape with `serde_json::from_value` to test the dispatch / custom_id parser in isolation |
| Cross-guild radio bridging | Requires two voice connections to two guilds simultaneously | Manual QA; the `tokio::broadcast` plumbing is unit-tested already (`station_broadcast_channel_fans_out`) |
| `on_error` UnknownInteraction reply | The reply requires a real `CommandInteraction` token | Manual QA; the parsing / branching logic in `log_command_error` could be extracted into a pure function and tested |

## Recommended near-term improvements

In rough priority order:

1. **Add the pure-function tests listed in "Gaps — pure-function tests we
   should add"** (this audit added the top three).
2. **Extract testable cores from poise commands**. Pattern: each command's
   `pub async fn name(ctx: Context, args)` becomes a thin wrapper over a
   pure helper that takes `&Data`, `GuildId`, args, and returns the embed +
   side-effect description. The helper is unit-testable; the wrapper is
   the part that's "untestable Discord glue."
   - First candidates: `/seek` arg validation + clamp, `/volume` clamp, `/jump`
     index validation, `/move` validation. All of those have logic worth
     locking in.
3. **Add `wiremock` for HTTP-fetch paths** (lyrics, oEmbed, lavalink REST,
   tts-service, stt provider). Catches parsing regressions on free-tier
   API drift.
4. **Direct `LavalinkSharedState` and `GuildMeta` tests** — verify
   eviction / cap / per-guild isolation without going through the backend.
5. **Integration suite (`--features integration-tests`)** with
   `testcontainers-rs` spinning a lavalink + an stt server. Heavy but
   would cover the lavalink REST/WS path properly. Defer to 0.2.x.

## The "test-vs-build" tradeoff for cargo features

Every test is implicitly gated by feature flags. Tests in `record.rs`
only compile under `--features record`. Tests in
`native_backend.rs` only under `--features music-native`. To exercise the
full surface you must run all three: default, music-native, all-features.
CI should run all three matrix points. Consider adding a small Makefile or
`xtask` to make this convention explicit:

```sh
cargo test
cargo test --no-default-features --features music-native
cargo test --all-features
```

Currently a developer could land code that passes the default test pass but
breaks music-native. Encoding the matrix in CI catches this.
