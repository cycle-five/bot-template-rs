mod commands;
mod data;
mod handlers;
mod lavalink;
mod logging;
mod music;
mod reply;
mod status;

use std::env;

use poise::serenity_prelude::{self as serenity};
use serenity::GatewayIntents;
use songbird::SerenityInit;
use tracing::{error, info, warn};

// Customize these constants for your bot
pub const BOT_NAME: &str = "bot_template_rs";
pub const COMMAND_TARGET: &str = "bot_template_rs::command";
pub const ERROR_TARGET: &str = "bot_template_rs::error";
pub const EVENT_TARGET: &str = "bot_template_rs::handlers";
pub const CONSOLE_TARGET: &str = "bot_template_rs";
pub use data::Data;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

/// Main function to run the bot
async fn async_main() -> Result<(), Error> {
    // Load variables from a local .env file (if present) before anything else
    // reads from the environment.
    let _ = dotenvy::dotenv();

    // Initialize logging
    logging::init()?;

    // Load environment variables
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN must be set");
    let prefix = env::var("PREFIX").unwrap_or_else(|_| "!".to_string());

    // Load the bot's data from file
    info!("Loading bot data...");
    let data = Data::load().await;
    let data_clone = data.clone();

    // Configure the Poise framework
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::ping(),
                status::status(),
                lavalink::lavalink(),
                music::join(),
                music::leave(),
                music::play(),
                music::stop(),
                music::pause(),
                music::resume(),
                music::skip(),
                music::queue(),
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
                })
            },
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some(prefix),
                ..Default::default()
            },
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            let data = data.clone();
            Box::pin(async move {
                logging::log_console(
                    "Registering commands and return data, this will go away in the next version of poise"
                );
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                // Try to bring up Lavalink on startup. Failure is non-fatal -
                // admins can always reconnect via `/lavalink connect` later.
                if let Err(e) = lavalink::connect(&data, ready.user.id).await {
                    warn!(
                        target: "bot_template_rs::lavalink",
                        error = %e,
                        "Lavalink connection failed at startup; use /lavalink connect to retry"
                    );
                }

                Ok(data)
            })
        })
        .build();

    // Configure the Serenity client
    // | GatewayIntents::MESSAGE_CONTENT
    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = serenity::ClientBuilder::new(token, intents)
        .event_handler(handlers::Handler)
        .framework(framework)
        .register_songbird()
        .await
        .expect("Failed to create client");

    info!("Starting bot...");

    let client_handle = client.start();

    // Wait for Ctrl+C or other termination signal
    tokio::select! {
        result = client_handle => {
            if let Err(err) = result {
                error!(target: ERROR_TARGET, error = %err, "Bot runtime error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
    }

    // Save data before shutting down
    info!("Saving bot data...");
    if let Err(err) = data_clone.save().await {
        error!(target: ERROR_TARGET, error = %err, "Error saving bot data");
    }

    info!("Bot shutdown complete");
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
        eprintln!("Error: {err}");
    }
}
