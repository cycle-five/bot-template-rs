//! Native [`MusicBackend`]: decodes and plays audio in-process via songbird's
//! driver.
//!
//! # Current state
//!
//! Resolves tracks through [`songbird::input::YoutubeDl`], which shells out
//! to the `yt-dlp` binary (must be on PATH). Track metadata (title, author,
//! duration, source URL) is pulled up-front via `aux_metadata` so the user
//! sees what was queued before playback starts.
//!
//! # TODO: pure-Rust extraction
//!
//! Port [cracktunes](https://github.com/cycle-five/cracktunes)' `rusty_ytdl`-
//! based `Compose` implementation so the common path doesn't need yt-dlp on
//! disk. Keep yt-dlp as a fallback for sites rusty_ytdl can't handle.
//!
//! # Queue metadata sync
//!
//! `now_playing`/`queue_snapshot` are kept aligned with songbird's own queue
//! via a per-track `TrackEvent::End` handler that pops our parallel metadata
//! list when the driver advances. `skip` and `stop` rely on this event too
//! rather than mutating state themselves, so there's a single source of
//! advancement to reason about.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, UserId};
use songbird::Songbird;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{HttpRequest, Input, YoutubeDl};
use tokio::sync::Mutex;
use tracing::debug;

use crate::Error;
use crate::music_backend::{BackendKind, MusicBackend, PlayResult, Track};

#[derive(Default)]
struct GuildMeta {
    /// Parallel metadata for queued tracks (positions match songbird's queue
    /// order, minus the current track). `now_playing` holds the head.
    queue: Vec<Track>,
    now_playing: Option<Track>,
}

/// Fires when songbird ends a track (natural end, skip, or stop). Advances
/// our parallel metadata queue: pop the front into `now_playing`, or clear
/// if the queue is empty.
struct AdvanceOnEnd {
    meta: Arc<DashMap<GuildId, Arc<Mutex<GuildMeta>>>>,
    guild: GuildId,
}

#[async_trait]
impl EventHandler for AdvanceOnEnd {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        if let Some(arc) = self.meta.get(&self.guild) {
            let mut m = arc.value().lock().await;
            if m.queue.is_empty() {
                m.now_playing = None;
            } else {
                m.now_playing = Some(m.queue.remove(0));
            }
        }
        None
    }
}

/// Native backend: native playback via songbird + yt-dlp for resolution.
pub struct NativeBackend {
    http: reqwest::Client,
    songbird: Arc<Songbird>,
    meta: Arc<DashMap<GuildId, Arc<Mutex<GuildMeta>>>>,
}

impl NativeBackend {
    #[must_use]
    pub fn new(songbird: Arc<Songbird>) -> Self {
        Self {
            http: reqwest::Client::new(),
            songbird,
            meta: Arc::new(DashMap::new()),
        }
    }

    fn guild_meta(&self, guild: GuildId) -> Arc<Mutex<GuildMeta>> {
        self.meta
            .entry(guild)
            .or_insert_with(|| Arc::new(Mutex::new(GuildMeta::default())))
            .clone()
    }
}

fn title_from_url(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    no_query
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(url)
        .to_string()
}

async fn track_from_youtube_dl(
    src: &mut YoutubeDl<'_>,
    fallback_title: &str,
    requester: UserId,
) -> Track {
    use songbird::input::Compose;
    let meta: Option<songbird::input::AuxMetadata> = src.aux_metadata().await.ok();
    Track {
        title: meta
            .as_ref()
            .and_then(|m| m.title.clone())
            .unwrap_or_else(|| fallback_title.to_string()),
        author: meta
            .as_ref()
            .and_then(|m| m.artist.clone())
            .unwrap_or_else(|| "Unknown".into()),
        uri: meta.as_ref().and_then(|m| m.source_url.clone()),
        duration_ms: meta
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d: std::time::Duration| d.as_millis() as u64),
        requester: Some(requester.get()),
    }
}

#[async_trait]
impl MusicBackend for NativeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Native
    }

    async fn ensure_joined(
        &self,
        _ctx: &serenity::Context,
        guild: GuildId,
        channel: ChannelId,
    ) -> Result<bool, Error> {
        if self.songbird.get(guild).is_some() {
            return Ok(false);
        }
        self.songbird.join(guild, channel).await?;
        Ok(true)
    }

    async fn leave(
        &self,
        _ctx: &serenity::Context,
        guild: GuildId,
    ) -> Result<(), Error> {
        if self.songbird.get(guild).is_some() {
            self.songbird.remove(guild).await?;
        }
        self.meta.remove(&guild);
        Ok(())
    }

    async fn play(
        &self,
        guild: GuildId,
        query: &str,
        requester: UserId,
    ) -> Result<PlayResult, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        

        let mut src = if query.starts_with("http") {
            YoutubeDl::new(self.http.clone(), query.to_string())
        } else {
            YoutubeDl::new_search(self.http.clone(), query.to_string())
        };
        let track = track_from_youtube_dl(&mut src, query, requester).await;

        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;

        let mut handler = call.lock().await;
        let track_handle = handler.enqueue_input(Input::from(src)).await;
        let queue_len = handler.queue().current_queue().len();
        drop(handler);

        // Attach the end-of-track advancer so our parallel metadata follows
        // songbird's own queue as it progresses.
        if let Err(e) = track_handle.add_event(
            Event::Track(TrackEvent::End),
            AdvanceOnEnd {
                meta: self.meta.clone(),
                guild,
            },
        ) {
            debug!(
                target: "bot_template_rs::music",
                error = ?e,
                "failed to attach track-end handler; metadata may lag"
            );
        }

        // songbird auto-starts playback when enqueueing into an empty queue.
        if queue_len == 1 {
            meta.now_playing = Some(track.clone());
        } else {
            meta.queue.push(track.clone());
        }

        Ok(PlayResult::Added(track))
    }

    async fn play_url(
        &self,
        guild: GuildId,
        url: &str,
        requester: UserId,
    ) -> Result<PlayResult, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        

        // Raw HTTP fetch — no yt-dlp, no metadata probe. Synthesize a
        // minimal Track from the URL itself.
        let src = HttpRequest::new(self.http.clone(), url.to_string());
        let track = Track {
            title: title_from_url(url),
            author: "URL".into(),
            uri: Some(url.to_string()),
            duration_ms: None,
            requester: Some(requester.get()),
        };

        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;

        let mut handler = call.lock().await;
        let track_handle = handler.enqueue_input(Input::from(src)).await;
        let queue_len = handler.queue().current_queue().len();
        drop(handler);

        if let Err(e) = track_handle.add_event(
            Event::Track(TrackEvent::End),
            AdvanceOnEnd {
                meta: self.meta.clone(),
                guild,
            },
        ) {
            debug!(
                target: "bot_template_rs::music",
                error = ?e,
                "failed to attach track-end handler for url; metadata may lag"
            );
        }

        if queue_len == 1 {
            meta.now_playing = Some(track.clone());
        } else {
            meta.queue.push(track.clone());
        }

        Ok(PlayResult::Added(track))
    }

    async fn skip(&self, guild: GuildId) -> Result<Option<Track>, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        

        // Snapshot the currently-playing track to return; the AdvanceOnEnd
        // handler will update the live state asynchronously.
        let skipped = self.guild_meta(guild).lock().await.now_playing.clone();

        let handler = call.lock().await;
        let _ = handler.queue().skip();
        Ok(skipped)
    }

    async fn stop(&self, guild: GuildId) -> Result<Option<Track>, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        

        let meta_arc = self.guild_meta(guild);
        let stopped = {
            // Clear queued metadata up-front so AdvanceOnEnd sees an empty
            // queue and sets now_playing to None rather than advancing.
            let mut meta = meta_arc.lock().await;
            let was_playing = meta.now_playing.clone();
            meta.queue.clear();
            was_playing
        };

        let handler = call.lock().await;
        handler.queue().stop();
        Ok(stopped)
    }

    async fn pause(&self, guild: GuildId) -> Result<(), Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        
        let handler = call.lock().await;
        handler.queue().pause()?;
        Ok(())
    }

    async fn resume(&self, guild: GuildId) -> Result<(), Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        
        let handler = call.lock().await;
        handler.queue().resume()?;
        Ok(())
    }

    async fn now_playing(&self, guild: GuildId) -> Result<Option<Track>, Error> {
        let meta_arc = self.guild_meta(guild);
        let meta = meta_arc.lock().await;
        Ok(meta.now_playing.clone())
    }

    async fn queue_snapshot(&self, guild: GuildId) -> Result<Vec<Track>, Error> {
        let meta_arc = self.guild_meta(guild);
        let meta = meta_arc.lock().await;
        Ok(meta.queue.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_backend_has_no_guild_state() {
        let b = NativeBackend::new(songbird::Songbird::serenity());
        // No guild state until a guild is touched.
        assert_eq!(b.meta.len(), 0);
    }

    #[tokio::test]
    async fn guild_meta_is_lazily_created() {
        let b = NativeBackend::new(songbird::Songbird::serenity());
        let g = GuildId::new(1);
        let m1 = b.guild_meta(g);
        let m2 = b.guild_meta(g);
        assert!(Arc::ptr_eq(&m1, &m2), "same guild reuses the same Arc");
        assert_eq!(b.meta.len(), 1);
    }

    #[tokio::test]
    async fn queue_snapshot_on_fresh_guild_is_empty() {
        let b = NativeBackend::new(songbird::Songbird::serenity());
        let g = GuildId::new(1);
        assert!(b.queue_snapshot(g).await.unwrap().is_empty());
        assert!(b.now_playing(g).await.unwrap().is_none());
    }

    #[test]
    fn backend_is_dyn_compatible() {
        fn _assert(_: &Arc<dyn MusicBackend>) {}
    }
}
