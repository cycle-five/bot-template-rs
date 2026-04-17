#[cfg(feature = "tts")]
mod audio_http;
mod commands;
mod data;
mod handlers;
#[cfg(feature = "lavalink")]
mod lavalink;
mod logging;
#[cfg(feature = "music-core")]
mod music;
#[cfg(feature = "music-core")]
mod music_backend;
// Native MusicBackend impl — only meaningful when we have the trait.
// `native` alone (e.g. from `tts`) pulls the songbird driver for playback
// without the music-command layer, so this module stays gated.
#[cfg(all(feature = "native", feature = "music-core"))]
mod native_backend;
#[cfg(feature = "playlists")]
mod playlist;
#[cfg(feature = "record")]
mod record;
mod reply;
mod status;
#[cfg(feature = "tts")]
mod tts;

use std::env;

use poise::serenity_prelude::{self as serenity};
use serenity::GatewayIntents;
#[cfg(feature = "voice")]
use songbird::SerenityInit;
use tracing::{error, info};

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
            commands: {
                #[allow(unused_mut)]
                let mut v = vec![commands::ping(), status::status()];
                #[cfg(feature = "lavalink")]
                v.push(lavalink::lavalink());
                #[cfg(feature = "music-core")]
                v.extend([
                    music::join(),
                    music::leave(),
                    music::play(),
                    music::play_file(),
                    music::stop(),
                    music::pause(),
                    music::resume(),
                    music::skip(),
                    music::queue(),
                ]);
                #[cfg(feature = "playlists")]
                v.push(playlist::playlist());
                #[cfg(feature = "record")]
                v.push(record::record());
                #[cfg(feature = "tts")]
                v.push(tts::tts());
                v
            },
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
                // Register to DISCORD_DEV_GUILD if set (instant propagation,
                // ideal for iterating on new commands); otherwise globally
                // (can take up to an hour to appear).
                match std::env::var("DISCORD_DEV_GUILD")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    Some(guild_id) => {
                        poise::builtins::register_in_guild(
                            ctx,
                            &framework.options().commands,
                            serenity::GuildId::new(guild_id),
                        )
                        .await?;
                        info!(
                            target: "bot_template_rs",
                            guild_id, "Registered commands to dev guild"
                        );
                    }
                    None => {
                        poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                    }
                }

                // Fire the backend's on_ready lifecycle hook (Lavalink opens
                // its control-plane connection here). Failure is non-fatal.
                #[cfg(feature = "music-core")]
                if let Err(e) = data.music.on_ready(ctx, ready.user.id).await {
                    tracing::warn!(
                        target: "bot_template_rs::music",
                        error = %e,
                        "music backend on_ready failed"
                    );
                }
                #[cfg(not(feature = "music-core"))]
                let _ = ready;

                Ok(data)
            })
        })
        .build();

    // Configure the Serenity client
    let intents = GatewayIntents::non_privileged();
    #[cfg(feature = "music")]
    let intents = intents | GatewayIntents::GUILD_VOICE_STATES;

    let client_builder = serenity::ClientBuilder::new(token, intents)
        .event_handler(handlers::Handler)
        .framework(framework);
    #[cfg(feature = "voice")]
    let client_builder = client_builder.register_songbird();
    let mut client = client_builder
        .await
        .expect("Failed to create client");

    // Spawn the audio HTTP layer if BOT_PUBLIC_URL is set. Without a public
    // URL there's nothing for remote pullers (e.g. lavalink) to reach, so
    // the server would just burn a port for no reason.
    #[cfg(feature = "tts")]
    if data_clone.audio_store.public_url().is_some() {
        let bind = env::var("BOT_HTTP_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let store = data_clone.audio_store.clone();
        tokio::spawn(async move {
            if let Err(e) = audio_http::serve(&bind, store).await {
                error!(
                    target: ERROR_TARGET,
                    error = %e,
                    "audio HTTP server exited"
                );
            }
        });
    }

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
