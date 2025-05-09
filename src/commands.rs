use crate::{Data, Error};
use poise::{Context, command};

/// Basic ping command
/// This command is used to check if the bot is responsive.
#[command(prefix_command, slash_command, guild_only)]
pub async fn ping(ctx: Context<'_, Data, Error>) -> Result<(), Error> {
    ctx.say("Pong!").await?;
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
        assert!(cmd.description.unwrap_or_else(Default::default).contains("check if the bot is responsive"));
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
