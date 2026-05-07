//! Voice recording: per-user Opus-in-Ogg files, packaged as a zip on stop.
//!
//! # Model
//!
//! Each guild may have at most one active [`RecordingSession`]. Starting a
//! session attaches a global [`RecorderHandler`] to the songbird `Call`; that
//! handler receives [`VoiceTick`](songbird::events::context::data::VoiceTick)
//! events every 20 ms and writes each speaker's raw Opus frames into an Ogg
//! stream keyed by SSRC. Passthrough-encoded Opus means no transcode: what
//! Discord delivers is what hits disk.
//!
//! # Privacy posture
//!
//! Recording announces itself publicly in the invoking text channel as a
//! non-ephemeral message (`🔴 now recording …`). Per-guild opt-out is in
//! `GuildConfig.recording_enabled`; admins toggle via
//! `/record disable`/`/record enable`. Per-user opt-out would require a
//! Discord consent flow — out of scope for this template; users who don't
//! want to be recorded should leave the voice channel until recording stops.
//!
//! # Known limitations (v1)
//!
//! - Per-user Ogg files only. A future "muxer" step (see
//!   [`mux_tracks_todo`]) would time-align and mix them into a single
//!   playback file for users who want one merged recording.
//! - File writes from the voice event handler are synchronous blocking I/O.
//!   For the 20 ms cadence and modest packet sizes this is fine in practice
//!   on any local disk; a channel-fed background writer is the right next
//!   step if production workloads hit this.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, GuildId, MessageId, UserId};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use tracing::{debug, warn};

use crate::data::GuildConfig;
use crate::reply::{self, Reply};
use crate::{Context, Error};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SESSION_ROOT: &str = "config/recordings";
/// Discord's attachment size cap for a tier-0 server (8 MiB).
/// Higher-boost tiers raise this; we fall back to a local save either way
/// on upload failure, so the cap is a guideline rather than a hard gate.
const UPLOAD_BYTE_LIMIT: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// A live recording session for one guild. Dropping the session finalizes
/// all Ogg streams (each writer flushes on Drop).
pub struct RecordingSession {
    pub id: String,
    pub guild: GuildId,
    pub invoker: UserId,
    pub voice_channel: ChannelId,
    pub announce_channel: ChannelId,
    pub started_at: DateTime<Utc>,
    pub announcement_msg: std::sync::Mutex<Option<MessageId>>,
    pub out_dir: PathBuf,
    /// SSRC -> Ogg writer. We use a synchronous `std::sync::Mutex` because
    /// the songbird handler path is hot and per-writer contention is
    /// single-SSRC (each writer only accessed by its own user's packets).
    writers: DashMap<u32, std::sync::Mutex<OpusOggWriter>>,
    /// Learned SSRC -> Discord user id mapping (used for file renaming).
    ssrc_to_user: DashMap<u32, u64>,
    /// Flipped on `/record stop`; the songbird event handler reads this
    /// before doing any work and returns `Some(Event::Cancel)` to detach
    /// itself once it sees the flag set. Songbird has no per-handler
    /// removal API (only `remove_all_global_events`, which would also
    /// detach handlers installed by other features like radio), so this
    /// flag is the only way to take just the recorder offline.
    cancelled: std::sync::atomic::AtomicBool,
}

impl RecordingSession {
    fn new(
        guild: GuildId,
        invoker: UserId,
        voice_channel: ChannelId,
        announce_channel: ChannelId,
    ) -> std::io::Result<Self> {
        let started_at = Utc::now();
        let id = format!("{}_{}", guild.get(), started_at.timestamp());
        let out_dir = PathBuf::from(SESSION_ROOT).join(&id);
        std::fs::create_dir_all(&out_dir)?;
        Ok(Self {
            id,
            guild,
            invoker,
            voice_channel,
            announce_channel,
            started_at,
            announcement_msg: std::sync::Mutex::new(None),
            out_dir,
            writers: DashMap::new(),
            ssrc_to_user: DashMap::new(),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn write_opus(&self, ssrc: u32, frame: &[u8]) {
        // Atomic init: two simultaneous packets for a previously-unseen
        // SSRC must not both try to create the file. `or_try_insert_with`
        // serializes the create call per-key.
        if !self.writers.contains_key(&ssrc) {
            let path = self.out_dir.join(format!("ssrc_{ssrc}.ogg"));
            let entry = self.writers.entry(ssrc);
            entry.or_try_insert_with(|| {
                OpusOggWriter::create(&path, ssrc).map(std::sync::Mutex::new)
            }).map_err(|e: std::io::Error| {
                warn!(
                    target: "bot_template_rs::record",
                    error = %e,
                    path = %path.display(),
                    "failed to open recording file"
                );
            }).ok();
        }
        if let Some(entry) = self.writers.get(&ssrc) {
            if let Ok(mut w) = entry.value().lock() {
                if let Err(e) = w.write_frame(frame) {
                    debug!(
                        target: "bot_template_rs::record",
                        error = %e,
                        ssrc,
                        "failed to write opus frame"
                    );
                }
            }
        }
    }

    fn learn_user(&self, ssrc: u32, user_id: u64) {
        self.ssrc_to_user.insert(ssrc, user_id);
    }

    /// Drop all live writers and rename per-SSRC files to per-user filenames
    /// where the user mapping is known. Returns the list of final file paths.
    fn finalize(&self) -> Vec<PathBuf> {
        // Flush all writers by removing them from the map (Drop runs).
        let keys: Vec<u32> = self.writers.iter().map(|e| *e.key()).collect();
        for k in keys {
            self.writers.remove(&k);
        }
        // Rename ssrc_*.ogg → user_*.ogg where we learned the mapping.
        let mut out = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.out_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let Some(ssrc_str) = name
                    .strip_prefix("ssrc_")
                    .and_then(|s| s.strip_suffix(".ogg"))
                else {
                    out.push(path);
                    continue;
                };
                let Ok(ssrc) = ssrc_str.parse::<u32>() else {
                    out.push(path);
                    continue;
                };
                if let Some(user) = self.ssrc_to_user.get(&ssrc) {
                    let new_path = self.out_dir.join(format!("user_{}.ogg", *user));
                    if std::fs::rename(&path, &new_path).is_ok() {
                        out.push(new_path);
                        continue;
                    }
                }
                out.push(path);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Ogg + Opus stream writing
// ---------------------------------------------------------------------------

struct OpusOggWriter {
    writer: PacketWriter<'static, std::fs::File>,
    serial: u32,
    granule: u64,
}

impl OpusOggWriter {
    fn create(path: &Path, serial: u32) -> std::io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let mut writer = PacketWriter::new(file);
        writer.write_packet(opus_head(2, 48_000), serial, PacketWriteEndInfo::EndPage, 0)?;
        writer.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;
        Ok(Self {
            writer,
            serial,
            granule: 0,
        })
    }

    /// Append one 20 ms Opus frame. Granule clock is 48 kHz, so each frame
    /// advances it by 960 samples.
    fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.granule += 960;
        self.writer.write_packet(
            frame.to_vec(),
            self.serial,
            PacketWriteEndInfo::NormalPacket,
            self.granule,
        )
    }
}

impl Drop for OpusOggWriter {
    fn drop(&mut self) {
        // Close the stream with an empty end-of-stream packet so the file
        // is a well-formed Ogg container.
        let _ = self.writer.write_packet(
            Vec::<u8>::new(),
            self.serial,
            PacketWriteEndInfo::EndStream,
            self.granule,
        );
    }
}

/// Build an OpusHead identification packet per RFC 7845 §5.1.
fn opus_head(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    v.extend_from_slice(b"OpusHead");
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    v.extend_from_slice(&sample_rate.to_le_bytes()); // original input sample rate
    v.extend_from_slice(&0i16.to_le_bytes()); // output gain, 0.0 dB
    v.push(0); // channel mapping family 0 (mono/stereo)
    v
}

/// Build an OpusTags comment-header packet per RFC 7845 §5.2.
fn opus_tags() -> Vec<u8> {
    const VENDOR: &[u8] = b"bot-template-rs";
    let mut v = Vec::new();
    v.extend_from_slice(b"OpusTags");
    v.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    v.extend_from_slice(VENDOR);
    v.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    v
}

/// TODO: align per-user `.ogg` files into a single mixed stream. Requires
/// decoding each file, time-aligning by RTP timestamps (gaps filled with
/// silence), mixing PCM, and re-encoding. Out of scope for v1 — users can
/// post-process with ffmpeg today: `ffmpeg -i user_A.ogg -i user_B.ogg
/// -filter_complex amix=inputs=2 mixed.ogg`.
#[allow(dead_code)]
pub fn mux_tracks_todo() {}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

struct RecorderHandler {
    session: Arc<RecordingSession>,
}

#[async_trait]
impl EventHandler for RecorderHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // First check after stop() flips the flag returns Event::Cancel,
        // which detaches this handler from songbird's global event store
        // without touching any other feature's handlers (radio's broadcast
        // handler in particular).
        if self.session.is_cancelled() {
            return Some(Event::Cancel);
        }
        match ctx {
            EventContext::VoiceTick(tick) => {
                for (ssrc, data) in &tick.speaking {
                    let Some(rtp) = data.packet.as_ref() else {
                        continue;
                    };
                    let end = rtp.packet.len().saturating_sub(rtp.payload_end_pad);
                    if end <= rtp.payload_offset {
                        continue;
                    }
                    let payload = &rtp.packet[rtp.payload_offset..end];
                    self.session.write_opus(*ssrc, payload);
                }
            }
            EventContext::SpeakingStateUpdate(sp) => {
                if let Some(user_id) = sp.user_id {
                    self.session.learn_user(sp.ssrc, user_id.0);
                }
            }
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Start / stop helpers
// ---------------------------------------------------------------------------

async fn guild_recording_enabled(ctx: &Context<'_>, guild: GuildId) -> bool {
    ctx.data()
        .guild_configs
        .get(&guild)
        .map_or(true, |c| c.recording_enabled)
}

async fn set_guild_recording_enabled(
    ctx: &Context<'_>,
    guild: GuildId,
    enabled: bool,
) -> Result<(), Error> {
    let data = ctx.data();
    {
        let mut entry = data
            .guild_configs
            .entry(guild)
            .or_insert_with(|| GuildConfig {
                guild_id: guild.get(),
                ..GuildConfig::default()
            });
        entry.recording_enabled = enabled;
    }
    data.save().await?;
    Ok(())
}

fn find_bot_voice_channel(ctx: &Context<'_>, guild: GuildId) -> Option<ChannelId> {
    let bot_id = ctx.serenity_context().cache.current_user().id;
    ctx.serenity_context().cache.guild(guild).and_then(|g| {
        g.voice_states
            .get(&bot_id)
            .and_then(|vs| vs.channel_id)
    })
}

fn format_duration(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m}m{s:02}s")
    }
}

fn zip_session_files(session: &RecordingSession) -> std::io::Result<PathBuf> {
    let zip_path = session.out_dir.join(format!("recording_{}.zip", session.id));
    let file = std::fs::File::create(&zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for path in session.finalize() {
        if path == zip_path {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".ogg") {
            continue;
        }
        zip.start_file(name, options)?;
        let bytes = std::fs::read(&path)?;
        use std::io::Write;
        zip.write_all(&bytes)?;
    }
    zip.finish()?;
    Ok(zip_path)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Voice recording. Creates per-user Opus/Ogg files and packages them
/// as a zip when stopped.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("start", "stop", "disable", "enable")
)]
pub async fn record(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Begin recording the voice channel the bot is currently in.
#[poise::command(slash_command, prefix_command, rename = "start")]
pub async fn start(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    if !guild_recording_enabled(&ctx, guild_id).await {
        reply::send(
            &ctx,
            Reply::new()
                .content("Recording is disabled in this guild. An admin can enable it with `/record enable`.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    if ctx.data().recordings.contains_key(&guild_id) {
        reply::send(
            &ctx,
            Reply::new()
                .content("A recording is already running. Stop it first with `/record stop`.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let Some(voice_channel) = find_bot_voice_channel(&ctx, guild_id) else {
        reply::send(
            &ctx,
            Reply::new()
                .content("The bot isn't in a voice channel. Use `/join` first.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    let manager = ctx.data().songbird.clone();
    let call = manager.get(guild_id).ok_or("not in voice")?;

    let session = Arc::new(RecordingSession::new(
        guild_id,
        ctx.author().id,
        voice_channel,
        ctx.channel_id().expect_channel(),
    )?);

    {
        let handler = RecorderHandler {
            session: session.clone(),
        };
        // Register the same handler instance against each event we care
        // about; songbird dispatches per event type.
        let mut c = call.lock().await;
        c.add_global_event(
            Event::Core(CoreEvent::VoiceTick),
            RecorderHandler {
                session: session.clone(),
            },
        );
        c.add_global_event(
            Event::Core(CoreEvent::SpeakingStateUpdate),
            handler,
        );
    }

    ctx.data()
        .recordings
        .insert(guild_id, session.clone());

    // Public announcement (intentionally not ephemeral — privacy posture).
    let body = format!(
        "🔴 **Now recording** <#{}>. Started by <@{}>.",
        voice_channel.get(),
        ctx.author().id.get()
    );
    if let Ok(msg) = ctx
        .channel_id()
        .send_message(
            &ctx.serenity_context().http,
            CreateMessage::new().content(body),
        )
        .await
    {
        if let Ok(mut slot) = session.announcement_msg.lock() {
            *slot = Some(msg.id);
        }
    }

    reply::send(
        &ctx,
        Reply::new()
            .content("Recording started.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Stop the active recording, package the files, and upload (or save) the zip.
#[poise::command(slash_command, prefix_command, rename = "stop")]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    let Some((_, session)) = ctx.data().recordings.remove(&guild_id) else {
        reply::send(
            &ctx,
            Reply::new()
                .content("No recording is running.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    // Detach this recording's handlers from songbird without touching any
    // other feature's handlers on the same Call. Flipping the cancel flag
    // makes the handler return `Event::Cancel` on its next dispatch, which
    // tells songbird to drop just it. (We can't use
    // `remove_all_global_events` here — radio also installs global
    // handlers and we'd silently kill its broadcast.)
    session.cancel();

    let duration = Utc::now() - session.started_at;
    let duration_str = format_duration(duration);

    // Run the potentially-slow zip on a blocking thread.
    let session_for_zip = session.clone();
    let zip_result = tokio::task::spawn_blocking(move || zip_session_files(&session_for_zip))
        .await
        .map_err(|e| -> Error { format!("zip task panicked: {e}").into() })?;

    let zip_path = match zip_result {
        Ok(p) => p,
        Err(e) => {
            reply::send(
                &ctx,
                Reply::new()
                    .content(format!("Recording stopped after {duration_str}, but packaging failed: {e}"))
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        }
    };

    let size = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    let within_upload_limit = size > 0 && size <= UPLOAD_BYTE_LIMIT;

    let send_result: Result<(), Error> = if within_upload_limit {
        match CreateAttachment::path(&zip_path) {
            Ok(att) => ctx
                .channel_id()
                .send_message(
                    &ctx.serenity_context().http,
                    CreateMessage::new()
                        .content(format!(
                            "⏹ Recording stopped. Duration: `{duration_str}`."
                        ))
                        .add_file(att),
                )
                .await
                .map(|_| ())
                .map_err(|e| -> Error { Box::new(e) }),
            Err(e) => Err(Box::new(e) as Error),
        }
    } else {
        // Too big (or stat failed) — post a local-path note instead.
        ctx.channel_id()
            .send_message(
                &ctx.serenity_context().http,
                CreateMessage::new().content(format!(
                    "⏹ Recording stopped. Duration: `{duration_str}`. \
                     File too large to upload ({} bytes); saved at `{}`.",
                    size,
                    zip_path.display()
                )),
            )
            .await
            .map(|_| ())
            .map_err(|e| -> Error { Box::new(e) })
    };

    if let Err(e) = send_result {
        warn!(
            target: "bot_template_rs::record",
            error = %e,
            "failed to post recording-stop message"
        );
    }

    // Ack the invoker ephemerally.
    reply::send(
        &ctx,
        Reply::new()
            .content("Recording stopped.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Disable recording in this guild. `/record start` will refuse.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "disable",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn disable(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    set_guild_recording_enabled(&ctx, guild_id, false).await?;
    reply::send(
        &ctx,
        Reply::new()
            .content("Recording disabled for this guild.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Re-enable recording in this guild.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "enable",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn enable(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    set_guild_recording_enabled(&ctx, guild_id, true).await?;
    reply::send(
        &ctx,
        Reply::new()
            .content("Recording enabled for this guild.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn opus_head_has_correct_magic_and_fields() {
        let head = opus_head(2, 48_000);
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[8], 1); // version
        assert_eq!(head[9], 2); // channels
        assert_eq!(&head[12..16], &48_000u32.to_le_bytes());
    }

    #[test]
    fn opus_tags_has_correct_magic() {
        let tags = opus_tags();
        assert_eq!(&tags[..8], b"OpusTags");
    }

    #[test]
    fn writer_writes_a_valid_ogg_stream() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.ogg");
        {
            let mut w = OpusOggWriter::create(&path, 0xdead_beef).unwrap();
            // Fake Opus frames (just any bytes — the validator below only
            // checks container framing, not codec payload validity).
            for _ in 0..5 {
                w.write_frame(&[0u8; 32]).unwrap();
            }
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"OggS"), "Ogg page header must start file");
        assert!(bytes.len() > 60, "expected more than just headers");
    }

    #[test]
    fn format_duration_formats_without_hours() {
        assert_eq!(format_duration(chrono::Duration::seconds(65)), "1m05s");
        assert_eq!(format_duration(chrono::Duration::seconds(3661)), "1h01m01s");
        assert_eq!(format_duration(chrono::Duration::seconds(0)), "0m00s");
    }

    #[test]
    fn record_commands_defined() {
        let cmd = record();
        assert_eq!(cmd.name, "record");
        assert!(cmd.guild_only);
        let names: Vec<&str> = cmd.subcommands.iter().map(|c| &*c.name).collect();
        for n in ["start", "stop", "disable", "enable"] {
            assert!(names.contains(&n), "missing subcommand: {n}");
        }
    }

    #[test]
    fn guild_config_default_enables_recording() {
        assert!(GuildConfig::default().recording_enabled);
    }
}
