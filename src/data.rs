use std::sync::Arc;

use chrono::{DateTime, Utc};
use lavalink_rs::client::LavalinkClient;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

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

/// Configuration for connecting to a Lavalink node.
///
/// All fields may be updated at runtime via bot commands and then persisted
/// to the config directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LavalinkConfig {
    /// Hostname (including port) for the Lavalink node, e.g. `localhost:2333`.
    pub hostname: String,
    /// Password used to authenticate with the Lavalink node.
    pub password: String,
    /// Whether to connect using TLS / WSS.
    pub is_ssl: bool,
}

impl Default for LavalinkConfig {
    fn default() -> Self {
        Self {
            hostname: "localhost:2333".to_string(),
            password: "youshallnotpass".to_string(),
            is_ssl: false,
        }
    }
}

impl LavalinkConfig {
    /// Build a config from `LAVALINK_HOST`, `LAVALINK_PASSWORD`, and
    /// `LAVALINK_SSL` environment variables, falling back to defaults for
    /// any unset fields.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            hostname: std::env::var("LAVALINK_HOST").unwrap_or(defaults.hostname),
            password: std::env::var("LAVALINK_PASSWORD").unwrap_or(defaults.password),
            is_ssl: std::env::var("LAVALINK_SSL")
                .ok()
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(defaults.is_ssl),
        }
    }
}

/// Main centrailized data structure for the bot. Should it use the `InnerData` idiom?
#[derive(Clone)]
pub struct Data {
    // Map of guild_id -> guild configuration, you'll need one of these for anything more
    // than the most trivial commands.
    pub guild_configs: dashmap::DashMap<serenity::GuildId, GuildConfig>,
    // Cache from the bot's context, you'll probably need this for some commands
    pub cache: Arc<serenity::Cache>,
    /// The Lavalink connection configuration. Mutable at runtime.
    pub lavalink_config: Arc<RwLock<LavalinkConfig>>,
    /// The active Lavalink client, if connected.
    ///
    /// This is `None` until the bot successfully connects to a Lavalink node.
    /// Updating `lavalink_config` does not replace an existing client or
    /// re-establish the connection in place; applying a new configuration to an
    /// already-connected client currently requires restarting the bot.
    pub lavalink: Arc<RwLock<Option<LavalinkClient>>>,
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
        Self {
            guild_configs: dashmap::DashMap::new(),
            cache: Arc::new(serenity::Cache::default()),
            lavalink_config: Arc::new(RwLock::new(LavalinkConfig::from_env())),
            lavalink: Arc::new(RwLock::new(None)),
            tracked_messages: Arc::new(dashmap::DashMap::new()),
            started_at: Utc::now(),
        }
    }

    /// Load data from YAML file
    ///
    /// This method loads guild configurations and the Lavalink configuration
    /// from the config directory. If the files don't exist, it returns a new
    /// empty Data instance with default settings.
    pub async fn load() -> Self {
        const CONFIG_FILE: &str = "config/bot_config.yaml";
        const LAVALINK_FILE: &str = "config/lavalink.yaml";

        // Create a new empty Data instance
        let data = Self::new();

        // Check if the guild config file exists
        if let Ok(file_content) = tokio::fs::read_to_string(CONFIG_FILE).await {
            // Try to deserialize the file content
            if let Ok(configs) = serde_yaml::from_str::<Vec<GuildConfig>>(&file_content) {
                // Add each guild config to the map
                for config in configs {
                    let guild_id = serenity::GuildId::new(config.guild_id);
                    data.guild_configs.insert(guild_id, config);
                }
            }
        }

        // Load Lavalink configuration if present
        match tokio::fs::read_to_string(LAVALINK_FILE).await {
            Ok(file_content) => match serde_yaml::from_str::<LavalinkConfig>(&file_content) {
                Ok(config) => {
                    *data.lavalink_config.write().await = config;
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

        data
    }

    /// Save data to YAML file
    ///
    /// This method saves all guild configurations and the current Lavalink
    /// configuration to disk. It creates the config directory if it doesn't
    /// exist.
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
        const LAVALINK_FILE: &str = "config/lavalink.yaml";

        // Create the config directory if it doesn't exist
        if !std::path::Path::new(CONFIG_DIR).exists() {
            tokio::fs::create_dir_all(CONFIG_DIR).await?;
        }

        // Collect all guild configs into a Vec for serialization
        let configs: Vec<GuildConfig> = self.guild_configs
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        // Serialize the configs to YAML
        let yaml = serde_yaml::to_string(&configs)?;

        // Write the YAML to the config file
        tokio::fs::write(CONFIG_FILE, yaml).await?;

        // Persist the Lavalink configuration
        let lavalink_yaml = serde_yaml::to_string(&*self.lavalink_config.read().await)?;
        tokio::fs::write(LAVALINK_FILE, lavalink_yaml).await?;

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
        assert!(data.lavalink.read().await.is_none());
    }

    #[test]
    fn test_guild_config_default() {
        let config = GuildConfig::default();
        assert_eq!(config.guild_id, 0);
        assert!(config.music_channel_id.is_none());
    }

    #[test]
    fn test_lavalink_config_default() {
        let config = LavalinkConfig::default();
        assert_eq!(config.hostname, "localhost:2333");
        assert!(!config.is_ssl);
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

        // Test serialization
        let serialized = serde_yaml::to_string(&config).expect("Failed to serialize");
        assert!(serialized.contains("guild_id: 12345"));
        assert!(serialized.contains("music_channel_id: 67890"));

        // Test deserialization
        let deserialized: GuildConfig = serde_yaml::from_str(&serialized).expect("Failed to deserialize");
        assert_eq!(deserialized.guild_id, 12345);
        assert_eq!(deserialized.music_channel_id, Some(67890));
    }

    #[test]
    fn test_lavalink_config_serialization() {
        let config = LavalinkConfig {
            hostname: "example.com:2333".to_string(),
            password: "secret".to_string(),
            is_ssl: true,
        };

        let yaml = serde_yaml::to_string(&config).expect("serialize");
        let back: LavalinkConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(back.hostname, "example.com:2333");
        assert_eq!(back.password, "secret");
        assert!(back.is_ssl);
    }
}
