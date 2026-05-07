#[cfg(feature = "tts")]
mod audio_http;
mod commands;
mod data;
mod handlers;
#[cfg(feature = "lavalink")]
mod lavalink;
mod logging;
// `music` (the user-facing command surface) needs both the backend trait
// and an actual backend impl. `music-core` alone (e.g. from `playlists`)
// pulls in only the trait + Track type for serialization. `tts` pulls
// `native` (songbird's driver for playback) without the command surface,
// so we require music-core explicitly.
#[cfg(all(feature = "music-core", any(feature = "lavalink", feature = "native")))]
mod music;
#[cfg(feature = "music-core")]
mod music_backend;
// Native MusicBackend impl — only meaningful when we have the trait.
// `native` alone (e.g. from `tts`) pulls the songbird driver for playback
// without the music-command layer, so this module stays gated.
#[cfg(all(feature = "native", feature = "music-core"))]
mod native_backend;
// `playlists` adds /playlist save/load/list/feature commands that drive a
// music backend, so it requires either lavalink or native to be useful.
#[cfg(all(feature = "playlists", not(any(feature = "lavalink", feature = "native"))))]
compile_error!(
    "the `playlists` feature needs a music backend; enable `lavalink` or `native` \
     (or the `music` / `music-native` bundles)"
);
#[cfg(feature = "playlists")]
mod playlist;
#[cfg(feature = "radio")]
mod radio;
#[cfg(feature = "record")]
mod record;
mod reply;
mod status;
#[cfg(feature = "stt")]
mod stt;
#[cfg(feature = "tts")]
mod tts;

use std::env;
use std::sync::Arc;

use poise::serenity_prelude::{self as serenity};
use serenity::GatewayIntents;
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

/// Build poise FrameworkOptions with our commands and lifecycle hooks.
fn framework_options(prefix: String) -> poise::FrameworkOptions<Data, Error> {
    poise::FrameworkOptions {
        commands: {
            #[allow(unused_mut)]
            let mut v = vec![commands::ping(), commands::register(), status::status()];
            #[cfg(feature = "lavalink")]
            v.push(lavalink::lavalink());
            #[cfg(all(feature = "music-core", any(feature = "lavalink", feature = "native")))]
            v.extend([
                music::join(),
                music::leave(),
                music::play(),
                music::play_file(),
                music::stop(),
                music::clear(),
                music::pause(),
                music::resume(),
                music::skip(),
                music::queue(),
                music::shuffle(),
                music::jump(),
                music::move_track(),
                music::remove(),
                music::remove_dupes(),
                music::leave_cleanup(),
                music::seek(),
                music::volume(),
                music::loop_mode(),
                music::previous(),
                music::lyrics(),
            ]);
            #[cfg(feature = "playlists")]
            v.push(playlist::playlist());
            #[cfg(feature = "record")]
            v.push(record::record());
            #[cfg(feature = "tts")]
            v.push(tts::tts());
            #[cfg(feature = "stt")]
            {
                v.push(stt::stt());
                v.push(stt::transcribe_message());
            }
            #[cfg(feature = "radio")]
            v.push(radio::radio());
            v
        },
        pre_command: |ctx| {
            Box::pin(async move {
                logging::log_command_start(ctx);
            })
        },
        post_command: |ctx| {
            Box::pin(async move {
                logging::log_command_end(ctx);
            })
        },
        on_error: |error| {
            Box::pin(async move {
                crate::logging::log_command_error(&error).await;
            })
        },
        prefix_options: poise::PrefixFrameworkOptions {
            prefix: Some(prefix.into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Main function to run the bot
async fn async_main() -> Result<(), Error> {
    // Load variables from a local .env file (if present) before anything else
    // reads from the environment.
    let _ = dotenvy::dotenv();

    // Pick a rustls CryptoProvider before any TLS code runs. Both `aws-lc-rs`
    // and `ring` end up in the tree (reqwest selects aws-lc-rs; some other
    // transitive enables ring), so rustls 0.23 refuses to auto-install.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls aws-lc-rs CryptoProvider");

    // Initialize logging
    logging::init()?;

    // Load environment variables
    let token = serenity::Token::from_env("DISCORD_TOKEN")
        .expect("DISCORD_TOKEN must be set");
    let prefix = env::var("PREFIX").unwrap_or_else(|_| "!".to_string());

    // Build the Songbird voice manager up-front so we can stash an Arc both
    // in serenity (so it dispatches voice gateway events to it) and in our
    // own Data (so backends drive playback without going through ctx).
    //
    // Radio needs decoded PCM from VoiceTick (default mode only decrypts).
    // Record is fine with either — it takes the raw Opus payload straight
    // off the packet. So: enable full decode when radio is compiled in,
    // otherwise use songbird's defaults.
    #[cfg(feature = "voice")]
    let songbird = {
        #[cfg(feature = "radio")]
        let sb_config = songbird::Config::default().decode_mode(
            songbird::driver::DecodeMode::Decode(songbird::driver::DecodeConfig::default()),
        );
        #[cfg(not(feature = "radio"))]
        let sb_config = songbird::Config::default();
        songbird::Songbird::serenity_from_config(sb_config)
    };

    // Load the bot's data from file
    info!("Loading bot data...");
    #[cfg(feature = "voice")]
    let data = Data::load(songbird.clone()).await;
    #[cfg(not(feature = "voice"))]
    let data = Data::load().await;
    let data_clone = data.clone();

    // Configure the Serenity client
    let intents = GatewayIntents::non_privileged();
    // Any voice-touching feature needs voice-state events, not just music.
    // record/radio/tts on the native side all need to know when users join
    // and leave the bot's channel.
    #[cfg(feature = "voice")]
    let intents = intents | GatewayIntents::GUILD_VOICE_STATES;

    let framework = poise::Framework::new(framework_options(prefix));

    let client_builder = serenity::ClientBuilder::new(token, intents)
        .event_handler(Arc::new(handlers::Handler))
        .framework(Box::new(framework))
        .data(Arc::new(data) as _);
    #[cfg(feature = "voice")]
    let client_builder = client_builder.voice_manager(songbird);
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
