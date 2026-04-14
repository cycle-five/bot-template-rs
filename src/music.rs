//! Music commands backed by a Lavalink node.
//!
//! All commands require that a Lavalink client has been successfully connected
//! (see [`crate::lavalink`]). They share a common helper, [`_join`], to ensure
//! the bot is in a voice channel before attempting playback operations.

use crate::{Context, Error};

use std::ops::Deref;

use lavalink_rs::prelude::*;
use poise::serenity_prelude as serenity;
use serenity::Mentionable;

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
                ctx.say("Lavalink is not connected. Ask an admin to run `/lavalink connect`.")
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
                let guild = ctx.guild().ok_or("guild cache miss")?.deref().clone();
                let user_channel_id = guild
                    .voice_states
                    .get(&ctx.author().id)
                    .and_then(|voice_state| voice_state.channel_id);

                match user_channel_id {
                    Some(channel) => channel,
                    None => {
                        ctx.say("You are not in a voice channel.").await?;
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

                ctx.say(format!("Joined {}", connect_to.mention())).await?;
                return Ok(true);
            }
            Err(why) => {
                ctx.say(format!("Error joining the channel: {why}")).await?;
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

    ctx.say("Left voice channel.").await?;
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
        ctx.say("Join the bot to a voice channel first.").await?;
        return Ok(());
    };

    let query = if let Some(term) = term {
        if term.starts_with("http") {
            term
        } else {
            SearchEngines::YouTube.to_query(&term)?
        }
    } else {
        if let Ok(player_data) = player.get_player().await {
            let queue = player.get_queue();

            if player_data.track.is_none()
                && queue.get_track(0).await.is_ok_and(|x| x.is_some())
            {
                player.skip()?;
            } else {
                ctx.say("The queue is empty.").await?;
            }
        }
        return Ok(());
    };

    let loaded_tracks = lava_client.load_tracks(guild_id, &query).await?;

    let mut playlist_info = None;

    let mut tracks: Vec<TrackInQueue> = match loaded_tracks.data {
        Some(TrackLoadData::Track(x)) => vec![x.into()],
        Some(TrackLoadData::Search(x)) => vec![x[0].clone().into()],
        Some(TrackLoadData::Playlist(x)) => {
            playlist_info = Some(x.info);
            x.tracks.iter().map(|x| x.clone().into()).collect()
        }
        _ => {
            ctx.say(format!("No results for `{query}`")).await?;
            return Ok(());
        }
    };

    if let Some(info) = playlist_info {
        ctx.say(format!("Added playlist to queue: {}", info.name))
            .await?;
    } else {
        let track = &tracks[0].track;
        if let Some(uri) = &track.info.uri {
            ctx.say(format!(
                "Added to queue: [{} - {}](<{}>)",
                track.info.author, track.info.title, uri
            ))
            .await?;
        } else {
            ctx.say(format!(
                "Added to queue: {} - {}",
                track.info.author, track.info.title
            ))
            .await?;
        }
    }

    for i in &mut tracks {
        i.track.user_data = Some(serde_json::json!({"requester_id": ctx.author().id.get()}));
    }

    let queue = player.get_queue();
    queue.append(tracks.into())?;

    if let Ok(player_data) = player.get_player().await {
        if player_data.track.is_none() && queue.get_track(0).await.is_ok_and(|x| x.is_some()) {
            player.skip()?;
        }
    }

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
        ctx.say("Join the bot to a voice channel first.").await?;
        return Ok(());
    };

    let now_playing = player.get_player().await?.track;
    if let Some(np) = now_playing {
        player.stop_now().await?;
        ctx.say(format!("Stopped {}", np.info.title)).await?;
    } else {
        ctx.say("Nothing to stop.").await?;
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
        ctx.say("Join the bot to a voice channel first.").await?;
        return Ok(());
    };

    player.set_pause(true).await?;
    ctx.say("Paused.").await?;
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
        ctx.say("Join the bot to a voice channel first.").await?;
        return Ok(());
    };

    player.set_pause(false).await?;
    ctx.say("Resumed.").await?;
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
        ctx.say("Join the bot to a voice channel first.").await?;
        return Ok(());
    };

    let now_playing = player.get_player().await?.track;
    if let Some(np) = now_playing {
        player.skip()?;
        ctx.say(format!("Skipped {}", np.info.title)).await?;
    } else {
        ctx.say("Nothing to skip.").await?;
    }
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

        assert!(play().guild_only);
        assert!(skip().guild_only);
    }
}
