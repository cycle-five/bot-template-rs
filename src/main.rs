mod logging;
mod commands;
mod data;

use std::env;

use poise::serenity_prelude::{self as serenity};
use serenity::GatewayIntents;
use tracing::{error, info};

// Customize these constants for your bot
pub const BOT_NAME: &str = "bot_template_rs";
pub const COMMAND_TARGET: &str = "bot_template_rs::command";
pub const ERROR_TARGET: &str = "bot_template_rs::error";
pub use data::Data;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// Main function to run the bot
async fn async_main() -> Result<(), Error> {
    // Initialize logging
    logging::init()?;

    // Load environment variables
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN must be set");
    
    // Set up the bot's data
    let data = Data::new();
    
    // Configure the Poise framework
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                // Register commands from commands module
                commands::ping(),
                // Add more commands here as needed
            ],
            pre_command: |ctx| {
                Box::pin(async move {
                    // Log the start of command execution
                    logging::log_command_start(ctx);
                })
            },
            post_command: |ctx| {
                Box::pin(async move {
                    // Log the end of command execution
                    logging::log_command_end(ctx);
                })
            },
            on_error: |error| {
                Box::pin(async move {
                    // Log the error using our logging system
                    crate::logging::log_command_error(&error);

                    // Still handle the error for user feedback
                    match error {
                        poise::FrameworkError::Command { error, ctx, .. } => {
                            let cmd_name = &ctx.command().name;
                            error!("Error in command `{cmd_name}`: {error:?}");

                            if let Err(e) = ctx.say(format!("An error occurred: {error}")).await {
                                error!("Error while sending error message: {e:?}");
                            }
                        }
                        poise::FrameworkError::CommandCheckFailed { error, ctx, .. } => {
                            error!("Command check failed: {error:?}");

                            if let Some(error) = error {
                                if let Err(e) =
                                    ctx.say(format!("Command check failed: {error}")).await
                                {
                                    error!("Error while sending check failure message: {:?}", e);
                                }
                            }
                        }
                        err => {
                            error!("Other framework error: {:?}", err);
                        }
                    }
                })
            },
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(data)
            })
        })
        .build();
    
    // Configure the Serenity client
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create client");
    

    info!("Starting bot...");
    // Start the bot
    if let Err(err) = client.start().await {
        eprintln!("Error starting the bot: {}", err);
    }
    
    Ok(())
}

fn main() {
    // Run the async main function
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main());
    
    // Handle any errors that occurred during execution
    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }
}