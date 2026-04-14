//! Music commands backed by a Lavalink node.
//!
//! All commands require that a Lavalink client has been successfully connected
//! (see [`crate::lavalink`]). They share a common helper, [`_join`], to ensure
//! the bot is in a voice channel before attempting playback operations.
//!
//! User-facing replies go through [`crate::reply`] so the music control panel
//! collapses to a single live message per slot (`NowPlaying`, `QueueView`,
//! `Status`) and the invoker's prefix message is cleaned up when possible.

use crate::reply::{self, Reply, Slot};
use crate::{Context, Error};

use lavalink_rs::prelude::*;
use poise::serenity_prelude as serenity;
use serenity::{CreateEmbed, Mentionable};

const MUSIC_EMBED_COLOR: u32 = 0x1DB954;

fn music_embed(title: impl Into<String>, description: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .title(title)
        .description(description)
        .color(MUSIC_EMBED_COLOR)
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

async fn now_playing(ctx: &Context<'_>, embed: CreateEmbed) -> Result<(), Error> {
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

/// Internal helper: ensure the bot is connected to a voice channel and has a
/// player context registered with Lavalink. Returns `true` if a new connection
/// was established.
async fn _join(
    ctx: &Context<'_>,
    guild_id: serenity::GuildId,
    channel_id: Option<serenity::ChannelId>,
) -> Result<bool, Error> {
    let lava_client = {
        let guard = ctx.data().lavalink.read().await;
        match guard.as_ref() {
            Some(c) => c.clone(),
            None => {
                status(
                    ctx,
                    "Lavalink is not connected. Ask an admin to run `/lavalink connect`.",
                    true,
                )
                .await?;
                return Err("lavalink not connected".into());
            }
        }
    };

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("songbird not registered")?
        .clone();

    if lava_client.get_player_context(guild_id).is_none() {
        let connect_to = match channel_id {
            Some(x) => x,
            None => {
                let cache = &ctx.serenity_context().cache;
                let author_id = ctx.author().id;
                let lookup = cache.guild(guild_id).and_then(|g| {
                    g.voice_states
                        .get(&author_id)
                        .and_then(|vs| vs.channel_id)
                });

                match lookup {
                    Some(channel) => channel,
                    None => {
                        status(
                            ctx,
                            "You're not in a voice channel. Join one first, or pass a channel explicitly.",
                            true,
                        )
                        .await?;
                        return Err("Not in a voice channel".into());
                    }
                }
            }
        };

        match manager.join_gateway(guild_id, connect_to).await {
            Ok((connection_info, _)) => {
                lava_client
                    .create_player_context(guild_id, connection_info)
                    .await?;
                status(ctx, format!("Joined {}", connect_to.mention()), false).await?;
                return Ok(true);
            }
            Err(why) => {
                status(ctx, format!("Error joining the channel: {why}"), true).await?;
                return Err(why.into());
            }
        }
    }

    Ok(false)
}

/// Join the voice channel you're in (or a specific one).
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn join(
    ctx: Context<'_>,
    #[description = "The channel to join."]
    #[channel_types("Voice")]
    channel_id: Option<serenity::ChannelId>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    _join(&ctx, guild_id, channel_id).await?;
    Ok(())
}

/// Leave the current voice channel.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("songbird not registered")?
        .clone();

    if let Some(lava_client) = ctx.data().lavalink.read().await.clone() {
        let _ = lava_client.delete_player(guild_id).await;
    }

    if manager.get(guild_id).is_some() {
        manager.remove(guild_id).await?;
    }

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
    term: Option<String>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    _join(&ctx, guild_id, None).await?;

    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Join the bot to a voice channel first.", true).await?;
        return Ok(());
    };

    let query = if let Some(term) = term {
        if term.starts_with("http") {
            term
        } else {
            match SearchEngines::YouTube.to_query(&term) {
                Ok(q) => q,
                Err(why) => {
                    status(&ctx, format!("Error processing search term: {why}"), true).await?;
                    return Err(why.into());
                }
            }
        }
    } else {
        if let Ok(player_data) = player.get_player().await {
            let queue = player.get_queue();

            if player_data.track.is_none()
                && queue.get_track(0).await.is_ok_and(|x| x.is_some())
            {
                player.skip()?;
            } else {
                status(&ctx, "The queue is empty.", true).await?;
            }
        }
        return Ok(());
    };

    let loaded_tracks = match lava_client.load_tracks(guild_id, &query).await {
        Ok(x) => x,
        Err(why) => {
            status(&ctx, format!("Error loading track: {why}"), true).await?;
            return Err(why.into());
        }
    };

    let mut playlist_info = None;

    let mut tracks: Vec<TrackInQueue> = match loaded_tracks.data {
        Some(TrackLoadData::Track(x)) => vec![x.into()],
        Some(TrackLoadData::Search(x)) => vec![x[0].clone().into()],
        Some(TrackLoadData::Playlist(x)) => {
            playlist_info = Some(x.info);
            x.tracks.iter().map(|x| x.clone().into()).collect()
        }
        _ => {
            status(&ctx, format!("No results for `{query}`"), true).await?;
            return Ok(());
        }
    };

    let announcement = if let Some(info) = playlist_info {
        music_embed(
            "Added playlist to queue",
            format!("**{}** — {} track(s)", info.name, tracks.len()),
        )
    } else {
        let track = &tracks[0].track;
        let body = if let Some(uri) = &track.info.uri {
            format!("[{} — {}](<{}>)", track.info.author, track.info.title, uri)
        } else {
            format!("{} — {}", track.info.author, track.info.title)
        };
        music_embed("Added to queue", body)
    };
    now_playing(&ctx, announcement).await?;

    for i in &mut tracks {
        i.track.user_data = Some(serde_json::json!({"requester_id": ctx.author().id.get()}));
    }

    let queue = player.get_queue();
    queue.append(tracks.into())?;

    match player.get_player().await {
        Ok(player_data) => {
            if player_data.track.is_none() && queue.get_track(0).await.is_ok_and(|x| x.is_some()) {
                player.skip()?;
            }
        }
        Err(why) => {
            status(&ctx, format!("Error getting player data: {why}"), true).await?;
            return Err(why.into());
        }
    };

    Ok(())
}

/// Stop playback and clear the current track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Join the bot to a voice channel first.", true).await?;
        return Ok(());
    };

    let np = player.get_player().await?.track;
    if let Some(np) = np {
        player.stop_now().await?;
        now_playing(&ctx, music_embed("Stopped", np.info.title)).await?;
    } else {
        status(&ctx, "Nothing to stop.", true).await?;
    }
    Ok(())
}

/// Pause the currently playing track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Join the bot to a voice channel first.", true).await?;
        return Ok(());
    };

    player.set_pause(true).await?;
    now_playing(&ctx, music_embed("Paused", "⏸")).await?;
    Ok(())
}

/// Resume a paused track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Join the bot to a voice channel first.", true).await?;
        return Ok(());
    };

    player.set_pause(false).await?;
    now_playing(&ctx, music_embed("Resumed", "▶")).await?;
    Ok(())
}

/// Skip the currently playing track.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Join the bot to a voice channel first.", true).await?;
        return Ok(());
    };

    let np = player.get_player().await?.track;
    if let Some(np) = np {
        player.skip()?;
        now_playing(&ctx, music_embed("Skipped", np.info.title)).await?;
    } else {
        status(&ctx, "Nothing to skip.", true).await?;
    }
    Ok(())
}

/// Show the current queue: what's playing now and the next tracks in line.
#[poise::command(slash_command, prefix_command, guild_only)]
pub async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    const MAX_SHOWN: usize = 10;

    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let lava_client = ctx
        .data()
        .lavalink
        .read()
        .await
        .clone()
        .ok_or("lavalink not connected")?;

    let Some(player) = lava_client.get_player_context(guild_id) else {
        status(&ctx, "Nothing is playing.", true).await?;
        return Ok(());
    };

    let np = player.get_player().await?.track;
    let queue_tracks = player.get_queue().get_queue().await?;
    let total = queue_tracks.len();

    let np_line = match np {
        Some(np) => {
            let info = &np.info;
            if let Some(uri) = &info.uri {
                format!("[{} — {}](<{}>)", info.author, info.title, uri)
            } else {
                format!("{} — {}", info.author, info.title)
            }
        }
        None => "_nothing_".to_string(),
    };

    let mut body = format!("**Now playing:** {np_line}\n");
    if total == 0 {
        body.push_str("\nQueue is empty.");
    } else {
        body.push_str(&format!("\n**Up next ({total} track(s)):**\n"));
        for (i, item) in queue_tracks.iter().take(MAX_SHOWN).enumerate() {
            let info = &item.track.info;
            let line = if let Some(uri) = &info.uri {
                format!("{}. [{} — {}](<{}>)\n", i + 1, info.author, info.title, uri)
            } else {
                format!("{}. {} — {}\n", i + 1, info.author, info.title)
            };
            body.push_str(&line);
        }
        if total > MAX_SHOWN {
            body.push_str(&format!("…and {} more", total - MAX_SHOWN));
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
}
