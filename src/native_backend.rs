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

/// Apply an in-place permutation to `arr`. After the call, the element that
/// was originally at position `perm[i]` ends up at position `i` (pull
/// semantics). Consumes `perm` (mutates internally to track completed
/// positions). Used by `shuffle` to apply the *same* random permutation to
/// the metadata queue and songbird's internal queue so they stay in lock-step.
fn apply_perm<T>(arr: &mut [T], mut perm: Vec<usize>) {
    debug_assert_eq!(arr.len(), perm.len());
    let n = arr.len();
    for i in 0..n {
        if perm[i] == i {
            continue;
        }
        // Walk the cycle that starts at `i`, swapping each link into place
        // until we wrap back to `i`.
        let mut cur = i;
        loop {
            let next = perm[cur];
            if next == i {
                break;
            }
            arr.swap(cur, next);
            perm[cur] = cur;
            cur = next;
        }
        perm[cur] = cur;
    }
}

#[derive(Default)]
struct GuildMeta {
    /// Parallel metadata for queued tracks (positions match songbird's queue
    /// order, minus the current track). `now_playing` holds the head.
    queue: std::collections::VecDeque<Track>,
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
            m.now_playing = m.queue.pop_front();
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
            meta.queue.push_back(track.clone());
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
            meta.queue.push_back(track.clone());
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

        // Native diverges from lavalink here: songbird's `TrackQueue` auto-
        // advances on track end via its built-in `QueueHandler`, so we
        // can't cleanly stop just the current track without the next one
        // jumping in. Drain everything instead — `/stop` ends the session
        // entirely on this backend. Users wanting just "drain the queue,
        // keep the current track" should call `/clear`. Matching lavalink
        // semantics exactly would require either a deeper integration with
        // songbird's `Driver` or a parallel-queue-with-replay design.
        let meta_arc = self.guild_meta(guild);
        let stopped = {
            let mut meta = meta_arc.lock().await;
            let was_playing = meta.now_playing.clone();
            meta.queue.clear();
            was_playing
        };

        let handler = call.lock().await;
        handler.queue().stop();
        Ok(stopped)
    }

    async fn clear(&self, guild: GuildId) -> Result<(), Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;

        // Drop our parallel metadata for upcoming tracks first so a
        // racing AdvanceOnEnd doesn't promote one of them into
        // `now_playing` after we've drained songbird's queue.
        self.guild_meta(guild).lock().await.queue.clear();

        // Pull every queued track *behind* the current one out of
        // songbird's internal queue and stop each so its driver-side
        // resources are released (per modify_queue's safety note).
        let handler = call.lock().await;
        let drained: Vec<_> = handler.queue().modify_queue(|q| {
            if q.len() <= 1 {
                Vec::new()
            } else {
                q.drain(1..).collect()
            }
        });
        for queued in &drained {
            let _ = queued.handle().stop();
        }
        Ok(())
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
        Ok(meta.queue.iter().cloned().collect())
    }

    async fn shuffle(&self, guild: GuildId) -> Result<(), Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;

        // Same permutation applied to both songbird's internal queue and our
        // parallel metadata, keeping them in lock-step. Index space is the
        // *upcoming* queue — songbird's q[0] is the currently playing track,
        // metadata holds it in `now_playing`, neither moves.
        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        let len = meta.queue.len();
        if len < 2 {
            return Ok(());
        }
        use rand::seq::SliceRandom;
        let mut perm: Vec<usize> = (0..len).collect();
        perm.shuffle(&mut rand::rng());

        apply_perm(meta.queue.make_contiguous(), perm.clone());

        let handler = call.lock().await;
        handler.queue().modify_queue(|q| {
            if q.len() <= 2 {
                return;
            }
            let slice = q.make_contiguous();
            // q[0] is currently playing; shuffle the upcoming portion only.
            // If songbird has diverged from metadata (track ended between
            // locks), truncate the perm to whatever matches.
            let upcoming = &mut slice[1..];
            let n = upcoming.len().min(perm.len());
            apply_perm(&mut upcoming[..n], perm[..n].to_vec());
        });
        Ok(())
    }

    async fn jump(&self, guild: GuildId, index: usize) -> Result<Option<Track>, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;

        if index == 0 {
            return Ok(self.guild_meta(guild).lock().await.now_playing.clone());
        }

        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        if index > meta.queue.len() {
            return Ok(None);
        }
        // Drop everything before the target so the next end-of-track advances
        // to it. AdvanceOnEnd pops front from meta.queue, so to land on the
        // target track we drop `index - 1` items first; calling skip then
        // ends the current track and advances into target.
        for _ in 0..(index.saturating_sub(1)) {
            meta.queue.pop_front();
        }
        let target = meta.queue.front().cloned();

        let handler = call.lock().await;
        handler.queue().modify_queue(|q| {
            // Mirror the metadata drop in songbird's queue.
            if q.len() < 2 {
                return;
            }
            let drop_count = (index.saturating_sub(1)).min(q.len() - 1);
            let drained: Vec<_> = q.drain(1..=drop_count).collect();
            // Stop the dropped tracks per modify_queue's safety contract.
            for queued in drained {
                let _ = queued.stop();
            }
        });
        let _ = handler.queue().skip();
        Ok(target)
    }

    async fn move_track(
        &self,
        guild: GuildId,
        from: usize,
        to: usize,
    ) -> Result<(), Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        if from >= meta.queue.len() || to >= meta.queue.len() || from == to {
            return Ok(());
        }
        if let Some(item) = meta.queue.remove(from) {
            meta.queue.insert(to, item);
        }

        let handler = call.lock().await;
        handler.queue().modify_queue(|q| {
            // Indices in the upcoming queue translate to q[1+i] (q[0] is current).
            let from_q = 1 + from;
            let to_q = 1 + to;
            if from_q >= q.len() || to_q >= q.len() || from_q == to_q {
                return;
            }
            // VecDeque has no direct move; remove and re-insert.
            if let Some(item) = q.remove(from_q) {
                q.insert(to_q, item);
            }
        });
        Ok(())
    }

    async fn remove_at(
        &self,
        guild: GuildId,
        index: usize,
    ) -> Result<Option<Track>, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        let removed_meta = meta.queue.remove(index);

        if removed_meta.is_some() {
            let handler = call.lock().await;
            handler.queue().modify_queue(|q| {
                let q_idx = 1 + index;
                if q_idx < q.len()
                    && let Some(removed) = q.remove(q_idx) {
                    let _ = removed.stop();
                }
            });
        }
        Ok(removed_meta)
    }

    async fn remove_duplicates(&self, guild: GuildId) -> Result<usize, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        let original = meta.queue.len();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut keep: Vec<bool> = Vec::with_capacity(original);
        for t in &meta.queue {
            let key = t
                .uri
                .clone()
                .unwrap_or_else(|| format!("{}\u{1}{}", t.title, t.author));
            keep.push(seen.insert(key));
        }
        // Drop from the back so indices stay valid.
        for i in (0..original).rev() {
            if !keep[i] {
                meta.queue.remove(i);
            }
        }
        let dropped = original - meta.queue.len();
        if dropped > 0 {
            let handler = call.lock().await;
            handler.queue().modify_queue(|q| {
                for i in (0..keep.len()).rev() {
                    if !keep[i] {
                        let q_idx = 1 + i;
                        if q_idx < q.len()
                            && let Some(removed) = q.remove(q_idx) {
                            let _ = removed.stop();
                        }
                    }
                }
            });
        }
        Ok(dropped)
    }

    async fn leave_cleanup(
        &self,
        guild: GuildId,
        present: &[UserId],
    ) -> Result<usize, Error> {
        let call = self.songbird.get(guild).ok_or("not in voice")?;
        let meta_arc = self.guild_meta(guild);
        let mut meta = meta_arc.lock().await;
        let present_set: std::collections::HashSet<u64> =
            present.iter().map(|u| u.get()).collect();
        let original = meta.queue.len();
        let keep: Vec<bool> = meta
            .queue
            .iter()
            .map(|t| t.requester.is_none_or(|id| present_set.contains(&id)))
            .collect();
        for i in (0..original).rev() {
            if !keep[i] {
                meta.queue.remove(i);
            }
        }
        let dropped = original - meta.queue.len();
        if dropped > 0 {
            let handler = call.lock().await;
            handler.queue().modify_queue(|q| {
                for i in (0..keep.len()).rev() {
                    if !keep[i] {
                        let q_idx = 1 + i;
                        if q_idx < q.len()
                            && let Some(removed) = q.remove(q_idx) {
                            let _ = removed.stop();
                        }
                    }
                }
            });
        }
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_perm_is_correct() {
        // Identity perm leaves array untouched.
        let mut a = vec!['a', 'b', 'c'];
        apply_perm(&mut a, vec![0, 1, 2]);
        assert_eq!(a, vec!['a', 'b', 'c']);

        // Reverse perm.
        let mut a = vec!['a', 'b', 'c', 'd'];
        apply_perm(&mut a, vec![3, 2, 1, 0]);
        assert_eq!(a, vec!['d', 'c', 'b', 'a']);

        // Cycle perm: 0→1→2→0.
        let mut a = vec!['a', 'b', 'c'];
        apply_perm(&mut a, vec![1, 2, 0]);
        // Element at perm[i] ends up at position i:
        //   i=0 gets a[1] = 'b'; i=1 gets a[2] = 'c'; i=2 gets a[0] = 'a'.
        assert_eq!(a, vec!['b', 'c', 'a']);
    }

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
