//! Lavalink client lifecycle and runtime configuration commands.

use crate::data::LavalinkConfig;
use crate::{Context, Data, Error};

use lavalink_rs::model::events;
use lavalink_rs::prelude::*;
use poise::serenity_prelude as serenity;
use tracing::{info, warn};

/// Build a Lavalink client from the given config and user id.
///
/// The client is constructed with a single node built from `config`. The
/// returned client has already performed its initial websocket handshake (it
/// does so lazily, but the call will not panic if the node is offline - errors
/// simply surface on the first API call).
pub async fn build_client(
    config: &LavalinkConfig,
    user_id: serenity::UserId,
) -> LavalinkClient {
    let node = NodeBuilder {
        hostname: config.hostname.clone(),
        is_ssl: config.is_ssl,
        events: events::Events::default(),
        password: config.password.clone(),
        user_id: user_id.get().into(),
        session_id: None,
    };

    LavalinkClient::new(
        events::Events::default(),
        vec![node],
        NodeDistributionStrategy::round_robin(),
    )
    .await
}

/// Attempt to connect to Lavalink using the current configuration, storing the
/// client in `data.lavalink`. Returns an error message string on failure.
pub async fn connect(data: &Data, user_id: serenity::UserId) -> Result<(), String> {
    let config = data.lavalink_config.read().await.clone();
    info!(target: "bot_template_rs::lavalink",
        hostname = %config.hostname,
        is_ssl = config.is_ssl,
        "Connecting to Lavalink node");

    let client = build_client(&config, user_id).await;

    // Ping the node to verify the connection succeeds. `get_node_by_index`
    // returns immediately; the real test is an HTTP call.
    if let Some(node) = client.get_node_by_index(0usize) {
        if let Err(e) = node.http.version().await {
            warn!(target: "bot_template_rs::lavalink", error = %e, "Lavalink node unreachable");
            return Err(format!("Lavalink node unreachable: {e}"));
        }
    }

    *data.lavalink.write().await = Some(client);
    info!(target: "bot_template_rs::lavalink", "Lavalink connected");
    Ok(())
}

/// Admin-only parent command for configuring and connecting Lavalink at
/// runtime.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("show", "set", "connect_cmd"),
    subcommand_required,
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn lavalink(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Show the current Lavalink configuration (with the password redacted).
#[poise::command(slash_command, prefix_command, rename = "show")]
pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    let cfg = ctx.data().lavalink_config.read().await.clone();
    let connected = ctx.data().lavalink.read().await.is_some();
    let redacted = if cfg.password.is_empty() { "(empty)" } else { "***" };
    ctx.say(format!(
        "**Lavalink config**\n\
         hostname: `{}`\n\
         ssl: `{}`\n\
         password: `{}`\n\
         connected: `{}`",
        cfg.hostname, cfg.is_ssl, redacted, connected
    ))
    .await?;
    Ok(())
}

/// Update one or more Lavalink settings. Any omitted field is left unchanged.
///
/// After updating, this persists the new configuration to disk. Use
/// `/lavalink connect` to actually (re)establish the connection.
#[poise::command(slash_command, prefix_command, rename = "set")]
pub async fn set(
    ctx: Context<'_>,
    #[description = "Hostname:port of the Lavalink node"] hostname: Option<String>,
    #[description = "Password for the Lavalink node"] password: Option<String>,
    #[description = "Whether to use SSL/TLS"] is_ssl: Option<bool>,
) -> Result<(), Error> {
    {
        let mut cfg = ctx.data().lavalink_config.write().await;
        if let Some(h) = hostname {
            cfg.hostname = h;
        }
        if let Some(p) = password {
            cfg.password = p;
        }
        if let Some(s) = is_ssl {
            cfg.is_ssl = s;
        }
    }

    if let Err(e) = ctx.data().save().await {
        ctx.say(format!("Updated in-memory but failed to persist config: {e}"))
            .await?;
        return Ok(());
    }

    ctx.say("Lavalink config updated. Run `/lavalink connect` to apply.")
        .await?;
    Ok(())
}

/// Connect (or reconnect) to Lavalink using the current configuration.
#[poise::command(slash_command, prefix_command, rename = "connect")]
pub async fn connect_cmd(ctx: Context<'_>) -> Result<(), Error> {
    // Drop any existing client so the old websocket is released.
    *ctx.data().lavalink.write().await = None;

    let user_id = ctx.serenity_context().cache.current_user().id;
    match connect(ctx.data(), user_id).await {
        Ok(()) => {
            ctx.say("Connected to Lavalink.").await?;
        }
        Err(e) => {
            ctx.say(format!("Failed to connect: {e}")).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lavalink_commands_defined() {
        let cmd = lavalink();
        assert_eq!(cmd.name, "lavalink");
        assert!(cmd.guild_only);
        assert!(cmd.subcommand_required);
        let sub_names: Vec<&str> = cmd.subcommands.iter().map(|c| c.name.as_str()).collect();
        assert!(sub_names.contains(&"show"));
        assert!(sub_names.contains(&"set"));
        assert!(sub_names.contains(&"connect"));
    }
}
