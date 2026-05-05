//! The `status` command: build & runtime information for the bot.

use std::fmt::Write;

use crate::reply::{self, Reply, Slot};
use crate::{Context, Error};

use chrono::{DateTime, Utc};

/// Format a duration as `HhMmSs`, e.g. `3h12m05s`.
fn format_uptime(started: DateTime<Utc>) -> String {
    let now = Utc::now();
    let total = (now - started).num_seconds().max(0);
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}d{hours:02}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    }
}

/// Parse a seconds-since-epoch string (from build-time env) into a `DateTime`.
fn parse_build_ts() -> Option<DateTime<Utc>> {
    let raw = option_env!("BUILD_TIMESTAMP")?;
    let secs: i64 = raw.parse().ok()?;
    DateTime::<Utc>::from_timestamp(secs, 0)
}

/// Report build and runtime information about the bot.
///
/// Pass `verbose: true` for a much more detailed dump including the Lavalink
/// connection details, guild counts, and environment info.
#[poise::command(slash_command, prefix_command)]
pub async fn status(
    ctx: Context<'_>,
    #[description = "Include verbose runtime details"] verbose: Option<bool>,
) -> Result<(), Error> {
    let verbose = verbose.unwrap_or(false);
    let data = ctx.data();

    let name = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    let git_hash = option_env!("BUILD_GIT_HASH").unwrap_or("unknown");
    let build_ts = parse_build_ts().map_or_else(|| "unknown".to_string(), |t| t.to_rfc3339());
    let uptime = format_uptime(data.started_at);

    let mut out = String::new();
    let _ = writeln!(out, "**{name}** v{version}");
    let _ = writeln!(out, "commit: `{git_hash}`");
    let _ = writeln!(out, "built: `{build_ts}`");
    let _ = writeln!(out, "uptime: `{uptime}`");

    #[cfg(feature = "lavalink")]
    {
        let lavalink_connected = data.lavalink.is_connected().await;
        let _ = writeln!(
            out,
            "lavalink: `{}`",
            if lavalink_connected { "connected" } else { "disconnected" }
        );
    }

    if verbose {
        let rustc = option_env!("BUILD_RUSTC_VERSION").unwrap_or("unknown");
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        let target_os = std::env::consts::OS;
        let target_arch = std::env::consts::ARCH;
        let pid = std::process::id();

        let guild_count = ctx.serenity_context().cache.guild_count();
        let shard_id = ctx.serenity_context().shard_id;
        let configured_guilds = data.guild_configs.len();
        let started_at = data.started_at.to_rfc3339();


        let _ = write!(out, "\n**Build**\n");
        let _ = writeln!(out, "rustc: `{rustc}`");
        let _ = writeln!(out, "profile: `{profile}`");
        let _ = writeln!(out, "target: `{target_os}/{target_arch}`");

        let _ = write!(out, "\n**Runtime**\n");
        let _ = writeln!(out, "pid: `{pid}`");
        let _ = writeln!(out, "started: `{started_at}`");
        let _ = writeln!(out, "shard: `{shard_id}`");
        let _ = writeln!(out, "guild count (cache): `{guild_count}`");
        let _ = writeln!(out, "configured guilds: `{configured_guilds}`");

        #[cfg(feature = "lavalink")]
        {
            let lava_cfg = data.lavalink.config().await;
            let lava_node_count = data.lavalink.node_count().await;

            let _ = write!(out, "\n**Lavalink**\n");
            let _ = writeln!(out, "host: `{}`", lava_cfg.hostname);
            let _ = writeln!(out, "ssl: `{}`", lava_cfg.is_ssl);
            let _ = writeln!(out, "active nodes: `{lava_node_count}`");
        }
    }

    reply::send(
        &ctx,
        Reply::new()
            .content(out)
            .slot(Slot::BotStatus)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_command_defined() {
        let cmd = status();
        assert_eq!(cmd.name, "status");
    }

    #[test]
    fn uptime_formats_reasonably() {
        let started = Utc::now() - chrono::Duration::seconds(3 * 3600 + 12 * 60 + 5);
        let s = format_uptime(started);
        assert!(s.contains('h'));
        assert!(s.contains('m'));
        assert!(s.contains('s'));
    }
}
