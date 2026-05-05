use crate::EVENT_TARGET;
use poise::serenity_prelude::{self as serenity, Context, EventHandler, Guild, GuildId, Ready};

use core::fmt;
use tracing::{info, warn};

pub struct Handler;

#[serenity::async_trait]
impl EventHandler for Handler {
    /// Called when the bot is ready, but the cache may not be fully populated yet.
    async fn ready(&self, ctx: Context, ready: Ready) {
        let user_name = ready.user.name.clone();
        let shard_id = ctx.shard_id;
        let ready_guild_count = ready.guilds.len();
        info!(
            target: EVENT_TARGET,
            user = %user_name,
            shard = %shard_id,
            guild_count = ready_guild_count,
            "Bot connected (ready payload)"
        );
    }

    /// Called when the cache is fully populated.
    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
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
            guild_count = guild_count,
            "Cache ready"
        );
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        info!(
            target: EVENT_TARGET,
            guild_id = %guild.id,
            guild_name = %guild.name,
            is_new = ?is_new,
            cache_size = ctx.cache.guild_count(),
            "guild_create"
        );
    }
}

/// Debug implementation for Handler, includes no fields
/// since they aren't usually printable anyhow.
impl fmt::Debug for Handler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handler")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test the Handler struct can be created
    #[test]
    fn test_handler_creation() {
        let handler = Handler;
        let handler_ref = &handler;
        assert!(true, "Handler can be created: {:?}", handler_ref);
    }

    // Since we can't easily mock Context and Ready objects due to their complex structure,
    // we'll test what we can about our handler implementation.
    #[test]
    fn test_handler_implements_event_handler() {
        // This test verifies at compile time that Handler implements EventHandler
        fn assert_impl<T: EventHandler>() {}
        assert_impl::<Handler>();
    }
}
