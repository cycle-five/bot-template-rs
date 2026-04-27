//! Cross-guild voice bridging ("radio").
//!
//! # Model
//!
//! A single bot instance in multiple guilds acts as its own relay. The
//! source guild's [`Call`](songbird::Call) has a [`BroadcastHandler`]
//! attached that mixes per-SSRC decoded PCM on each `VoiceTick` and
//! pushes the mixed frame into a [`tokio::sync::broadcast`] channel.
//! Destination guilds subscribe to that channel, encode the PCM to Opus,
//! and feed songbird's mixer a DCA1 stream — which triggers Opus
//! passthrough mode so the packets go straight to Discord's RTP with no
//! decode/re-encode roundtrip.
//!
//! # Privacy posture
//!
//! Broadcasting is **opt-in per guild**: admins enable with `/radio enable`.
//! Starting a broadcast posts a public (non-ephemeral) announcement so
//! users in the source channel can see they are being relayed. There is no
//! per-user opt-out — users who don't want to be relayed should leave the
//! channel until broadcasting stops. Tuning in is *not* gated because the
//! bot in the listener guild is the one emitting audio; from Discord's
//! perspective it's the same as `/play`.
//!
//! # Network story
//!
//! No bot-to-bot transport. Audio flows over Discord's voice servers on
//! both legs (users → bot in source guild, bot → users in destination
//! guild), so the bot never exposes a public voice endpoint. Bridging
//! between two *different* bot instances (different operators) is not
//! supported — that's a WebSocket-transport design, deliberately left out
//! of the template.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use std::io;
use std::pin::Pin;
use std::task::{Context as StdContext, Poll};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::GuildId;
use songbird::driver::opus::{
    self as opus_codec, Application as OpusApplication, Channels as OpusChannels,
};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{AsyncAdapterStream, AsyncMediaSource, AudioStream, Input, LiveInput};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWriteExt, DuplexStream, ReadBuf, duplex};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::reply::{self, Reply};
use crate::{Context, Error};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// 20 ms @ 48 kHz stereo, interleaved — matches songbird's VoiceTick cadence.
const SAMPLES_PER_FRAME: usize = 960 * 2;
/// Broadcast channel slot count. At ~50 Hz, 64 slots = ~1.3 s of buffering
/// before a slow subscriber starts dropping with `Lagged`.
const CHANNEL_CAPACITY: usize = 64;
/// DuplexStream + AsyncAdapter ringbuffer size. Opus frames are small
/// (~80 B at voip quality) so 64 KiB is plenty of jitter absorption.
const DUPLEX_BUFFER: usize = 64 * 1024;
/// Max Opus packet size the encoder can emit. Per RFC 7845, 4000 bytes
/// covers the worst case (120 ms stereo high-bitrate).
const OPUS_MAX_PACKET: usize = 4000;
/// Minimal DCA1 metadata blob. DcaReader parses this as JSON and uses it
/// to configure the track. Stereo @ 48 kHz, 20 ms frames, voip mode — must
/// match what we actually encode below.
const DCA1_METADATA_JSON: &str = concat!(
    r#"{"dca":{"version":1,"tool":{"name":"bot-template-rs/radio","version":"0.1.0"}},"#,
    r#""opus":{"mode":"voip","sample_rate":48000,"frame_size":960,"vbr":true,"channels":2}}"#,
);

// ---------------------------------------------------------------------------
// Station + subscription state
// ---------------------------------------------------------------------------

pub struct RadioStation {
    pub name: String,
    pub source_guild: GuildId,
    pub started_at: DateTime<Utc>,
    active: Arc<AtomicBool>,
    tx: broadcast::Sender<Arc<[i16]>>,
}

impl RadioStation {
    #[must_use]
    fn new(name: String, source_guild: GuildId) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            name,
            source_guild,
            started_at: Utc::now(),
            active: Arc::new(AtomicBool::new(true)),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<[i16]>> {
        self.tx.subscribe()
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

/// Active tuning in a destination guild. Drop it (via unplug / guild change)
/// to cancel the forwarder task and let the destination track end.
pub struct RadioSubscription {
    pub station_name: String,
    pub started_at: DateTime<Utc>,
    _forwarder: JoinHandle<()>,
}

impl Drop for RadioSubscription {
    fn drop(&mut self) {
        self._forwarder.abort();
    }
}

// ---------------------------------------------------------------------------
// Broadcaster: VoiceTick handler
// ---------------------------------------------------------------------------

struct BroadcastHandler {
    tx: broadcast::Sender<Arc<[i16]>>,
    active: Arc<AtomicBool>,
    tick_ct: AtomicU64,
    voiced_ct: AtomicU64,
}

#[async_trait]
impl EventHandler for BroadcastHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if !self.active.load(Ordering::Relaxed) {
            return None;
        }
        if let EventContext::VoiceTick(tick) = ctx {
            // Always emit a frame, even a silent one — the listener's
            // RawAdapter pipeline expects a steady 50 Hz feed, and gaps
            // would show up as audio glitches.
            let mut mixed = vec![0i16; SAMPLES_PER_FRAME];
            let mut voiced = false;
            for data in tick.speaking.values() {
                let Some(samples) = &data.decoded_voice else {
                    continue;
                };
                voiced = true;
                for (i, &s) in samples.iter().enumerate().take(SAMPLES_PER_FRAME) {
                    let sum = mixed[i] as i32 + s as i32;
                    mixed[i] = sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                }
            }
            // Ignore SendError — just means no subscribers right now.
            let _ = self.tx.send(Arc::from(mixed));

            let ticks = self.tick_ct.fetch_add(1, Ordering::Relaxed) + 1;
            if voiced {
                self.voiced_ct.fetch_add(1, Ordering::Relaxed);
            }
            if ticks == 1 || ticks % 250 == 0 {
                debug!(
                    target: "bot_template_rs::radio",
                    ticks,
                    voiced = self.voiced_ct.load(Ordering::Relaxed),
                    subscribers = self.tx.receiver_count(),
                    "broadcast tick"
                );
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Receiver: broadcast channel -> opus encoder -> DCA1 framing -> duplex pipe
//          -> AsyncAdapterStream -> songbird (Opus passthrough)
// ---------------------------------------------------------------------------

/// A non-seekable, length-unknown async wrapper around a `DuplexStream`,
/// tagged with the traits songbird's `AsyncAdapterStream` needs. The bytes
/// flowing through are a live DCA1 stream.
struct DuplexMediaSource {
    inner: DuplexStream,
}

impl AsyncRead for DuplexMediaSource {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncSeek for DuplexMediaSource {
    fn start_seek(self: Pin<&mut Self>, _pos: io::SeekFrom) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "radio stream is not seekable"))
    }
    fn poll_complete(
        self: Pin<&mut Self>,
        _cx: &mut StdContext<'_>,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

#[async_trait]
impl AsyncMediaSource for DuplexMediaSource {
    fn is_seekable(&self) -> bool {
        false
    }
    async fn byte_len(&self) -> Option<u64> {
        None
    }
}

struct TrackEndLogger;

#[async_trait]
impl EventHandler for TrackEndLogger {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(list) = ctx {
            for (state, _handle) in *list {
                warn!(
                    target: "bot_template_rs::radio",
                    play_mode = ?state.playing,
                    position_ms = state.position.as_millis(),
                    "listener track ended"
                );
            }
        }
        None
    }
}

/// Build the one-time DCA1 header: magic + u32-le metadata length + JSON blob.
fn dca1_header() -> Vec<u8> {
    let meta = DCA1_METADATA_JSON.as_bytes();
    let mut out = Vec::with_capacity(8 + meta.len());
    out.extend_from_slice(b"DCA1");
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(meta);
    out
}

async fn attach_listener(
    dest_call: Arc<tokio::sync::Mutex<songbird::Call>>,
    mut rx: broadcast::Receiver<Arc<[i16]>>,
) -> Result<JoinHandle<()>, Error> {
    let (writer, reader) = duplex(DUPLEX_BUFFER);

    // Forwarder: pull i16 stereo frames off the broadcast channel, encode
    // each as a single 20 ms Opus packet, and write it framed for DcaReader
    // (`u16-le len || opus-bytes`). Prepend the DCA1 header exactly once so
    // the first bytes the sync reader sees form a valid format signature.
    let forwarder = tokio::spawn(async move {
        let mut writer = writer;

        if let Err(e) = writer.write_all(&dca1_header()).await {
            warn!(
                target: "bot_template_rs::radio",
                error = %e,
                "forwarder exiting: DCA1 header write failed"
            );
            return;
        }

        let mut encoder = match opus_codec::Encoder::new(
            48_000,
            OpusChannels::Stereo,
            OpusApplication::Voip,
        ) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    target: "bot_template_rs::radio",
                    error = %e,
                    "forwarder exiting: opus encoder init failed"
                );
                return;
            }
        };
        let mut packet = vec![0u8; OPUS_MAX_PACKET];
        let mut frame_ct: u64 = 0;
        let mut slow_write_ct: u64 = 0;

        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let n = match encoder.encode(&frame, &mut packet) {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(
                                target: "bot_template_rs::radio",
                                error = %e,
                                "opus encode failed; skipping frame"
                            );
                            continue;
                        }
                    };

                    let len_bytes = (n as u16).to_le_bytes();
                    let write_start = std::time::Instant::now();
                    let r = async {
                        writer.write_all(&len_bytes).await?;
                        writer.write_all(&packet[..n]).await
                    }
                    .await;
                    let elapsed = write_start.elapsed();

                    if let Err(e) = r {
                        warn!(
                            target: "bot_template_rs::radio",
                            frames = frame_ct,
                            error = %e,
                            "forwarder exiting: duplex write_all failed"
                        );
                        return;
                    }
                    if elapsed.as_millis() > 30 {
                        slow_write_ct += 1;
                        warn!(
                            target: "bot_template_rs::radio",
                            frame = frame_ct,
                            elapsed_ms = elapsed.as_millis(),
                            slow_writes = slow_write_ct,
                            "forwarder duplex write took > 30 ms (mixer back-pressure)"
                        );
                    }
                    frame_ct += 1;
                    if frame_ct == 1 || frame_ct % 250 == 0 {
                        debug!(
                            target: "bot_template_rs::radio",
                            frames = frame_ct,
                            opus_bytes = n,
                            slow_writes = slow_write_ct,
                            "forwarder frames written"
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!(
                        target: "bot_template_rs::radio",
                        frames = frame_ct,
                        "forwarder exiting: broadcast channel closed"
                    );
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    info!(
                        target: "bot_template_rs::radio",
                        dropped = n,
                        "listener lagged; dropped frames"
                    );
                }
            }
        }
    });

    // Async duplex reader -> sync MediaSource. symphonia's probe sees the
    // "DCA1" magic on the first read and picks DcaReader; songbird's mixer
    // then activates Opus passthrough, forwarding our packets straight to
    // Discord's RTP with no decode or re-encode.
    let src = DuplexMediaSource { inner: reader };
    let sync_stream = AsyncAdapterStream::new(Box::new(src), DUPLEX_BUFFER);
    let live = LiveInput::Raw(AudioStream {
        input: Box::new(sync_stream),
    });
    let input = Input::Live(live, None);

    let mut handler = dest_call.lock().await;
    let track_handle = handler.enqueue_input(input).await;
    let _ = track_handle.add_event(Event::Track(TrackEvent::End), TrackEndLogger);
    let _ = track_handle.add_event(Event::Track(TrackEvent::Error), TrackEndLogger);

    Ok(forwarder)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Cross-guild voice bridging. Broadcasts require the guild to be opted in.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("broadcast", "silence", "tune", "unplug", "stations", "enable", "disable"),
    subcommand_required
)]
pub async fn radio(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Start broadcasting this voice channel under `name`. Guild must be enabled.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "broadcast",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn broadcast(
    ctx: Context<'_>,
    #[description = "Station name other guilds use to tune in"] name: String,
    #[description = "Voice channel to broadcast from (default: bot's current or yours)"]
    #[channel_types("Voice")]
    channel: Option<serenity::ChannelId>,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    if !guild_broadcast_enabled(&ctx, guild_id).await {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(
                    "This guild isn't enabled for radio broadcasting. An admin \
                     can opt in with `/radio enable`.",
                )
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    if ctx.data().radio_stations.contains_key(&name) {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(format!("A station named `{name}` is already live."))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("songbird not registered")?
        .clone();

    // Resolve target: explicit arg > existing bot call > invoker's VC.
    let call = if let Some(target) = channel {
        manager.join(guild_id, target).await?
    } else if let Some(existing) = manager.get(guild_id) {
        existing
    } else if let Some(invoker_vc) = voice_channel_of(&ctx, guild_id) {
        manager.join(guild_id, invoker_vc).await?
    } else {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(
                    "I'm not in a voice channel and neither are you. Join one, \
                     pass `channel`, or have me `/join` first.",
                )
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    // Register the station in the shared state *before* attaching the
    // event handler so the handler's clone of the sender can't outlive
    // the map entry on the happy path.
    let station = Arc::new(RadioStation::new(name.clone(), guild_id));
    ctx.data()
        .radio_stations
        .insert(name.clone(), station.clone());

    {
        let mut handler = call.lock().await;
        handler.add_global_event(
            Event::Core(CoreEvent::VoiceTick),
            BroadcastHandler {
                tx: station.tx.clone(),
                active: station.active.clone(),
                tick_ct: AtomicU64::new(0),
                voiced_ct: AtomicU64::new(0),
            },
        );
    }

    info!(
        target: "bot_template_rs::radio",
        station = %name,
        guild = %guild_id,
        "broadcast started"
    );

    reply::send(
        ctx_ref(&ctx),
        Reply::new()
            .content(format!(
                "📻 **Broadcasting as `{name}`.** Users in this voice channel \
                 can be heard by any guild that tunes in with `/radio tune {name}`."
            ))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Stop the broadcast originating from this guild.
#[poise::command(slash_command, prefix_command, rename = "silence")]
pub async fn silence(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    // Find a station whose source is this guild. One guild = at most one
    // broadcast, by convention; enforce by removing all matches.
    let names: Vec<String> = ctx
        .data()
        .radio_stations
        .iter()
        .filter_map(|e| {
            if e.value().source_guild == guild_id {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();

    if names.is_empty() {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content("This guild isn't broadcasting anything.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    for name in &names {
        if let Some((_, station)) = ctx.data().radio_stations.remove(name) {
            station.deactivate();
            info!(
                target: "bot_template_rs::radio",
                station = %name,
                "broadcast stopped"
            );
        }
    }

    reply::send(
        ctx_ref(&ctx),
        Reply::new()
            .content(format!("📻 Stopped broadcasting: {}.", names.join(", ")))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Tune this guild's voice channel in to a named station.
#[poise::command(slash_command, prefix_command, rename = "tune")]
pub async fn tune(
    ctx: Context<'_>,
    #[description = "Name of a live station (see /radio stations)"] name: String,
    #[description = "Voice channel to play in (default: bot's current or yours)"]
    #[channel_types("Voice")]
    channel: Option<serenity::ChannelId>,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    let Some(station) = ctx.data().radio_stations.get(&name).map(|s| s.clone()) else {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(format!(
                    "No live station named `{name}`. Run `/radio stations` \
                     to see what's broadcasting."
                ))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    if station.source_guild == guild_id {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content("You can't tune into your own broadcast — that'd feedback-loop.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("songbird not registered")?
        .clone();

    // Resolve target: explicit arg > existing bot call > invoker's VC.
    let call = if let Some(target) = channel {
        manager.join(guild_id, target).await?
    } else if let Some(existing) = manager.get(guild_id) {
        existing
    } else if let Some(invoker_vc) = voice_channel_of(&ctx, guild_id) {
        manager.join(guild_id, invoker_vc).await?
    } else {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(
                    "I'm not in a voice channel and neither are you. Join one, \
                     pass `channel`, or have me `/join` first.",
                )
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    // Replace any existing subscription for this guild.
    if let Some((_, _old)) = ctx.data().radio_subscriptions.remove(&guild_id) {
        // _old dropping aborts its forwarder.
    }

    let forwarder = attach_listener(call, station.subscribe()).await?;
    ctx.data().radio_subscriptions.insert(
        guild_id,
        RadioSubscription {
            station_name: name.clone(),
            started_at: Utc::now(),
            _forwarder: forwarder,
        },
    );

    reply::send(
        ctx_ref(&ctx),
        Reply::new()
            .content(format!("📻 Tuned in to `{name}`."))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Stop listening to the currently tuned station.
#[poise::command(slash_command, prefix_command, rename = "unplug")]
pub async fn unplug(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    if let Some((_, sub)) = ctx.data().radio_subscriptions.remove(&guild_id) {
        let name = sub.station_name.clone();
        drop(sub); // aborts the forwarder
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content(format!("📻 Unplugged from `{name}`."))
                .delete_invoker(true),
        )
        .await?;
    } else {
        reply::send(
            ctx_ref(&ctx),
            Reply::new()
                .content("Not currently tuned in to anything.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
    }
    Ok(())
}

/// List all live stations the bot is hosting right now.
#[poise::command(slash_command, prefix_command, rename = "stations")]
pub async fn stations(ctx: Context<'_>) -> Result<(), Error> {
    let body = if ctx.data().radio_stations.is_empty() {
        "No stations are live right now.".to_string()
    } else {
        let mut lines = vec!["**Live stations:**".to_string()];
        for entry in ctx.data().radio_stations.iter() {
            let s = entry.value();
            lines.push(format!(
                "- `{}` (source: {}, started {})",
                s.name,
                s.source_guild,
                s.started_at.format("%H:%M:%S UTC")
            ));
        }
        lines.join("\n")
    };

    reply::send(
        ctx_ref(&ctx),
        Reply::new()
            .content(body)
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Opt this guild in to radio broadcasting.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "enable",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn enable(ctx: Context<'_>) -> Result<(), Error> {
    set_broadcast_enabled(&ctx, true).await
}

/// (Admin) Disable radio broadcasting for this guild.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "disable",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn disable(ctx: Context<'_>) -> Result<(), Error> {
    set_broadcast_enabled(&ctx, false).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn set_broadcast_enabled(ctx: &Context<'_>, enabled: bool) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let mut entry = ctx
        .data()
        .guild_configs
        .entry(guild_id)
        .or_insert_with(|| crate::data::GuildConfig {
            guild_id: guild_id.get(),
            ..Default::default()
        });
    entry.radio_broadcast_enabled = enabled;
    drop(entry);

    if let Err(e) = ctx.data().save().await {
        reply::send(
            ctx,
            Reply::new()
                .content(format!("Updated in-memory, but persist failed: {e}"))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let body = if enabled {
        "Radio broadcasting **enabled** for this guild."
    } else {
        "Radio broadcasting **disabled** for this guild."
    };
    reply::send(
        ctx,
        Reply::new()
            .content(body)
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

async fn guild_broadcast_enabled(ctx: &Context<'_>, guild: GuildId) -> bool {
    ctx.data()
        .guild_configs
        .get(&guild)
        .map_or(false, |c| c.radio_broadcast_enabled)
}

fn voice_channel_of(ctx: &Context<'_>, guild: GuildId) -> Option<serenity::ChannelId> {
    let cache = &ctx.serenity_context().cache;
    let author_id = ctx.author().id;
    cache.guild(guild).and_then(|g| {
        g.voice_states
            .get(&author_id)
            .and_then(|vs| vs.channel_id)
    })
}

// Tiny shim to take a `&Context` reference in callers that have the owned
// form (`Context<'_>`). Reduces noise where `reply::send` wants a `&Context`.
fn ctx_ref<'a>(c: &'a Context<'a>) -> &'a Context<'a> {
    c
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_starts_active_and_can_deactivate() {
        let s = RadioStation::new("test".into(), GuildId::new(1));
        assert!(s.active.load(Ordering::Relaxed));
        s.deactivate();
        assert!(!s.active.load(Ordering::Relaxed));
    }

    #[test]
    fn station_broadcast_channel_fans_out() {
        let s = RadioStation::new("fan".into(), GuildId::new(1));
        let mut r1 = s.subscribe();
        let mut r2 = s.subscribe();
        let frame: Arc<[i16]> = Arc::from(vec![1i16, 2, 3]);
        s.tx.send(frame.clone()).unwrap();
        assert_eq!(r1.try_recv().unwrap().as_ref(), &[1, 2, 3]);
        assert_eq!(r2.try_recv().unwrap().as_ref(), &[1, 2, 3]);
    }

    #[test]
    fn radio_commands_defined() {
        let cmd = radio();
        assert_eq!(cmd.name, "radio");
        assert!(cmd.guild_only);
        assert!(cmd.subcommand_required);
        let sub: Vec<&str> = cmd.subcommands.iter().map(|c| c.name.as_str()).collect();
        for s in ["broadcast", "silence", "tune", "unplug", "stations", "enable", "disable"] {
            assert!(sub.contains(&s), "missing {s}");
        }
    }
}
