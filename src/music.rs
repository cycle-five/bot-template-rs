//! Backend-agnostic music commands.
//!
//! All commands dispatch through [`crate::music_backend::MusicBackend`] via
//! `ctx.data().music`. Backend-specific state and protocol details live in
//! the backend impl (e.g. [`crate::lavalink::LavalinkBackend`]).
//!
//! User-facing replies go through [`crate::reply`] so the music control panel
//! collapses to a single live message per slot (`NowPlaying`, `QueueView`,
//! `Status`) and the invoker's prefix message is cleaned up when possible.

use std::fmt::Write;
use crate::music_backend::{PlayResult, Track};
use crate::reply::{self, Reply, Slot};
use crate::{Context, Error};

use poise::serenity_prelude as serenity;
use serenity::{CreateEmbed, Mentionable};

const MUSIC_EMBED_COLOR: u32 = 0x001D_B954;
const QUEUE_MAX_SHOWN: usize = 10;

fn music_embed(title: impl Into<String>, description: impl Into<String>) -> CreateEmbed<'static> {
    CreateEmbed::new()
        .title(title.into())
        .description(description.into())
        .color(MUSIC_EMBED_COLOR)
}

fn track_line(t: &Track) -> String {
    match &t.uri {
        Some(uri) => format!("[{} — {}](<{}>)", t.author, t.title, uri),
        None => format!("{} — {}", t.author, t.title),
    }
}

async fn status(
    ctx: &Context<'_>,
    msg: impl Into<String>,
    ephemeral: bool,
) -> Result<(), Error> {
    reply::send(
        ctx,
        Reply::new()
            .content(msg)
            .slot(Slot::Status)
            .ephemeral(ephemeral)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

async fn now_playing(ctx: &Context<'_>, embed: CreateEmbed<'static>) -> Result<(), Error> {
    reply::send(
        ctx,
        Reply::new()
            .embed(embed)
            .slot(Slot::NowPlaying)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Resolve the voice channel to join: the explicit override, or the channel
/// the invoker is currently in.
fn resolve_target_channel(
    ctx: &Context<'_>,
    guild_id: serenity::GuildId,
    explicit: Option<serenity::ChannelId>,
) -> Option<serenity::ChannelId> {
    if let Some(c) = explicit {
        return Some(c);
    }
    let cache = &ctx.serenity_context().cache;
    let author_id = ctx.author().id;
    cache.guild(guild_id).and_then(|g| {
        g.voice_states
            .get(&author_id)
            .and_then(|vs| vs.channel_id)
    })
}

/// Join helper: if the bot is already in voice and the caller didn't force a
/// specific channel, accept the existing connection (so `/play` works from
/// text channels even when the invoker isn't in voice themselves). Otherwise
/// resolve the target channel and ask the backend to join.
pub(crate) async fn join_voice(
    ctx: &Context<'_>,
    guild_id: serenity::GuildId,
    explicit: Option<serenity::ChannelId>,
) -> Result<(), Error> {
    if explicit.is_none() && ctx.data().songbird.get(guild_id).is_some() {
        return Ok(());
    }

    let backend = ctx.data().music.clone();
    let Some(channel) = resolve_target_channel(ctx, guild_id, explicit) else {
        status(
            ctx,
            "You're not in a voice channel. Join one first, or pass a channel explicitly.",
            true,
        )
        .await?;
        return Err("Not in a voice channel".into());
    };
    match backend
        .ensure_joined(ctx.serenity_context(), guild_id, channel)
        .await
    {
        Ok(newly_joined) => {
            if newly_joined {
                status(ctx, format!("Joined {}", channel.mention()), false).await?;
            }
            Ok(())
        }
        Err(why) => {
            status(ctx, format!("Error joining the channel: {why}"), true).await?;
            Err(why)
        }
    }
}

/// Join the voice channel you're in (or a specific one).
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn join(
    ctx: Context<'_>,
    #[description = "The channel to join."]
    #[channel_types("Voice")]
    channel_id: Option<serenity::GenericChannelId>,
) -> Result<(), Error> {
    // Native songbird's join() waits for the full driver/websocket handshake
    // (can exceed 10s). Lavalink's path is faster but still worth deferring.
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let channel_id = channel_id.map(serenity::GenericChannelId::expect_channel);
    join_voice(&ctx, guild_id, channel_id).await?;
    Ok(())
}

/// Leave the current voice channel.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    ctx.data()
        .music
        .leave(ctx.serenity_context(), guild_id)
        .await?;
    status(&ctx, "Left voice channel.", false).await?;
    Ok(())
}

/// Play a track from a URL or a search term.
///
/// With no argument, resume from a paused state or advance in the queue.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "Search term or URL"]
    #[rest]
    term: String,
) -> Result<(), Error> {
    // Extend Discord's 3s interaction deadline to 15m — yt-dlp/lavalink
    // resolves can easily exceed the default window on playlist URLs or
    // slow searches. No-op for prefix commands.
    ctx.defer().await?;

    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let backend = ctx.data().music.clone();

    // Ensure the bot is in voice before doing anything.
    join_voice(&ctx, guild_id, None).await?;

    let query = term.trim();
    if query.is_empty() {
        // No argument: resume if paused and show what's playing. Slash
        // commands must respond within ~3s or Discord shows "application did
        // not respond", so always send something back here.
        backend.resume(guild_id).await.ok();
        match backend.now_playing(guild_id).await? {
            Some(t) => {
                now_playing(&ctx, music_embed("Now playing", track_line(&t))).await?;
            }
            None => {
                status(
                    &ctx,
                    "Nothing is playing. Pass a URL or search term to queue something.",
                    true,
                )
                .await?;
            }
        }
        return Ok(());
    }

    let result = match backend
        .play(guild_id, query, ctx.author().id)
        .await
    {
        Ok(r) => r,
        Err(why) => {
            status(&ctx, format!("Error loading track: {why}"), true).await?;
            return Err(why);
        }
    };

    let announcement = match result {
        PlayResult::Added(t) => music_embed("Added to queue", track_line(&t)),
        PlayResult::Playlist { name, count, .. } => music_embed(
            "Added playlist to queue",
            format!("**{name}** — {count} track(s)"),
        ),
        PlayResult::NoMatch => {
            status(&ctx, format!("No results for `{query}`"), true).await?;
            return Ok(());
        }
    };
    now_playing(&ctx, announcement).await?;
    Ok(())
}

/// Stop the currently playing track. Use /clear to also drain the queue.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let stopped = ctx.data().music.stop(guild_id).await?;
    match stopped {
        Some(t) => now_playing(&ctx, music_embed("Stopped", t.title)).await?,
        None => status(&ctx, "Nothing to stop.", true).await?,
    }
    Ok(())
}

/// Drain the upcoming queue. Does not stop the currently playing track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let cleared = ctx.data().music.queue_snapshot(guild_id).await?.len();
    ctx.data().music.clear(guild_id).await?;
    let body = if cleared == 0 {
        "Queue was already empty.".to_string()
    } else {
        format!("Cleared {cleared} track(s) from the queue.")
    };
    now_playing(&ctx, music_embed("Cleared", body)).await?;
    Ok(())
}

/// Pause the currently playing track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    ctx.data().music.pause(guild_id).await?;
    now_playing(&ctx, music_embed("Paused", "⏸")).await?;
    Ok(())
}

/// Resume a paused track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    ctx.data().music.resume(guild_id).await?;
    now_playing(&ctx, music_embed("Resumed", "▶")).await?;
    Ok(())
}

/// Skip the currently playing track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let skipped = ctx.data().music.skip(guild_id).await?;
    match skipped {
        Some(t) => now_playing(&ctx, music_embed("Skipped", t.title)).await?,
        None => status(&ctx, "Nothing to skip.", true).await?,
    }
    Ok(())
}

/// Play audio attachments from a Discord message.
///
/// Works as a message context-menu action ("Apps → Play attachments").
/// Discord attachments live on the Discord CDN at publicly reachable URLs,
/// so we route each audio attachment through [`MusicBackend::play_url`] —
/// no bot-hosted HTTP server required.
#[poise::command(context_menu_command = "Play attachments", guild_only)]
pub async fn play_file(
    ctx: Context<'_>,
    #[description = "Message with an audio attachment"] msg: serenity::Message,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let backend = ctx.data().music.clone();

    join_voice(&ctx, guild_id, None).await?;

    let audio_attachments: Vec<&serenity::Attachment> = msg
        .attachments
        .iter()
        .filter(|a| is_audio_attachment(a))
        .collect();

    if audio_attachments.is_empty() {
        status(&ctx, "That message has no audio attachments.", true).await?;
        return Ok(());
    }

    let mut added = 0usize;
    let mut last_embed: Option<CreateEmbed<'static>> = None;
    for att in &audio_attachments {
        match backend
            .play_url(guild_id, &att.url, ctx.author().id)
            .await
        {
            Ok(PlayResult::Added(t)) => {
                added += 1;
                last_embed = Some(music_embed("Added attachment", track_line(&t)));
            }
            Ok(PlayResult::Playlist { name, count, .. }) => {
                added += count;
                last_embed = Some(music_embed(
                    "Added attachment playlist",
                    format!("**{name}** — {count} track(s)"),
                ));
            }
            Ok(PlayResult::NoMatch) => {
                // One bad attachment shouldn't abort the others.
            }
            Err(why) => {
                status(&ctx, format!("Error loading `{}`: {why}", att.filename), true).await?;
            }
        }
    }

    if added == 0 {
        status(&ctx, "Couldn't resolve any of those attachments.", true).await?;
        return Ok(());
    }

    if let Some(embed) = last_embed {
        now_playing(&ctx, embed).await?;
    }
    Ok(())
}

fn is_audio_attachment(a: &serenity::Attachment) -> bool {
    if let Some(ct) = a.content_type.as_deref()
        && ct.starts_with("audio/") {
            return true;
        }
    let name = a.filename.to_ascii_lowercase();
    matches!(
        name.rsplit('.').next(),
        Some("wav" | "mp3" | "ogg" | "opus" | "flac" | "m4a" | "aac" | "webm")
    )
}

/// Show the current queue: what's playing now and the next tracks in line.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let backend = ctx.data().music.clone();

    let np = backend.now_playing(guild_id).await?;
    let tracks = backend.queue_snapshot(guild_id).await?;
    let total = tracks.len();

    let np_line = match np {
        Some(t) => track_line(&t),
        None => "_nothing_".to_string(),
    };

    let mut body = format!("**Now playing:** {np_line}\n");
    if total == 0 {
        let _ = write!(body, "\nQueue is empty.");
    } else {
        let _ = write!(body, "\n**Up next ({total} track(s)):**\n");
        for (i, t) in tracks.iter().take(QUEUE_MAX_SHOWN).enumerate() {
            let _ = writeln!(body, "{}. {}", i + 1, track_line(t));
        }
        if total > QUEUE_MAX_SHOWN {
            let _ = write!(body, "…and {} more", total - QUEUE_MAX_SHOWN);
        }
    }

    reply::send(
        &ctx,
        Reply::new()
            .embed(music_embed("Queue", body))
            .slot(Slot::QueueView)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_commands_defined() {
        assert_eq!(join().name, "join");
        assert_eq!(leave().name, "leave");
        assert_eq!(play().name, "play");
        assert_eq!(stop().name, "stop");
        assert_eq!(pause().name, "pause");
        assert_eq!(resume().name, "resume");
        assert_eq!(skip().name, "skip");
        assert_eq!(queue().name, "queue");

        assert!(play().guild_only);
        assert!(skip().guild_only);
    }

    #[test]
    fn track_line_formats_with_and_without_uri() {
        let t1 = Track {
            title: "T".into(),
            author: "A".into(),
            uri: Some("https://x".into()),
            duration_ms: None,
            requester: None,
        };
        assert_eq!(track_line(&t1), "[A — T](<https://x>)");

        let t2 = Track {
            uri: None,
            ..t1
        };
        assert_eq!(track_line(&t2), "A — T");
    }
}
