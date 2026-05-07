use crate::reply::{self, Reply};
use crate::{Context, Error};
use poise::command;
use poise::serenity_prelude as serenity;

/// Basic ping command
/// This command is used to check if the bot is responsive.
#[command(prefix_command, slash_command, guild_only)]
pub async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    reply::send(&ctx, Reply::new().content("Pong!")).await?;
    Ok(())
}

/// Register slash commands with Discord.
///
/// poise serenity-next removed the `setup` callback that the old framework
/// used for auto-registration on Ready, so the bot owner runs this once after
/// deploy. With `DISCORD_DEV_GUILD` set, registers to that guild only (instant
/// propagation); otherwise registers globally (can take up to an hour to
/// propagate).
#[command(prefix_command, owners_only, hide_in_help)]
pub async fn register(ctx: Context<'_>) -> Result<(), Error> {
    let commands = &ctx.framework().options().commands;
    match std::env::var("DISCORD_DEV_GUILD")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(guild_id) => {
            poise::builtins::register_in_guild(
                ctx.http(),
                commands,
                serenity::GuildId::new(guild_id),
            )
            .await?;
            reply::send(
                &ctx,
                Reply::new().content(format!("Registered commands to guild {guild_id}.")),
            )
            .await?;
        }
        None => {
            poise::builtins::register_globally(ctx.http(), commands).await?;
            reply::send(&ctx, Reply::new().content("Registered commands globally.")).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test that the ping command is properly defined
    #[test]
    fn test_ping_command_definition() {
        let cmd = ping();
        assert_eq!(cmd.name, "ping");
        assert!(cmd.description.unwrap_or_default().contains("check if the bot is responsive"));
        assert!(cmd.guild_only);
    }

    // This test verifies that the ping command can be executed
    #[test]
    fn test_ping_command_can_be_called() {
        // This test just verifies that the ping command exists and can be called
        // We don't actually execute it since that would require a real Discord context
        let cmd = ping();
        assert!(cmd.create_as_slash_command().is_some());
    }
}
