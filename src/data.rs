use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
#[cfg(feature = "lavalink")]
use tracing::warn;

#[cfg(feature = "lavalink")]
pub use crate::lavalink::LavalinkConfig;
#[cfg(feature = "lavalink")]
use crate::lavalink::LavalinkBackend;
#[cfg(feature = "music-core")]
use crate::music_backend::MusicBackend;
#[cfg(feature = "native")]
use crate::native_backend::NativeBackend;
#[cfg(feature = "playlists")]
use crate::playlist::{PlaylistStore, YamlPlaylistStore};
use crate::reply::{Slot, TrackedMessage};

/// Guild configuration structure.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GuildConfig {
    // The ID of the guild
    pub guild_id: u64,
    // For example if you're doing a music bot, this could be the ID of the channel
    // where the bot should send music messages.
    pub music_channel_id: Option<u64>,
}

/// Main centralized data structure for the bot.
#[derive(Clone)]
pub struct Data {
    // Map of guild_id -> guild configuration, you'll need one of these for anything more
    // than the most trivial commands.
    pub guild_configs: dashmap::DashMap<serenity::GuildId, GuildConfig>,
    // Cache from the bot's context, you'll probably need this for some commands
    pub cache: Arc<serenity::Cache>,
    /// Concrete Lavalink backend handle — used by `/lavalink` admin commands
    /// and shared (via `Arc::clone`) into [`Self::music`] when Lavalink is
    /// the active music backend.
    #[cfg(feature = "lavalink")]
    pub lavalink: Arc<LavalinkBackend>,
    /// Backend-agnostic music dispatch. For a Lavalink build this Arc points
    /// at the same object as `lavalink`; a future `music-native` build would
    /// point at a `NativeBackend` instead.
    #[cfg(feature = "music-core")]
    pub music: Arc<dyn MusicBackend>,
    /// User-owned playlist persistence. Decoupled from the music backend via
    /// the [`PlaylistStore`] trait.
    #[cfg(feature = "playlists")]
    pub playlists: Arc<dyn PlaylistStore>,
    /// Live bot-sent messages tracked per (guild, slot) for the
    /// replace-previous behavior. In-memory only; not persisted.
    pub tracked_messages: Arc<dashmap::DashMap<(serenity::GuildId, Slot), TrackedMessage>>,
    /// When the bot process started.
    pub started_at: DateTime<Utc>,
}

impl Default for Data {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Data")
            .field("guild_configs", &self.guild_configs)
            .field("cache", &self.cache)
            .field("started_at", &self.started_at)
            .finish()
    }
}

impl Data {
    // Create a new Data instance
    #[must_use]
    pub fn new() -> Self {
        #[cfg(feature = "lavalink")]
        let lavalink = Arc::new(LavalinkBackend::new(LavalinkConfig::from_env()));
        #[cfg(feature = "native")]
        let native = Arc::new(NativeBackend::new());

        // Backend selection: when both are compiled in, honor the
        // `MUSIC_BACKEND` env var (lavalink|native); default to lavalink for
        // historical-default reasons.
        #[cfg(all(feature = "music-core", feature = "lavalink", feature = "native"))]
        let music: Arc<dyn MusicBackend> = match std::env::var("MUSIC_BACKEND")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Ok("native") => native.clone() as Arc<dyn MusicBackend>,
            _ => lavalink.clone() as Arc<dyn MusicBackend>,
        };
        #[cfg(all(feature = "music-core", feature = "lavalink", not(feature = "native")))]
        let music: Arc<dyn MusicBackend> = lavalink.clone() as Arc<dyn MusicBackend>;
        #[cfg(all(feature = "music-core", feature = "native", not(feature = "lavalink")))]
        let music: Arc<dyn MusicBackend> = native.clone() as Arc<dyn MusicBackend>;

        Self {
            guild_configs: dashmap::DashMap::new(),
            cache: Arc::new(serenity::Cache::default()),
            #[cfg(feature = "lavalink")]
            lavalink: lavalink.clone(),
            #[cfg(feature = "music-core")]
            music,
            #[cfg(feature = "playlists")]
            playlists: Arc::new(YamlPlaylistStore::new("config/playlists")) as Arc<dyn PlaylistStore>,
            tracked_messages: Arc::new(dashmap::DashMap::new()),
            started_at: Utc::now(),
        }
    }

    /// Load data from YAML files.
    ///
    /// Loads guild configurations and the Lavalink configuration from the
    /// config directory. Missing or unreadable files fall back to defaults.
    pub async fn load() -> Self {
        const CONFIG_FILE: &str = "config/bot_config.yaml";

        let data = Self::new();

        if let Ok(file_content) = tokio::fs::read_to_string(CONFIG_FILE).await {
            if let Ok(configs) = serde_yaml::from_str::<Vec<GuildConfig>>(&file_content) {
                for config in configs {
                    let guild_id = serenity::GuildId::new(config.guild_id);
                    data.guild_configs.insert(guild_id, config);
                }
            }
        }

        #[cfg(feature = "lavalink")]
        {
            const LAVALINK_FILE: &str = "config/lavalink.yaml";
            match tokio::fs::read_to_string(LAVALINK_FILE).await {
                Ok(file_content) => match serde_yaml::from_str::<LavalinkConfig>(&file_content) {
                    Ok(config) => {
                        data.lavalink.set_config(config).await;
                    }
                    Err(e) => {
                        warn!(
                            target: "bot_template_rs::data",
                            error = %e,
                            "Failed to parse Lavalink config file; using defaults"
                        );
                    }
                },
                Err(e) => {
                    warn!(
                        target: "bot_template_rs::data",
                        error = %e,
                        "Failed to read Lavalink config file; using defaults"
                    );
                }
            }
        }

        data
    }

    /// Save data to YAML files.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// - The config directory cannot be created
    /// - The configurations cannot be serialized to YAML
    /// - The YAML data cannot be written to disk
    pub async fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        const CONFIG_DIR: &str = "config";
        const CONFIG_FILE: &str = "config/bot_config.yaml";

        if !std::path::Path::new(CONFIG_DIR).exists() {
            tokio::fs::create_dir_all(CONFIG_DIR).await?;
        }

        let configs: Vec<GuildConfig> = self
            .guild_configs
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        let yaml = serde_yaml::to_string(&configs)?;
        tokio::fs::write(CONFIG_FILE, yaml).await?;

        #[cfg(feature = "lavalink")]
        {
            const LAVALINK_FILE: &str = "config/lavalink.yaml";
            let cfg = self.lavalink.config().await;
            let lavalink_yaml = serde_yaml::to_string(&cfg)?;
            tokio::fs::write(LAVALINK_FILE, lavalink_yaml).await?;
        }

        Ok(())
    }
}

/// Tests for the data module
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_data_new() {
        let data = Data::new();
        assert_eq!(data.guild_configs.len(), 0);
        assert!(data.cache.guilds().is_empty());
        #[cfg(feature = "lavalink")]
        assert!(!data.lavalink.is_connected().await);
    }

    #[test]
    fn test_guild_config_default() {
        let config = GuildConfig::default();
        assert_eq!(config.guild_id, 0);
        assert!(config.music_channel_id.is_none());
    }

    #[test]
    fn test_data_debug_impl() {
        let data = Data::new();
        let debug_output = format!("{:?}", data);
        assert!(debug_output.contains("Data"));
        assert!(debug_output.contains("guild_configs"));
        assert!(debug_output.contains("cache"));
    }

    #[test]
    fn test_guild_config_serialization() {
        let config = GuildConfig {
            guild_id: 12345,
            music_channel_id: Some(67890),
        };

        let serialized = serde_yaml::to_string(&config).expect("Failed to serialize");
        assert!(serialized.contains("guild_id: 12345"));
        assert!(serialized.contains("music_channel_id: 67890"));

        let deserialized: GuildConfig = serde_yaml::from_str(&serialized).expect("Failed to deserialize");
        assert_eq!(deserialized.guild_id, 12345);
        assert_eq!(deserialized.music_channel_id, Some(67890));
    }
}
