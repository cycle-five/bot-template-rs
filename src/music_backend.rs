//! Backend-agnostic interface for music playback.
//!
//! Music commands call methods on `Arc<dyn MusicBackend>`, stored in
//! [`crate::data::Data`]. Each backend (Lavalink, native songbird, etc.)
//! implements this trait and hides its own state and protocol details.

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{ChannelId, GuildId, UserId};

use crate::Error;

/// A track resolved from a URL or search query.
///
/// Intentionally backend-agnostic: each backend maps its native track
/// representation to this struct. `Serialize`/`Deserialize` allows playlist
/// storage to round-trip tracks. On restore, a backend re-resolves the track
/// from `uri` (or re-searches on `title`/`author`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub author: String,
    pub uri: Option<String>,
    pub duration_ms: Option<u64>,
    /// Discord user who queued the track, if known.
    pub requester: Option<u64>,
}

/// Summary of what `MusicBackend::play` produced.
#[derive(Debug, Clone)]
pub enum PlayResult {
    /// A single track was added to the queue.
    Added(Track),
    /// A playlist was added.
    Playlist {
        name: String,
        count: usize,
        first: Track,
    },
    /// Query matched nothing.
    NoMatch,
}

/// Trait every music backend implements.
///
/// Lifecycle:
/// 1. Constructed during [`crate::data::Data::load`].
/// 2. [`MusicBackend::on_ready`] fires once when the gateway reports ready.
/// 3. Command methods fire per user invocation.
#[async_trait]
pub trait MusicBackend: Send + Sync + 'static {
    /// Backend-initialization hook. Default no-op. Lavalink overrides to open
    /// its control-plane connection once the bot's user id is known.
    async fn on_ready(
        &self,
        _ctx: &serenity::Context,
        _user_id: UserId,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Ensure the bot is connected to `channel` in `guild` and has a player
    /// set up. Returns `true` if a new connection was established.
    async fn ensure_joined(
        &self,
        ctx: &serenity::Context,
        guild: GuildId,
        channel: ChannelId,
    ) -> Result<bool, Error>;

    /// Leave the voice channel in `guild`.
    async fn leave(&self, ctx: &serenity::Context, guild: GuildId) -> Result<(), Error>;

    /// Resolve `query` (URL or search term) and enqueue matching tracks.
    async fn play(
        &self,
        guild: GuildId,
        query: &str,
        requester: UserId,
    ) -> Result<PlayResult, Error>;

    /// Skip the current track. Returns the skipped track, if any.
    async fn skip(&self, guild: GuildId) -> Result<Option<Track>, Error>;

    /// Stop the current track and clear it. Returns the stopped track.
    async fn stop(&self, guild: GuildId) -> Result<Option<Track>, Error>;

    async fn pause(&self, guild: GuildId) -> Result<(), Error>;
    async fn resume(&self, guild: GuildId) -> Result<(), Error>;

    async fn now_playing(&self, guild: GuildId) -> Result<Option<Track>, Error>;
    async fn queue_snapshot(&self, guild: GuildId) -> Result<Vec<Track>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn track_round_trips_through_yaml() {
        let t = Track {
            title: "Song".into(),
            author: "Artist".into(),
            uri: Some("https://example.com/s".into()),
            duration_ms: Some(12345),
            requester: Some(42),
        };
        let yaml = serde_yaml::to_string(&t).unwrap();
        let back: Track = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn backend_is_dyn_compatible() {
        fn _assert(_: &Arc<dyn MusicBackend>) {}
    }
}
