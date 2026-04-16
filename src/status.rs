//! The `status` command: build & runtime information for the bot.

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

/// Parse a seconds-since-epoch string (from build-time env) into a DateTime.
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
    let build_ts = parse_build_ts()
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    let uptime = format_uptime(data.started_at);

    let mut out = String::new();
    out.push_str(&format!("**{name}** v{version}\n"));
    out.push_str(&format!("commit: `{git_hash}`\n"));
    out.push_str(&format!("built: `{build_ts}`\n"));
    out.push_str(&format!("uptime: `{uptime}`\n"));

    #[cfg(feature = "lavalink")]
    {
        let lavalink_connected = data.lavalink.read().await.is_some();
        out.push_str(&format!(
            "lavalink: `{}`\n",
            if lavalink_connected { "connected" } else { "disconnected" }
        ));
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

        out.push_str("\n**Build**\n");
        out.push_str(&format!("rustc: `{rustc}`\n"));
        out.push_str(&format!("profile: `{profile}`\n"));
        out.push_str(&format!("target: `{target_os}/{target_arch}`\n"));

        out.push_str("\n**Runtime**\n");
        out.push_str(&format!("pid: `{pid}`\n"));
        out.push_str(&format!("started: `{started_at}`\n"));
        out.push_str(&format!("shard: `{shard_id}`\n"));
        out.push_str(&format!("guild count (cache): `{guild_count}`\n"));
        out.push_str(&format!("configured guilds: `{configured_guilds}`\n"));

        #[cfg(feature = "lavalink")]
        {
            let lava_cfg = data.lavalink_config.read().await.clone();
            let lava_host = lava_cfg.hostname;
            let lava_ssl = lava_cfg.is_ssl;

            let lava_node_count = if let Some(client) = data.lavalink.read().await.as_ref() {
                // Count nodes by probing successive indices. lavalink-rs doesn't
                // expose a direct `len()`, so this is the portable approach.
                let mut n = 0usize;
                while client.get_node_by_index(n).is_some() {
                    n += 1;
                }
                n
            } else {
                0
            };

            out.push_str("\n**Lavalink**\n");
            out.push_str(&format!("host: `{lava_host}`\n"));
            out.push_str(&format!("ssl: `{lava_ssl}`\n"));
            out.push_str(&format!("active nodes: `{lava_node_count}`\n"));
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
        assert!(s.contains("h"));
        assert!(s.contains("m"));
        assert!(s.contains("s"));
    }
}
