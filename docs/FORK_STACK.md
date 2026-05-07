# Fork stack

Why this bot pins git forks of serenity, songbird, lavalink-rs, and stream_lib
instead of crates.io releases — and how to keep it building when any link in
the chain moves.

## Why fork at all

The serenity 0.12.5 release (and everything that depends on it) is on a major
API surface that's actively being replaced on the maintainers' `next` branches.
Sticking with the released stack means:

- **Two of every websocket and TLS crate in the tree.** serenity 0.12.5 pins
  `tokio-tungstenite 0.21` (which pulls `tungstenite 0.21`, `tokio-rustls
  0.25`, `rustls 0.22`, `rand 0.8`, `thiserror 1`, …); lavalink-rs 0.15 pins
  `tokio-tungstenite 0.28` (newer everything). Cargo can't unify across
  incompatible majors.
- **Old `dashmap 5` kept alive.** serenity 0.12.5 hard-pins `^5.5.3`; the
  rest of the world has moved to `6.x`.
- **OpenSSL pulled in by default.** songbird's `native` feature enables
  `serenity/native_tls_backend`, which means `hyper-tls`, `tokio-native-tls`,
  and `openssl` all show up in the tree even if you set `serenity` to
  `default-features = false, features = ["rustls_backend"]` at the top level.
- **No path to the new event-handling and typed-Data API.** poise's stable
  0.6 only knows the old `EventHandler::ready/cache_ready/...` per-event
  trait and the `setup` callback. The new poise (`serenity-next` branch)
  rewires both.

The maintainers are landing all of this on coordinated `next` / `serenity-next`
branches across serenity, songbird, and poise. We point at those, plus a
matching lavalink-rs fork, and the whole tree collapses to a single copy of
each of the duplicated crates above. **No `openssl`, no `native-tls`, no
`hyper-tls`** — pure rustls.

## The chain

| Crate | Source | Branch | Edits beyond upstream `next`/`serenity-next` |
|---|---|---|---|
| serenity | `github.com/CycleFive/serenity` | `next` | none — we just want a stable pin under our control |
| poise | `github.com/serenity-rs/poise` | `serenity-next` | unforked; consumed upstream |
| songbird | `github.com/CycleFive/songbird` | `bot-template-experiment` | bump optional `reqwest` 0.12 → 0.13 + rename `reqwest?/rustls-tls` → `reqwest?/rustls` |
| lavalink-rs | `gitlab.com/cycle.five/lavalink-rs` | `bot-template-experiment` | repoint `serenity-dep` and `songbird-dep` at the `next` git branches; remove the dead `last_should_continue` plumbing |
| stream_lib | `github.com/CycleFive/rsget` (workspace member) | `bot-template-experiment` | bump `reqwest` 0.12 → 0.13; collapse the `rustls-tls` feature onto reqwest 0.13's renamed `rustls` |

All four fork branches are named `bot-template-experiment` (except serenity,
where there's nothing to fork — we point at upstream `next` via the CycleFive
mirror). They're independent — none of them assumes the others exist; they
only assume the rest of the stack lives on the same generation of the
serenity API.

### The reqwest 0.13 chain reaction

reqwest 0.13 renamed the `rustls-tls` feature to `rustls` and made rustls
the default TLS backend (replacing native-tls). serenity-next pins
`reqwest >= 0.13`. That cascades:

```
serenity-next  →  reqwest 0.13
   ↓
bot's direct reqwest dep                  →  must be 0.13
   ↓
bot uses songbird/driver (music-native)   →  songbird's reqwest dep must be 0.13
   ↓
songbird/driver pulls stream_lib          →  stream_lib's reqwest dep must be 0.13
```

If any link stays on reqwest 0.12, cargo can't unify (different majors), and
you get type errors at the API boundary — `reqwest::Client` from one major
isn't the same type as from the other. The songbird fork bumps reqwest;
stream_lib gets the same bump (CycleFive/rsget). The default (lavalink-only)
build doesn't enable `songbird/driver` and so doesn't pull stream_lib at all
— stream_lib only matters for `music-native` consumers.

## How `Cargo.toml` ties it together

Three `[patch]` entries collapse every reference to the stack onto our forks,
even when the reference comes from a transitive dep:

```toml
# poise's serenity-next branch and lavalink-rs both depend on
# serenity-rs/serenity:next. Redirect to our fork so cargo unifies.
[patch."https://github.com/serenity-rs/serenity"]
serenity = { git = "https://github.com/CycleFive/serenity", branch = "next" }

# lavalink-rs depends on serenity-rs/songbird:serenity-next. Same trick.
[patch."https://github.com/serenity-rs/songbird"]
songbird = { git = "https://github.com/CycleFive/songbird", branch = "bot-template-experiment" }

# stream_lib comes in via songbird/driver as a crates.io dep.
[patch.crates-io]
stream_lib = { git = "https://github.com/CycleFive/rsget", branch = "bot-template-experiment" }
```

Without the redirects you'd end up with two serenitys / two songbirds /
both reqwests in the tree — exactly the duplicates we forked to eliminate.

## Bot-side adaptations

The new serenity API removes a couple of pieces of glue the old code relied
on; both have small but visible effects:

- **`songbird::get(ctx)` is gone.** songbird-next removed its serenity
  integration shim (the `SongbirdKey` `TypeMap` entry, `register_songbird`
  ClientBuilder ext, and `get(ctx)` accessor) because the new serenity has
  no `TypeMap`. We now build `Arc<Songbird>` once in `main.rs`, register it
  via `client_builder.voice_manager(arc.clone())`, and stash a clone on
  `Data.songbird`. Backends take it at construction
  (`LavalinkBackend::new(config, songbird)`), and command handlers reach it
  via `ctx.data().songbird`.
- **No more auto-register on `Ready`.** poise serenity-next dropped the
  `Framework::builder().setup(...)` callback that the old framework used to
  push slash commands on startup. The new pattern is a manual,
  owners-only `!register` poise command (defined in `commands.rs`) — run it
  once after deploy. `DISCORD_DEV_GUILD` controls whether it registers
  globally or to a single guild.
- **`EventHandler` is one method now.** `dispatch(&self, ctx: &Context,
  event: &FullEvent)` replaces the per-event hooks (`ready`, `cache_ready`,
  `guild_create`, …). See `src/handlers.rs`.

## Rust toolchain

`rust-toolchain.toml` pins to **1.95.0**. fork serenity's `Cargo.toml` sets
`rust-version = "1.95"` (it uses `let-chains` and a few other features that
landed in that release). Toolchain pinning means a fresh clone needs no
`rustup` ceremony — cargo will fetch the pinned toolchain on first invocation.

## Updating

When one of the upstream `next` branches advances and you want to pull it:

1. **For a fork with edits** (songbird, lavalink-rs, stream_lib): in the
   fork's checkout, `git pull --rebase` from the upstream branch you forked
   from, resolve any conflicts, push the rebased `bot-template-experiment`
   branch.
2. **For a fork that's a marker** (serenity): the bot's Cargo.toml points
   at `CycleFive/serenity:next`; `cargo update -p serenity` repulls.
3. Run both build configurations end-to-end:
   ```sh
   cargo build
   cargo build --no-default-features --features music-native
   cargo test
   ```
4. If serenity's `next` reshuffles its API surface again (it has been an
   active branch), expect to revisit `handlers.rs`, `main.rs` framework
   setup, and any `Cow<'_, str>` / `GenericChannelId` / `&Http`-vs-`&Context`
   call sites.

## Re-mainstreaming

When the maintainers cut a stable serenity 0.13 / poise 0.7 / songbird 0.7
release and lavalink-rs follows with a compatible pin, this whole apparatus
collapses back to plain crates.io deps. The migration looks like:

1. Drop the `[patch]` entries.
2. Replace each `git = "..."` dep with `version = "..."`.
3. Drop `rust-toolchain.toml` if the released stack no longer needs the
   pinned toolchain.
4. Re-test both build configurations.

Until then, the fork stack is the way through. The bot-side migrations
(handlers, songbird Arc, manual register command) are forward-compatible —
they're how the new API wants to be used and won't need un-doing.
