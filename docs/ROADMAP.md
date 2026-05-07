# Roadmap

Versioned plan for what's coming and what's been deferred. Each item carries
its own rationale so the *why* survives long after the conversation that
spawned it.

Current version: **0.1.1** (cut 2026-05-07).

## 0.1.x — small follow-ups

These are the items that didn't make 0.1.1 but should be small enough to
land without a major version bump.

### Deferred PR review items (from PR #2)

These were Copilot/gemini-code-assist comments deliberately not addressed
in PR #2 — each kept for a real reason. See [`memory/pr2_deferred_items.md`]
in the project memory for full context per item.

| Item | Path | Effort | Notes |
|---|---|---|---|
| `audio_http` enumerable IDs | `src/audio_http.rs` | one-line UUIDv4 | Threat-model dependent; only matters if `BOT_PUBLIC_URL` exposed broadly |
| `record` zip step OOM risk | `src/record.rs` | structural rewrite to `io::copy` streaming | Long voice sessions OOM under current `std::fs::read`-then-write |
| `stt` extra `to_vec` copy | `src/stt.rs` | signature change to `Vec<u8>` ownership | Genuine micro-perf, savings bounded by Discord attachment limits |

### Native backend gaps

Carried over from 0.1.1's "lavalink-only" cuts:

- **`/loop queue` on native** — would require async yt-dlp re-resolution from
  inside songbird's `TrackEvent::End` handler. Either spawn a tokio task
  from `AdvanceOnEnd` to re-call `play_url`, or persist the source query on
  `Track` and have a separate background task drain a per-guild
  re-enqueue channel. Lavalink path is in
  `src/lavalink.rs::track_end_handler`; mirror its semantics.
- **Audio filters (`/bassboost` / `/nightcore` / `/speed` / `/pitch`) on
  native** — songbird has no DSP layer. Adding one means splicing
  symphonia's effects (or a custom EQ/timescale) into the input pipeline.
  Substantial work; lavalink remains the path for filter-heavy use cases.
- **Native `/stop` semantic divergence** — currently drains the queue (matches
  songbird's `TrackQueue::stop`); lavalink retains. Aligning would need a
  parallel-queue-with-replay design or a deeper `Driver` integration.
  Documented in `src/native_backend.rs::stop`.

### Other small wins

- `/stop` consistency across backends (see above)
- Fix native backend `apply_perm` shuffle to handle same-track-twice queues
  gracefully (current dedupe-by-URI in `/removedupes` handles it; shuffle is
  fine as-is but the comment in `shuffle` claiming "lock-step" between meta
  and songbird queues should be re-checked under racy ends)

## 0.2.x — the next material feature drop

Three big-ticket items, in priority order.

### 0.2.1 — Server administration

Three new commands plus `GuildConfig` fields. Pre-req for #7 (interactive
buttons need permission decisions).

**`/djmode <on|off>`** — toggle a guild flag that restricts music control
commands (anything that mutates playback state: play / skip / stop / clear
/ shuffle / move / remove / loop / filters) to a designated DJ role.

- Add `GuildConfig.dj_mode_enabled: bool` and `GuildConfig.dj_role_id:
  Option<RoleId>`.
- `/djrole <role>` — set the role.
- Permission check: poise has `check` callbacks per command. Define a
  `dj_required` check that reads `GuildConfig`; attach to mutating music
  commands. Owner/admin always pass.
- Read-only commands (`/queue`, `/now`, `/lyrics`) bypass the check.

**`/setvc <channel>`** — bind the bot's music interactions to a specific
voice channel. Music commands invoked by users not in that channel get a
"go to #binding" reply. Useful for servers with a dedicated music room.

- Add `GuildConfig.bound_voice_channel: Option<ChannelId>`.
- Implement as another poise `check` callback.
- `/setvc unbind` (or accept `None`) to clear.

**`/announcechannel <channel>`** — set the channel where now-playing
announcements post automatically (currently each command's reply is its
own announcement). When configured, track-start events (need wiring on
both backends — already partially there for lavalink via
`track_end_handler`; add `track_start`) post a now-playing embed there.

- Add `GuildConfig.announce_channel: Option<ChannelId>`.
- Add `track_start_handler` on lavalink and a parallel hook on native.
- Skip the broadcast when the command itself was invoked in the
  announce channel (avoid double-posting).

### 0.2.2 — Interactive now-playing buttons

UX upgrade. Each now-playing embed gets a row of component buttons
(Pause/Resume, Skip, Stop, Shuffle, Queue) so users don't have to type
slash commands for the common path.

**Implementation outline:**

1. Add a `with_buttons(bool)` builder on `Reply` (or a `Slot::NowPlayingInteractive`).
   Default `now_playing()` calls in `music.rs` opt in.
2. Component rows via `serenity::CreateActionRow::buttons` with `CreateButton`.
   Custom IDs: `music:pause:<guild_id>`, `music:resume:<guild_id>`,
   `music:skip:<guild_id>`, `music:stop:<guild_id>`, `music:shuffle:<guild_id>`,
   `music:queue:<guild_id>`. Embed the guild_id so the handler doesn't have to
   re-derive context.
3. New `handlers::on_component_interaction` arm in `Handler::dispatch`.
   Parse custom_id, dispatch to `MusicBackend` method, ack with
   `InteractionResponseType::DeferredUpdateMessage`, then `edit_response` with
   updated embed.
4. **Permission decision**: integrate with `/djmode`. If DJ mode is on and the
   clicker isn't in the DJ role, ack ephemerally with "DJ-only".
5. **Stale-message handling**: button clicks on an old now-playing message can
   still fire. Decision: act on them anyway (the action makes sense in any
   state, e.g. /skip skips whatever's currently playing). Document so it's
   not surprising.

**Open question:** auto-update the now-playing embed on track change.
Without this, the embed becomes stale once the track advances. Solution:
on track-start (per #11's announce-channel hook), edit the previous
now-playing message in place. Adds a per-guild "last-now-playing-message-id"
to either `GuildConfig` or transient state.

### 0.2.3 — `/autoplay`

Toggle per-guild "when the queue empties, automatically queue a related
track". Recommendation source is the trickiest part.

**Approach: lavalink + LavaSrc**

LavaSrc's `LavaSearch` source has a `recommendations` endpoint that returns
related tracks for a given track ID. If LavaSrc is configured, this is the
clean path:

```
ytmsearch:related:<videoId>
```

(or LavaSrc's own `lsrec:` prefix depending on plugin version).

**Approach: native + yt-dlp**

yt-dlp can fetch YouTube's "watch?v=X&list=RDX" autoplay mix:

```
yt-dlp -j "https://www.youtube.com/watch?v=<id>&list=RD<id>" --flat-playlist
```

This returns a list of related video IDs. Take the first one.

**Implementation outline:**

1. `GuildConfig.autoplay_enabled: bool`, `/autoplay` toggles.
2. New `MusicBackend` method `next_recommendation(guild) -> Result<Option<String>>`
   returning the URL/query for the next related track. Default impl returns
   `Ok(None)` (no recommendations available).
3. Lavalink impl: call LavaSearch's recommendations endpoint via
   `client.load_tracks("ytmsearch:rec:<lastTrackId>")` or similar — confirm
   exact prefix with current LavaSrc version.
4. Native impl: shell out to yt-dlp `--flat-playlist` against the autoplay
   mix URL; parse the JSONL output for the first non-current video.
5. Wire into `track_end_handler` (lavalink) and `AdvanceOnEnd` (native): if
   queue empty after advance and autoplay is on, fetch and enqueue.

**Pre-req:** the bot needs to know "what just played" — already captured by
the `LavalinkSharedState.history` machinery from the /loop+/previous commit.

### 0.2.4 — Spotify support

See [`memory/pr2_deferred_items.md`] and the in-conversation analysis around
the four paths (extended quota, per-user OAuth, librespot, embed scraping).

Recommended sequencing:
1. **0.2.4-a:** Bot-side embed scraping (covers public Spotify URLs / curated
   playlists / individual tracks). Doesn't fix daylists but adds Spotify
   support that doesn't exist today. ~150 LOC.
2. **0.2.4-b:** Push for Spotify Extended Quota approval (server-side change,
   no bot code) — best long-term path for daylist coverage if Spotify
   approves.
3. **0.2.4-c (deferred to 0.3):** Per-user OAuth for personalized playlists if
   extended quota doesn't pan out.

## 0.3.x — bigger lifts

- **Per-user Spotify OAuth** (if 0.2.4-b doesn't land daylist coverage)
- **Web player / dashboard** — the commercial bots all have one. Useful for
  admins who don't want to live in slash commands. Likely a separate axum
  service that talks to the bot via a shared state channel.
- **Multi-bot per server** (Jockie's signature) — deep change; multiple
  serenity clients in one process, each with their own gateway connection.
  Probably out of scope unless there's strong demand.
- **librespot-rs integration** — alternative Spotify path. Bot account is a
  Spotify Premium user; tracks stream through librespot directly, no
  YouTube re-search step. ~200-400 LOC plus the dep.

## Notes on the comparison matrix

The session that produced 0.1.1's command set built a feature-comparison
table against Jockie / Luna / FlaviBot. Headlines:

- **Where we trail (post-0.1.1):** Spotify family (any), per-track filters
  beyond the basics (karaoke / vibrato / distortion are exposed by
  lavalink; we just don't surface them), web player, multi-bot, lyrics
  display polish (no synced timing yet).
- **Where we lead:** voice recording (`/record`), STT/TTS (any
  OpenAI-compatible backend), cross-guild radio (`/radio`), self-hostable
  Rust + cargo-feature-gated, yt-dlp long-tail (~1000 sources via the
  native backend).

The lead items aren't in any of the SaaS bots — keeping them
sharp differentiates the project even as we close the music-feature gap.
