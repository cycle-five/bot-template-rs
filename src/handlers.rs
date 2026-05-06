use crate::EVENT_TARGET;
#[cfg(feature = "music-core")]
use crate::Data;
use poise::serenity_prelude::{self as serenity, Context, EventHandler, FullEvent};

use core::fmt;
use tracing::{info, warn};

pub struct Handler;

#[serenity::async_trait]
impl EventHandler for Handler {
    /// Single dispatch entrypoint replacing the per-event methods that older
    /// serenity exposed (`ready`, `cache_ready`, `guild_create`, …). Keep this
    /// match short — long-running work should be spawned off.
    async fn dispatch(&self, ctx: &Context, event: &FullEvent) {
        match event {
            FullEvent::Ready { data_about_bot, .. } => {
                info!(
                    target: EVENT_TARGET,
                    user = %data_about_bot.user.name,
                    shard = ctx.shard_id.0,
                    guild_count = data_about_bot.guilds.len(),
                    "Bot connected (ready payload)"
                );
                // Backend startup hook (Lavalink connect, etc.). Failure is
                // non-fatal — admins can retry via /lavalink connect.
                #[cfg(feature = "music-core")]
                {
                    let data = ctx.data::<Data>();
                    if let Err(e) = data.music.on_ready(&ctx.http, data_about_bot.user.id).await {
                        warn!(
                            target: "bot_template_rs::music",
                            error = %e,
                            "music backend on_ready failed"
                        );
                    }
                }
            }
            FullEvent::CacheReady { guilds, .. } => {
                let guild_count_cache = ctx.cache.guild_count();
                let guild_count = guilds.len();
                if guild_count != guild_count_cache {
                    warn!(
                        target: EVENT_TARGET,
                        cache_count = guild_count_cache,
                        payload_count = guild_count,
                        "Cache guild count mismatch"
                    );
                }
                info!(
                    target: EVENT_TARGET,
                    guild_count, "Cache ready"
                );
            }
            FullEvent::GuildCreate { guild, is_new, .. } => {
                info!(
                    target: EVENT_TARGET,
                    guild_id = %guild.id,
                    guild_name = %guild.name,
                    is_new = ?is_new,
                    cache_size = ctx.cache.guild_count(),
                    "guild_create"
                );
            }
            _ => {}
        }
    }
}

impl fmt::Debug for Handler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handler").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_implements_event_handler() {
        fn _assert<T: EventHandler>() {}
        _assert::<Handler>();
    }
}
