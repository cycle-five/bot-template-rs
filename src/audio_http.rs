//! In-memory audio store + axum HTTP server for bot-generated audio.
//!
//! Some mixers (notably the lavalink node, when running remotely) can only
//! consume audio by URL. Features that produce audio in-process — TTS today,
//! potentially sound effects or STT-replay tomorrow — put their bytes here,
//! get back a URL, and hand that URL to the music backend.
//!
//! Entries live in memory with a short TTL (default 60s) — just long enough
//! for the lavalink node to fetch them — and are swept on a background
//! interval. The bind address is local; put a tunnel (cloudflared, ngrok)
//! or reverse proxy in front to make the URL publicly reachable, and point
//! `BOT_PUBLIC_URL` at the public side.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use dashmap::DashMap;
use tracing::info;

use crate::Error;

#[derive(Clone)]
pub struct AudioEntry {
    pub bytes: Vec<u8>,
    pub content_type: String,
    expires_at: Instant,
}

pub struct AudioStore {
    entries: DashMap<String, AudioEntry>,
    counter: AtomicU64,
    ttl: Duration,
    public_url: Option<String>,
}

impl AudioStore {
    #[must_use]
    pub fn new(public_url: Option<String>, ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            counter: AtomicU64::new(0),
            ttl,
            public_url,
        }
    }

    #[must_use]
    pub fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    /// Store audio bytes and return (id, public_url). The URL is `None` when
    /// `BOT_PUBLIC_URL` isn't configured; callers must fall back to a
    /// bytes-based path in that case.
    pub fn put(&self, bytes: Vec<u8>, content_type: impl Into<String>) -> (String, Option<String>) {
        let id = format!("{:x}", self.counter.fetch_add(1, Ordering::SeqCst));
        self.entries.insert(
            id.clone(),
            AudioEntry {
                bytes,
                content_type: content_type.into(),
                expires_at: Instant::now() + self.ttl,
            },
        );
        let url = self
            .public_url
            .as_ref()
            .map(|base| format!("{}/audio/{}", base.trim_end_matches('/'), id));
        (id, url)
    }

    /// Fetch an entry if it exists and hasn't passed its TTL. Expired
    /// entries are dropped on the spot rather than waiting for the next
    /// sweep — otherwise a request that lands between expiry and the next
    /// sweep tick (up to 30s window) would still get served stale bytes.
    pub fn get(&self, id: &str) -> Option<AudioEntry> {
        let entry = self.entries.get(id)?.value().clone();
        if entry.expires_at <= Instant::now() {
            self.entries.remove(id);
            return None;
        }
        Some(entry)
    }

    pub fn sweep(&self) {
        let now = Instant::now();
        self.entries.retain(|_, e| e.expires_at > now);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

async fn handler_audio(
    State(store): State<Arc<AudioStore>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match store.get(&id) {
        Some(entry) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, entry.content_type)],
            entry.bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "audio not found").into_response(),
    }
}

async fn handler_health() -> &'static str {
    "ok"
}

/// Start the audio HTTP server on `bind_addr` and the background sweeper.
/// Runs until the listener errors.
pub async fn serve(bind_addr: &str, store: Arc<AudioStore>) -> Result<(), Error> {
    let app = Router::new()
        .route("/health", get(handler_health))
        .route("/audio/{id}", get(handler_audio))
        .with_state(store.clone());

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!(
        target: "bot_template_rs::audio_http",
        bind = %bind_addr,
        public_url = ?store.public_url(),
        "audio HTTP server listening"
    );

    let sweep_store = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            sweep_store.sweep();
        }
    });

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_without_public_url_returns_none_url() {
        let store = AudioStore::new(None, Duration::from_secs(60));
        let (_id, url) = store.put(vec![1, 2, 3], "audio/wav");
        assert!(url.is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn put_with_public_url_builds_full_url() {
        let store = AudioStore::new(
            Some("https://bot.example.com".into()),
            Duration::from_secs(60),
        );
        let (id, url) = store.put(vec![1, 2, 3], "audio/wav");
        assert_eq!(
            url.as_deref(),
            Some(format!("https://bot.example.com/audio/{id}").as_str())
        );
    }

    #[test]
    fn public_url_trailing_slash_is_normalized() {
        let store = AudioStore::new(
            Some("https://bot.example.com/".into()),
            Duration::from_secs(60),
        );
        let (id, url) = store.put(vec![], "audio/wav");
        assert_eq!(
            url.as_deref(),
            Some(format!("https://bot.example.com/audio/{id}").as_str())
        );
    }

    #[test]
    fn get_returns_stored_bytes_and_type() {
        let store = AudioStore::new(None, Duration::from_secs(60));
        let (id, _) = store.put(vec![9, 8, 7], "audio/mpeg");
        let got = store.get(&id).expect("entry present");
        assert_eq!(got.bytes, vec![9, 8, 7]);
        assert_eq!(got.content_type, "audio/mpeg");
    }

    #[test]
    fn sweep_removes_expired_entries() {
        let store = AudioStore::new(None, Duration::from_millis(0));
        store.put(vec![1], "audio/wav");
        std::thread::sleep(Duration::from_millis(5));
        store.sweep();
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn get_treats_expired_entries_as_missing() {
        let store = AudioStore::new(None, Duration::from_millis(0));
        let (id, _) = store.put(vec![1, 2, 3], "audio/wav");
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.get(&id).is_none(), "expired entry should not be served");
        // Bonus: opportunistic eviction
        assert_eq!(store.len(), 0, "expired entry should be evicted on get");
    }
}
