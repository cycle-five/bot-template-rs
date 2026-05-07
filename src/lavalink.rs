//! Lavalink backend: [`MusicBackend`] implementation plus admin commands.
//!
//! The backend owns its configuration and client handle. Music commands go
//! through the [`MusicBackend`] trait; lavalink-specific `/lavalink` admin
//! commands address the concrete [`LavalinkBackend`] stored in
//! [`crate::data::Data::lavalink`].

use std::sync::Arc;

#[cfg(feature = "music-core")]
use async_trait::async_trait;
use lavalink_rs::model::events;
use lavalink_rs::prelude::*;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::UserId;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[cfg(feature = "music-core")]
use crate::music_backend::{BackendKind, MusicBackend, PlayResult, Track};
use crate::reply::{self, Reply};
use crate::{Context, Error};

/// Configuration for connecting to a Lavalink node.
///
/// All fields may be updated at runtime via `/lavalink set` and then
/// persisted to `config/lavalink.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LavalinkConfig {
    /// Hostname including port, e.g. `localhost:2333`.
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
    /// `LAVALINK_SSL` env vars, falling back to defaults for any unset field.
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

/// Lavalink-backed music backend. Owns the config and the (lazily-established)
/// lavalink client handle.
///
/// `lavalink-rs` 0.15 has no client-shutdown API — its reconnect task lives
/// for the life of the process and holds its own `Arc` to the client — so
/// applying config changes requires restarting the bot.
pub struct LavalinkBackend {
    config: Arc<RwLock<LavalinkConfig>>,
    client: Arc<RwLock<Option<LavalinkClient>>>,
    songbird: Arc<songbird::Songbird>,
    #[cfg(feature = "music-core")]
    http: reqwest::Client,
    /// Per-guild state the lavalink-side track-end handler reaches into via
    /// `LavalinkClient::data::<LavalinkSharedState>()`. Lavalink's event
    /// callbacks are `fn` pointers (not closures), so any state they need has
    /// to come through that channel.
    #[cfg(feature = "music-core")]
    state: Arc<LavalinkSharedState>,
}

/// State shared between the bot's `LavalinkBackend` and the lavalink event
/// callbacks. Stored on `LavalinkClient::user_data` and downcasted by the
/// `track_end` handler.
#[cfg(feature = "music-core")]
#[derive(Default)]
pub(crate) struct LavalinkSharedState {
    pub loop_modes: dashmap::DashMap<serenity::GuildId, crate::music_backend::LoopMode>,
    /// Bounded per-guild history of recently-finished tracks. `/previous`
    /// pops the most recent entry. Capped at 50 to avoid unbounded growth.
    pub history: dashmap::DashMap<serenity::GuildId, std::collections::VecDeque<lavalink_rs::model::track::TrackData>>,
}

impl LavalinkBackend {
    #[must_use]
    pub fn new(config: LavalinkConfig, songbird: Arc<songbird::Songbird>) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            client: Arc::new(RwLock::new(None)),
            songbird,
            #[cfg(feature = "music-core")]
            http: reqwest::Client::new(),
            #[cfg(feature = "music-core")]
            state: Arc::new(LavalinkSharedState::default()),
        }
    }

    pub async fn config(&self) -> LavalinkConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, cfg: LavalinkConfig) {
        *self.config.write().await = cfg;
    }

    pub async fn is_connected(&self) -> bool {
        self.client.read().await.is_some()
    }

    /// Count active lavalink nodes attached to the client. `lavalink-rs` has
    /// no direct `len()`, so probe successive indices.
    pub async fn node_count(&self) -> usize {
        let Some(client) = self.client.read().await.clone() else {
            return 0;
        };
        let mut n = 0usize;
        while client.get_node_by_index(n).is_some() {
            n += 1;
        }
        n
    }

    /// Attempt to connect using the current configuration. Errors if a client
    /// already exists (use `/lavalink connect` in-place is idempotent).
    pub async fn connect(&self, user_id: UserId) -> Result<(), String> {
        if self.client.read().await.is_some() {
            return Err("Lavalink is already connected".to_string());
        }
        let config = self.config.read().await.clone();
        info!(
            target: "bot_template_rs::lavalink",
            hostname = %config.hostname,
            is_ssl = config.is_ssl,
            "Connecting to Lavalink node"
        );

        let evs = events::Events {
            track_end: Some(track_end_handler),
            ..Default::default()
        };
        let node = NodeBuilder {
            hostname: config.hostname.clone(),
            is_ssl: config.is_ssl,
            events: evs.clone(),
            password: config.password.clone(),
            user_id: user_id.get().into(),
            session_id: None,
        };

        let client = LavalinkClient::new_with_data(
            evs,
            vec![node],
            NodeDistributionStrategy::round_robin(),
            self.state.clone(),
        )
        .await;

        // Verify reachability — `new` is lazy and will not fail if the node
        // is offline; hit its HTTP version endpoint to be sure.
        if let Some(node) = client.get_node_by_index(0usize)
            && let Err(e) = node.http.version().await {
                warn!(target: "bot_template_rs::lavalink", error = %e, "Lavalink node unreachable");
                return Err(format!("Lavalink node unreachable: {e}"));
            }

        *self.client.write().await = Some(client);
        info!(target: "bot_template_rs::lavalink", "Lavalink connected");
        Ok(())
    }

    #[cfg(feature = "music-core")]
    async fn client(&self) -> Result<LavalinkClient, Error> {
        self.client
            .read()
            .await
            .clone()
            .ok_or_else(|| "Lavalink is not connected".into())
    }

    #[cfg(feature = "music-core")]
    async fn player(
        &self,
        guild: serenity::GuildId,
    ) -> Result<lavalink_rs::player_context::PlayerContext, Error> {
        let client = self.client().await?;
        client
            .get_player_context(guild)
            .ok_or_else(|| "join the bot to a voice channel first".into())
    }

    /// Resolve a `YouTube` video title via the public oEmbed endpoint. No auth,
    /// no API key. Used by the title-search fallback when a direct URL load
    /// fails playabilityStatus checks on the Lavalink node.
    #[cfg(feature = "music-core")]
    async fn resolve_youtube_title(&self, video_id: &str) -> Option<(String, String)> {
        let url = format!(
            "https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={video_id}&format=json"
        );
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        let title = json.get("title")?.as_str()?.to_string();
        let author = json
            .get("author_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Some((title, author))
    }
}

/// Extract the video ID from a `YouTube` URL. Recognizes `youtube.com/watch?v=`,
/// `youtu.be/`, `youtube.com/shorts/`, `youtube.com/embed/`, `youtube.com/v/`,
/// and `music.youtube.com` / `m.youtube.com` variants. Returns `None` for any
/// other host or non-URL input.
#[cfg(feature = "music-core")]
fn youtube_video_id(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path_q) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.trim_start_matches("www.");

    let id = match host {
        "youtu.be" => path_q.split(['?', '&', '#']).next().unwrap_or(""),
        "youtube.com" | "music.youtube.com" | "m.youtube.com" => {
            let (path, query) = path_q.split_once('?').unwrap_or((path_q, ""));
            if path == "watch" {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("v="))
                    .unwrap_or("")
            } else if let Some(rest) = path
                .strip_prefix("shorts/")
                .or_else(|| path.strip_prefix("embed/"))
                .or_else(|| path.strip_prefix("v/"))
            {
                rest.split('/').next().unwrap_or("")
            } else {
                ""
            }
        }
        _ => "",
    };

    (!id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then(|| id.to_string())
}

/// Mirror of the catch-all arm in [`LavalinkBackend::play`]'s match on
/// `loaded.data`: returns `true` for shapes that should trigger the
/// title-search fallback (Lavalink Empty or Error load types).
#[cfg(all(test, feature = "music-core"))]
fn should_fallback_to_search(data: &Option<TrackLoadData>) -> bool {
    matches!(data, None | Some(TrackLoadData::Error(_)))
}

#[cfg(feature = "music-core")]
fn track_from_lavalink(t: &lavalink_rs::model::track::TrackData) -> Track {
    Track {
        title: t.info.title.clone(),
        author: t.info.author.clone(),
        uri: t.info.uri.clone(),
        duration_ms: Some(t.info.length),
        requester: t
            .user_data
            .as_ref()
            .and_then(|u| u.get("requester_id"))
            .and_then(serde_json::Value::as_u64),
    }
}

#[cfg(feature = "music-core")]
#[async_trait]
impl MusicBackend for LavalinkBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Lavalink
    }

    async fn on_ready(
        &self,
        _http: &serenity::Http,
        user_id: UserId,
    ) -> Result<(), Error> {
        // Non-fatal — admins can retry via /lavalink connect.
        if let Err(e) = self.connect(user_id).await {
            warn!(
                target: "bot_template_rs::lavalink",
                error = %e,
                "Lavalink connection failed at startup; use /lavalink connect to retry"
            );
        }
        Ok(())
    }

    async fn ensure_joined(
        &self,
        _ctx: &serenity::Context,
        guild: serenity::GuildId,
        channel: serenity::ChannelId,
    ) -> Result<bool, Error> {
        let lava = self.client().await?;

        if lava.get_player_context(guild).is_some() {
            return Ok(false);
        }

        let (sb_info, _) = self.songbird.join_gateway(guild, channel).await?;
        // lavalink-rs's `songbird` feature is disabled to avoid a second
        // songbird version in the tree, so we convert manually.
        let lava_info = lavalink_rs::model::player::ConnectionInfo {
            endpoint: sb_info.endpoint,
            token: sb_info.token,
            session_id: sb_info.session_id,
            channel_id: Some(lavalink_rs::model::ChannelId(sb_info.channel_id.get())),
        };
        lava.create_player_context(guild, lava_info).await?;
        Ok(true)
    }

    async fn leave(
        &self,
        _ctx: &serenity::Context,
        guild: serenity::GuildId,
    ) -> Result<(), Error> {
        if let Ok(lava) = self.client().await {
            let _ = lava.delete_player(guild).await;
        }
        if self.songbird.get(guild).is_some() {
            self.songbird.remove(guild).await?;
        }
        Ok(())
    }

    async fn play(
        &self,
        guild: serenity::GuildId,
        query: &str,
        requester: UserId,
    ) -> Result<PlayResult, Error> {
        let client = self.client().await?;
        let player = self.player(guild).await?;

        let resolved_query = if query.starts_with("http") {
            query.to_string()
        } else {
            SearchEngines::YouTube.to_query(query)?
        };

        let loaded = client.load_tracks(guild, &resolved_query).await?;
        let mut resolved_via_search = false;

        let (mut lav_tracks, summary) = match loaded.data {
            Some(TrackLoadData::Track(t)) => {
                let display = track_from_lavalink(&t);
                (vec![TrackInQueue::from(t)], PlayResult::Added(display))
            }
            Some(TrackLoadData::Search(list)) => {
                let Some(first) = list.first() else {
                    return Ok(PlayResult::NoMatch);
                };
                let display = track_from_lavalink(first);
                (
                    vec![TrackInQueue::from(first.clone())],
                    PlayResult::Added(display),
                )
            }
            Some(TrackLoadData::Playlist(pl)) => {
                let name = pl.info.name.clone();
                let Some(first) = pl.tracks.first().map(track_from_lavalink) else {
                    return Ok(PlayResult::NoMatch);
                };
                let count = pl.tracks.len();
                let lav: Vec<TrackInQueue> =
                    pl.tracks.into_iter().map(TrackInQueue::from).collect();
                (lav, PlayResult::Playlist { name, count, first })
            }
            _ => {
                if let Some(TrackLoadData::Error(ref e)) = loaded.data {
                    tracing::debug!(
                        target: "bot_template_rs::lavalink",
                        severity = %e.severity,
                        cause = %e.cause,
                        "Lavalink load failed; attempting title-search fallback"
                    );
                }
                let Some(video_id) = youtube_video_id(&resolved_query) else {
                    return Ok(PlayResult::NoMatch);
                };
                let Some((title, author)) = self.resolve_youtube_title(&video_id).await else {
                    tracing::warn!(
                        target: "bot_template_rs::lavalink",
                        video_id = %video_id,
                        "oEmbed title resolution failed"
                    );
                    return Ok(PlayResult::NoMatch);
                };
                let search_query = format!("ytsearch:{title} {author}");
                tracing::info!(
                    target: "bot_template_rs::lavalink",
                    video_id = %video_id,
                    query = %search_query,
                    "Title-search fallback firing"
                );
                let fallback = client.load_tracks(guild, &search_query).await?;
                let first = match fallback.data {
                    Some(TrackLoadData::Search(list)) => list.into_iter().next(),
                    Some(TrackLoadData::Track(t)) => Some(t),
                    _ => None,
                };
                let Some(first) = first else {
                    return Ok(PlayResult::NoMatch);
                };
                resolved_via_search = true;
                let display = track_from_lavalink(&first);
                (vec![TrackInQueue::from(first)], PlayResult::Added(display))
            }
        };

        for i in &mut lav_tracks {
            let mut ud = serde_json::json!({"requester_id": requester.get()});
            if resolved_via_search {
                ud["resolved_via"] = serde_json::Value::String("title_search".into());
            }
            i.track.user_data = Some(ud);
        }

        let queue = player.get_queue();
        queue.append(lav_tracks.into())?;

        // Start playback if idle.
        if let Ok(player_data) = player.get_player().await
            && player_data.track.is_none()
                && queue.get_track(0).await.is_ok_and(|x| x.is_some())
            {
                player.skip()?;
            }

        Ok(summary)
    }

    async fn play_url(
        &self,
        guild: serenity::GuildId,
        url: &str,
        requester: UserId,
    ) -> Result<PlayResult, Error> {
        let client = self.client().await?;
        let player = self.player(guild).await?;

        let loaded = client.load_tracks(guild, url).await?;
        let mut resolved_via_search = false;

        let (mut lav_tracks, summary) = match loaded.data {
            Some(TrackLoadData::Track(t)) => {
                let display = track_from_lavalink(&t);
                (vec![TrackInQueue::from(t)], PlayResult::Added(display))
            }
            Some(TrackLoadData::Search(list)) => {
                let Some(first) = list.first() else {
                    return Ok(PlayResult::NoMatch);
                };
                let display = track_from_lavalink(first);
                (
                    vec![TrackInQueue::from(first.clone())],
                    PlayResult::Added(display),
                )
            }
            Some(TrackLoadData::Playlist(pl)) => {
                let name = pl.info.name.clone();
                let Some(first) = pl.tracks.first().map(track_from_lavalink) else {
                    return Ok(PlayResult::NoMatch);
                };
                let count = pl.tracks.len();
                let lav: Vec<TrackInQueue> =
                    pl.tracks.into_iter().map(TrackInQueue::from).collect();
                (lav, PlayResult::Playlist { name, count, first })
            }
            _ => {
                if let Some(TrackLoadData::Error(ref e)) = loaded.data {
                    tracing::debug!(
                        target: "bot_template_rs::lavalink",
                        severity = %e.severity,
                        cause = %e.cause,
                        "Lavalink load failed; attempting title-search fallback"
                    );
                }
                let Some(video_id) = youtube_video_id(url) else {
                    return Ok(PlayResult::NoMatch);
                };
                let Some((title, author)) = self.resolve_youtube_title(&video_id).await else {
                    tracing::warn!(
                        target: "bot_template_rs::lavalink",
                        video_id = %video_id,
                        "oEmbed title resolution failed"
                    );
                    return Ok(PlayResult::NoMatch);
                };
                let search_query = format!("ytsearch:{title} {author}");
                tracing::info!(
                    target: "bot_template_rs::lavalink",
                    video_id = %video_id,
                    query = %search_query,
                    "Title-search fallback firing"
                );
                let fallback = client.load_tracks(guild, &search_query).await?;
                let first = match fallback.data {
                    Some(TrackLoadData::Search(list)) => list.into_iter().next(),
                    Some(TrackLoadData::Track(t)) => Some(t),
                    _ => None,
                };
                let Some(first) = first else {
                    return Ok(PlayResult::NoMatch);
                };
                resolved_via_search = true;
                let display = track_from_lavalink(&first);
                (vec![TrackInQueue::from(first)], PlayResult::Added(display))
            }
        };

        for i in &mut lav_tracks {
            let mut ud = serde_json::json!({"requester_id": requester.get()});
            if resolved_via_search {
                ud["resolved_via"] = serde_json::Value::String("title_search".into());
            }
            i.track.user_data = Some(ud);
        }

        let queue = player.get_queue();
        queue.append(lav_tracks.into())?;

        if let Ok(player_data) = player.get_player().await
            && player_data.track.is_none()
                && queue.get_track(0).await.is_ok_and(|x| x.is_some())
            {
                player.skip()?;
            }

        Ok(summary)
    }

    async fn skip(&self, guild: serenity::GuildId) -> Result<Option<Track>, Error> {
        let player = self.player(guild).await?;
        let np = player.get_player().await?.track;
        if let Some(t) = np {
            player.skip()?;
            Ok(Some(track_from_lavalink(&t)))
        } else {
            Ok(None)
        }
    }

    async fn stop(&self, guild: serenity::GuildId) -> Result<Option<Track>, Error> {
        let player = self.player(guild).await?;
        let np = player.get_player().await?.track;
        if let Some(t) = np {
            player.stop_now().await?;
            Ok(Some(track_from_lavalink(&t)))
        } else {
            Ok(None)
        }
    }

    async fn clear(&self, guild: serenity::GuildId) -> Result<(), Error> {
        let player = self.player(guild).await?;
        player.get_queue().clear()?;
        Ok(())
    }

    async fn pause(&self, guild: serenity::GuildId) -> Result<(), Error> {
        self.player(guild).await?.set_pause(true).await?;
        Ok(())
    }

    async fn resume(&self, guild: serenity::GuildId) -> Result<(), Error> {
        self.player(guild).await?.set_pause(false).await?;
        Ok(())
    }

    async fn now_playing(&self, guild: serenity::GuildId) -> Result<Option<Track>, Error> {
        let player = self.player(guild).await?;
        Ok(player.get_player().await?.track.as_ref().map(track_from_lavalink))
    }

    async fn queue_snapshot(&self, guild: serenity::GuildId) -> Result<Vec<Track>, Error> {
        let player = self.player(guild).await?;
        let items = player.get_queue().get_queue().await?;
        Ok(items.iter().map(|t| track_from_lavalink(&t.track)).collect())
    }

    async fn shuffle(&self, guild: serenity::GuildId) -> Result<(), Error> {
        use rand::seq::SliceRandom;
        let player = self.player(guild).await?;
        let queue = player.get_queue();
        let items = queue.get_queue().await?;
        let mut v: Vec<_> = items.into();
        v.shuffle(&mut rand::rng());
        queue.replace(v.into())?;
        Ok(())
    }

    async fn jump(
        &self,
        guild: serenity::GuildId,
        index: usize,
    ) -> Result<Option<Track>, Error> {
        let player = self.player(guild).await?;
        if index == 0 {
            // Already at the head; treat as a no-op skip.
            let np = player.get_player().await?.track;
            return Ok(np.as_ref().map(track_from_lavalink));
        }
        let queue = player.get_queue();
        let items = queue.get_queue().await?;
        if index > items.len() {
            return Ok(None);
        }
        // Drop the first `index` queued tracks so the next .skip() starts on
        // the target.
        let kept: std::collections::VecDeque<_> = items.into_iter().skip(index).collect();
        queue.replace(kept)?;
        let target = player.get_queue().get_track(0).await?.map(|t| track_from_lavalink(&t.track));
        player.skip()?;
        Ok(target)
    }

    async fn move_track(
        &self,
        guild: serenity::GuildId,
        from: usize,
        to: usize,
    ) -> Result<(), Error> {
        let player = self.player(guild).await?;
        let queue = player.get_queue();
        let mut items = queue.get_queue().await?;
        if from >= items.len() || to >= items.len() || from == to {
            return Ok(());
        }
        if let Some(item) = items.remove(from) {
            items.insert(to, item);
            queue.replace(items)?;
        }
        Ok(())
    }

    async fn remove_at(
        &self,
        guild: serenity::GuildId,
        index: usize,
    ) -> Result<Option<Track>, Error> {
        let player = self.player(guild).await?;
        let queue = player.get_queue();
        let items = queue.get_queue().await?;
        let Some(item) = items.get(index).cloned() else {
            return Ok(None);
        };
        queue.remove(index)?;
        Ok(Some(track_from_lavalink(&item.track)))
    }

    async fn remove_duplicates(&self, guild: serenity::GuildId) -> Result<usize, Error> {
        let player = self.player(guild).await?;
        let queue = player.get_queue();
        let items = queue.get_queue().await?;
        let original = items.len();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let kept: std::collections::VecDeque<_> = items
            .into_iter()
            .filter(|item| {
                let key = item
                    .track
                    .info
                    .uri
                    .clone()
                    .unwrap_or_else(|| {
                        format!("{}\u{1}{}", item.track.info.title, item.track.info.author)
                    });
                seen.insert(key)
            })
            .collect();
        let dropped = original - kept.len();
        if dropped > 0 {
            queue.replace(kept)?;
        }
        Ok(dropped)
    }

    async fn leave_cleanup(
        &self,
        guild: serenity::GuildId,
        present: &[UserId],
    ) -> Result<usize, Error> {
        let player = self.player(guild).await?;
        let queue = player.get_queue();
        let items = queue.get_queue().await?;
        let original = items.len();
        let present_set: std::collections::HashSet<u64> =
            present.iter().map(|u| u.get()).collect();
        let kept: std::collections::VecDeque<_> = items
            .into_iter()
            .filter(|item| {
                let requester = item
                    .track
                    .user_data
                    .as_ref()
                    .and_then(|v| v.get("requester_id"))
                    .and_then(serde_json::Value::as_u64);
                match requester {
                    None => true, // unknown requester: keep
                    Some(id) => present_set.contains(&id),
                }
            })
            .collect();
        let dropped = original - kept.len();
        if dropped > 0 {
            queue.replace(kept)?;
        }
        Ok(dropped)
    }

    async fn seek(
        &self,
        guild: serenity::GuildId,
        position: std::time::Duration,
    ) -> Result<(), Error> {
        let player = self.player(guild).await?;
        player.set_position(position).await?;
        Ok(())
    }

    async fn set_volume(&self, guild: serenity::GuildId, percent: u16) -> Result<(), Error> {
        // Cap at 200 — anything higher is screech territory and we want
        // uniform behavior across backends. Lavalink itself accepts up to
        // 1000.
        let v = percent.min(200);
        let player = self.player(guild).await?;
        player.set_volume(v).await?;
        Ok(())
    }

    async fn set_loop(
        &self,
        guild: serenity::GuildId,
        mode: crate::music_backend::LoopMode,
    ) -> Result<(), Error> {
        if matches!(mode, crate::music_backend::LoopMode::Off) {
            self.state.loop_modes.remove(&guild);
        } else {
            self.state.loop_modes.insert(guild, mode);
        }
        Ok(())
    }

    async fn get_loop(
        &self,
        guild: serenity::GuildId,
    ) -> Result<crate::music_backend::LoopMode, Error> {
        Ok(self
            .state
            .loop_modes
            .get(&guild)
            .map(|r| *r)
            .unwrap_or_default())
    }

    async fn previous(
        &self,
        guild: serenity::GuildId,
    ) -> Result<Option<Track>, Error> {
        let Some(mut entry) = self.state.history.get_mut(&guild) else {
            return Ok(None);
        };
        let Some(prev) = entry.pop_back() else {
            return Ok(None);
        };
        let display = track_from_lavalink(&prev);
        drop(entry);
        let player = self.player(guild).await?;
        player
            .get_queue()
            .push_to_front(TrackInQueue::from(prev))?;
        // If something's playing, skip to the previous; if idle, kick playback.
        if player.get_player().await?.track.is_some() {
            player.skip()?;
        } else {
            player.skip()?;
        }
        Ok(Some(display))
    }
}

/// Lavalink-side track-end callback. Reaches into `LavalinkSharedState`
/// (stored on the client's `user_data`) for per-guild loop and history
/// state. Lavalink event hooks are `fn` pointers, not closures, so we
/// can't capture the backend Arc directly.
fn track_end_handler(
    client: lavalink_rs::client::LavalinkClient,
    _session_id: String,
    event: &lavalink_rs::model::events::TrackEnd,
) -> lavalink_rs::model::BoxFuture<'static, ()> {
    use crate::music_backend::LoopMode;
    // lavalink-rs has its own GuildId type. Convert to serenity's so the
    // DashMap key type matches what /loop and /previous use to read it.
    let guild = serenity::GuildId::new(event.guild_id.0);
    let lav_guild = event.guild_id;
    let track = event.track.clone();
    Box::pin(async move {
        let Ok(state) = client.data::<LavalinkSharedState>() else {
            return;
        };
        // Always record into history so /previous has something to pop. Cap
        // at 50 entries per guild to bound memory.
        state
            .history
            .entry(guild)
            .or_default()
            .push_back(track.clone());
        if let Some(mut h) = state.history.get_mut(&guild)
            && h.len() > 50
        {
            h.pop_front();
        }
        // Apply loop mode.
        let mode = state
            .loop_modes
            .get(&guild)
            .map(|r| *r)
            .unwrap_or_default();
        let Some(player) = client.get_player_context(lav_guild) else {
            return;
        };
        match mode {
            LoopMode::Off => {}
            LoopMode::Track => {
                let _ = player
                    .get_queue()
                    .push_to_front(lavalink_rs::player_context::TrackInQueue::from(track));
            }
            LoopMode::Queue => {
                let _ = player
                    .get_queue()
                    .push_to_back(lavalink_rs::player_context::TrackInQueue::from(track));
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Admin commands
// ---------------------------------------------------------------------------

/// Admin-only parent command for configuring and connecting Lavalink at
/// runtime.
#[allow(clippy::unused_async)]
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

/// Show the current Lavalink configuration (password redacted).
#[poise::command(slash_command, prefix_command, rename = "show")]
pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    let backend = &ctx.data().lavalink;
    let cfg = backend.config().await;
    let connected = backend.is_connected().await;
    let redacted = if cfg.password.is_empty() { "(empty)" } else { "***" };
    let body = format!(
        "**Lavalink config**\n\
         hostname: `{}`\n\
         ssl: `{}`\n\
         password: `{}`\n\
         connected: `{}`",
        cfg.hostname, cfg.is_ssl, redacted, connected
    );
    reply::send(
        &ctx,
        Reply::new()
            .content(body)
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Update one or more Lavalink settings. Omitted fields are left unchanged.
///
/// After updating, persists to disk. Use `/lavalink connect` to (re)connect;
/// applying to a live client requires a full bot restart.
#[poise::command(slash_command, prefix_command, rename = "set")]
pub async fn set(
    ctx: Context<'_>,
    #[description = "Hostname:port of the Lavalink node"] hostname: Option<String>,
    #[description = "Password for the Lavalink node"] password: Option<String>,
    #[description = "Whether to use SSL/TLS"] is_ssl: Option<bool>,
) -> Result<(), Error> {
    let backend = &ctx.data().lavalink;
    let mut cfg = backend.config().await;
    if let Some(h) = hostname {
        cfg.hostname = h;
    }
    if let Some(p) = password {
        cfg.password = p;
    }
    if let Some(s) = is_ssl {
        cfg.is_ssl = s;
    }
    backend.set_config(cfg).await;

    if let Err(e) = ctx.data().save().await {
        reply::send(
            &ctx,
            Reply::new()
                .content(format!("Updated in-memory but failed to persist config: {e}"))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    reply::send(
        &ctx,
        Reply::new()
            .content("Lavalink config updated. Restart the bot to apply.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Connect to Lavalink using the current configuration.
///
/// Idempotent: no-op when already connected, since `lavalink-rs` 0.15 has
/// no client shutdown API and rebuilding in place leaks a zombie reconnect
/// task. Apply config changes by restarting the bot.
#[poise::command(slash_command, prefix_command, rename = "connect")]
pub async fn connect_cmd(ctx: Context<'_>) -> Result<(), Error> {
    let backend = &ctx.data().lavalink;
    if backend.is_connected().await {
        reply::send(
            &ctx,
            Reply::new()
                .content(
                    "Lavalink is already connected. Restart the bot to apply configuration changes.",
                )
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let user_id = ctx.serenity_context().cache.current_user().id;
    let body = match backend.connect(user_id).await {
        Ok(()) => "Connected to Lavalink.".to_string(),
        Err(e) => format!("Failed to connect: {e}"),
    };
    reply::send(
        &ctx,
        Reply::new()
            .content(body)
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
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
        let sub_names: Vec<&str> = cmd.subcommands.iter().map(|c| &*c.name).collect();
        assert!(sub_names.contains(&"show"));
        assert!(sub_names.contains(&"set"));
        assert!(sub_names.contains(&"connect"));
    }

    #[test]
    fn lavalink_config_default() {
        let config = LavalinkConfig::default();
        assert_eq!(config.hostname, "localhost:2333");
        assert!(!config.is_ssl);
    }

    #[test]
    fn lavalink_config_serialization() {
        let config = LavalinkConfig {
            hostname: "example.com:2333".to_string(),
            password: "secret".to_string(),
            is_ssl: true,
        };
        let yaml = serde_yaml::to_string(&config).expect("serialize");
        let back: LavalinkConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(back.hostname, "example.com:2333");
        assert!(back.is_ssl);
    }

    #[tokio::test]
    async fn backend_starts_disconnected() {
        let songbird = songbird::Songbird::serenity();
        let b = LavalinkBackend::new(LavalinkConfig::default(), songbird);
        assert!(!b.is_connected().await);
        assert_eq!(b.node_count().await, 0);
    }

    #[tokio::test]
    async fn backend_set_config_round_trips() {
        let songbird = songbird::Songbird::serenity();
        let b = LavalinkBackend::new(LavalinkConfig::default(), songbird);
        let new_cfg = LavalinkConfig {
            hostname: "example.net:9999".into(),
            password: "p".into(),
            is_ssl: true,
        };
        b.set_config(new_cfg.clone()).await;
        let got = b.config().await;
        assert_eq!(got.hostname, new_cfg.hostname);
        assert!(got.is_ssl);
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_recognizes_standard_watch() {
        assert_eq!(
            youtube_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            youtube_video_id("https://youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_recognizes_short_form() {
        assert_eq!(
            youtube_video_id("https://youtu.be/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            youtube_video_id("https://youtu.be/dQw4w9WgXcQ?si=abc123"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_recognizes_shorts_and_embed() {
        assert_eq!(
            youtube_video_id("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            youtube_video_id("https://www.youtube.com/embed/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            youtube_video_id("https://www.youtube.com/v/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_recognizes_music_and_mobile() {
        assert_eq!(
            youtube_video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=foo"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            youtube_video_id("https://m.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_strips_extra_query_params() {
        assert_eq!(
            youtube_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10s"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn youtube_video_id_rejects_non_youtube() {
        assert!(youtube_video_id("https://soundcloud.com/foo/bar").is_none());
        assert!(youtube_video_id("https://example.com/watch?v=abc").is_none());
        assert!(youtube_video_id("https://www.youtube.com/").is_none());
        assert!(youtube_video_id("https://www.youtube.com/feed/trending").is_none());
        assert!(youtube_video_id("not a url").is_none());
        assert!(youtube_video_id("ytsearch:rick astley").is_none());
        assert!(youtube_video_id("").is_none());
    }

    #[cfg(feature = "music-core")]
    #[test]
    fn should_fallback_to_search_predicate() {
        use lavalink_rs::model::track::{TrackData, TrackError};

        assert!(should_fallback_to_search(&None));
        assert!(should_fallback_to_search(&Some(TrackLoadData::Error(
            TrackError {
                message: "fail".into(),
                severity: "common".into(),
                cause: "playabilityStatus".into(),
            }
        ))));
        assert!(!should_fallback_to_search(&Some(TrackLoadData::Track(
            TrackData::default()
        ))));
        assert!(!should_fallback_to_search(&Some(TrackLoadData::Search(
            vec![]
        ))));
    }
}
