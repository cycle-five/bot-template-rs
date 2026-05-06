//! Text-to-speech backed by the [`tts-service`][1] HTTP microservice.
//!
//! The bot fetches synthesized audio bytes from the service and plays them
//! through songbird's native driver as a one-shot track. Config lives in
//! `config/tts.yaml` (auto-created from env/defaults on first run) so
//! `/tts set …` changes survive restarts.
//!
//! Pairs equally well with the Lavalink or native music backend — playback
//! goes through songbird's local mixer either way, since Lavalink only
//! handles its own tracks, not arbitrary bytes from outside.
//!
//! [1]: https://github.com/GnomedDev/tts-service

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use songbird::input::Input;
use tokio::sync::RwLock;

use crate::reply::{self, Reply};
use crate::{Context, Error};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Runtime-mutable TTS settings. Mirrors the query parameters the
/// `tts-service` endpoint accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    /// Base URL (scheme + host + optional port) for the tts-service instance.
    pub service_url: String,
    /// Sent in the `Authorization` header when non-empty.
    #[serde(default)]
    pub auth_key: String,
    /// One of `eSpeak`, `gTTS`, `gcloud`, `Polly`.
    pub mode: String,
    /// Default voice tag (e.g. `en`). Overridable per-invocation.
    pub voice: String,
    pub speaking_rate: f32,
    pub max_length: u32,
    /// Preferred audio format the service should return. songbird's symphonia
    /// probe handles whatever gets returned.
    pub preferred_format: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            service_url: "http://localhost:3000".to_string(),
            auth_key: String::new(),
            mode: "eSpeak".to_string(),
            voice: "en".to_string(),
            speaking_rate: 1.0,
            max_length: 200,
            preferred_format: "wav".to_string(),
        }
    }
}

fn mime_for_format(fmt: &str) -> &'static str {
    match fmt.to_ascii_lowercase().as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

impl TtsConfig {
    /// Build from `TTS_*` env vars, falling back to defaults per field.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            service_url: std::env::var("TTS_SERVICE_URL").unwrap_or(d.service_url),
            auth_key: std::env::var("TTS_AUTH_KEY").unwrap_or(d.auth_key),
            mode: std::env::var("TTS_MODE").unwrap_or(d.mode),
            voice: std::env::var("TTS_VOICE").unwrap_or(d.voice),
            speaking_rate: std::env::var("TTS_SPEAKING_RATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.speaking_rate),
            max_length: std::env::var("TTS_MAX_LENGTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.max_length),
            preferred_format: std::env::var("TTS_PREFERRED_FORMAT").unwrap_or(d.preferred_format),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct TtsClient {
    config: Arc<RwLock<TtsConfig>>,
    http: reqwest::Client,
}

impl TtsClient {
    #[must_use]
    pub fn new(config: TtsConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            http: reqwest::Client::new(),
        }
    }

    pub async fn config(&self) -> TtsConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, cfg: TtsConfig) {
        *self.config.write().await = cfg;
    }

    /// Synthesize `text` and return the raw audio bytes plus the
    /// `Content-Type` reported by tts-service (or a default inferred from
    /// `preferred_format` if the server didn't send one).
    pub async fn synthesize(
        &self,
        text: &str,
        voice_override: Option<&str>,
    ) -> Result<(Vec<u8>, String), Error> {
        let cfg = self.config.read().await.clone();
        let voice = voice_override.unwrap_or(&cfg.voice);
        let url = format!("{}/tts", cfg.service_url.trim_end_matches('/'));
        let speaking_rate = cfg.speaking_rate.to_string();
        let max_length = cfg.max_length.to_string();
        let mut req = self.http.get(&url).query(&[
            ("text", text),
            ("lang", voice),
            ("mode", cfg.mode.as_str()),
            ("speaking_rate", speaking_rate.as_str()),
            ("max_length", max_length.as_str()),
            ("preferred_format", cfg.preferred_format.as_str()),
        ]);
        if !cfg.auth_key.is_empty() {
            req = req.header("Authorization", &cfg.auth_key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("tts-service returned {status}: {body}").into());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| mime_for_format(&cfg.preferred_format).to_string());
        Ok((resp.bytes().await?.to_vec(), content_type))
    }

    /// Query the service for the list of available voices under the current mode.
    pub async fn list_voices(&self) -> Result<Vec<String>, Error> {
        let cfg = self.config.read().await.clone();
        let url = format!("{}/voices", cfg.service_url.trim_end_matches('/'));
        let mut req = self.http.get(&url).query(&[("mode", cfg.mode.as_str())]);
        if !cfg.auth_key.is_empty() {
            req = req.header("Authorization", &cfg.auth_key);
        }
        let resp = req.send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Text-to-speech: synthesize via the configured service and play in voice.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("speak", "show", "set", "voices"),
    subcommand_required
)]
pub async fn tts(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Synthesize and play the given text in the bot's current voice channel.
#[poise::command(slash_command, prefix_command, rename = "speak")]
pub async fn speak(
    ctx: Context<'_>,
    #[description = "Text to speak"]
    #[rest]
    text: String,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx.guild_id().ok_or("guild only")?;

    // Lavalink path: synthesize → stash bytes in the audio store → hand
    // lavalink the public URL via play_url. Requires BOT_PUBLIC_URL to be
    // set and a tunnel/reverse-proxy in front of the bot's HTTP layer.
    #[cfg(feature = "music-core")]
    if matches!(
        ctx.data().music.kind(),
        crate::music_backend::BackendKind::Lavalink
    ) {
        let Some(_) = ctx.data().audio_store.public_url() else {
            reply::send(
                &ctx,
                Reply::new()
                    .content(
                        "TTS on the lavalink backend requires `BOT_PUBLIC_URL` \
                         to be set to a URL lavalink can reach (e.g. a \
                         cloudflared tunnel). Switch to `MUSIC_BACKEND=native` \
                         to bypass this requirement.",
                    )
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        };

        let (bytes, content_type) = match ctx.data().tts.synthesize(&text, None).await {
            Ok(b) => b,
            Err(e) => {
                reply::send(
                    &ctx,
                    Reply::new()
                        .content(format!("TTS failed: {e}"))
                        .ephemeral(true)
                        .delete_invoker(true),
                )
                .await?;
                return Ok(());
            }
        };

        let (_id, url) = ctx.data().audio_store.put(bytes, content_type);
        let url = url.expect("public_url was just asserted");

        if let Err(e) = ctx
            .data()
            .music
            .play_url(guild_id, &url, ctx.author().id)
            .await
        {
            reply::send(
                &ctx,
                Reply::new()
                    .content(format!("Lavalink refused TTS URL: {e}"))
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        }

        reply::send(
            &ctx,
            Reply::new()
                .content(format!("🔊 `{text}`"))
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    // Native/no-music-core path: enqueue raw bytes directly onto songbird.
    let manager = ctx.data().songbird.clone();
    let Some(call) = manager.get(guild_id) else {
        reply::send(
            &ctx,
            Reply::new()
                .content("The bot isn't in a voice channel. Use `/join` first.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    let (bytes, _ct) = match ctx.data().tts.synthesize(&text, None).await {
        Ok(b) => b,
        Err(e) => {
            reply::send(
                &ctx,
                Reply::new()
                    .content(format!("TTS failed: {e}"))
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        }
    };

    let mut handler = call.lock().await;
    let _handle = handler.enqueue_input(Input::from(bytes)).await;
    drop(handler);

    reply::send(
        &ctx,
        Reply::new()
            .content(format!("🔊 `{text}`"))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Show the current TTS configuration (auth key redacted).
#[poise::command(
    slash_command,
    prefix_command,
    rename = "show",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    let cfg = ctx.data().tts.config().await;
    let auth = if cfg.auth_key.is_empty() { "(none)" } else { "***" };
    let body = format!(
        "**TTS config**\n\
         service: `{}`\n\
         mode: `{}`\n\
         voice: `{}`\n\
         speaking_rate: `{}`\n\
         max_length: `{}`\n\
         preferred_format: `{}`\n\
         auth: `{}`",
        cfg.service_url,
        cfg.mode,
        cfg.voice,
        cfg.speaking_rate,
        cfg.max_length,
        cfg.preferred_format,
        auth
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

/// (Admin) Update one or more TTS settings. Persists to `config/tts.yaml`.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "set",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn set(
    ctx: Context<'_>,
    #[description = "Service base URL"] service_url: Option<String>,
    #[description = "Auth key (empty to clear)"] auth_key: Option<String>,
    #[description = "Mode: eSpeak | gTTS | gcloud | Polly"] mode: Option<String>,
    #[description = "Default voice"] voice: Option<String>,
    #[description = "Speaking rate"] speaking_rate: Option<f32>,
    #[description = "Max text length"] max_length: Option<u32>,
    #[description = "Preferred audio format"] preferred_format: Option<String>,
) -> Result<(), Error> {
    let mut cfg = ctx.data().tts.config().await;
    if let Some(v) = service_url {
        cfg.service_url = v;
    }
    if let Some(v) = auth_key {
        cfg.auth_key = v;
    }
    if let Some(v) = mode {
        cfg.mode = v;
    }
    if let Some(v) = voice {
        cfg.voice = v;
    }
    if let Some(v) = speaking_rate {
        cfg.speaking_rate = v;
    }
    if let Some(v) = max_length {
        cfg.max_length = v;
    }
    if let Some(v) = preferred_format {
        cfg.preferred_format = v;
    }
    ctx.data().tts.set_config(cfg).await;

    if let Err(e) = ctx.data().save().await {
        reply::send(
            &ctx,
            Reply::new()
                .content(format!("Updated in-memory, but persist failed: {e}"))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }
    reply::send(
        &ctx,
        Reply::new()
            .content("TTS config updated.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) List voices available under the current mode.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "voices",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn voices(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    const MAX_SHOWN: usize = 40;
    match ctx.data().tts.list_voices().await {
        Ok(list) => {
            let total = list.len();
            let sample: Vec<&str> = list.iter().take(MAX_SHOWN).map(String::as_str).collect();
            let body = if total > MAX_SHOWN {
                format!(
                    "**Voices** (first {MAX_SHOWN} of {total}): `{}`",
                    sample.join("`, `")
                )
            } else {
                format!("**Voices** ({total}): `{}`", sample.join("`, `"))
            };
            reply::send(
                &ctx,
                Reply::new()
                    .content(body)
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
        }
        Err(e) => {
            reply::send(
                &ctx,
                Reply::new()
                    .content(format!("Failed to fetch voices: {e}"))
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tts_config_default() {
        let d = TtsConfig::default();
        assert_eq!(d.mode, "eSpeak");
        assert_eq!(d.voice, "en");
        assert!(d.speaking_rate > 0.0);
    }

    #[test]
    fn tts_config_round_trip() {
        let cfg = TtsConfig {
            service_url: "http://tts.example:3000".into(),
            auth_key: "secret".into(),
            mode: "gTTS".into(),
            voice: "en-US".into(),
            speaking_rate: 1.25,
            max_length: 300,
            preferred_format: "mp3".into(),
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: TtsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.service_url, cfg.service_url);
        assert_eq!(back.mode, cfg.mode);
        assert_eq!(back.speaking_rate, cfg.speaking_rate);
    }

    #[tokio::test]
    async fn client_config_round_trips_through_client() {
        let c = TtsClient::new(TtsConfig::default());
        let mut cfg = c.config().await;
        cfg.voice = "fr".into();
        c.set_config(cfg).await;
        assert_eq!(c.config().await.voice, "fr");
    }

    #[test]
    fn tts_commands_defined() {
        let cmd = tts();
        assert_eq!(cmd.name, "tts");
        assert!(cmd.guild_only);
        assert!(cmd.subcommand_required);
        let sub: Vec<&str> = cmd.subcommands.iter().map(|c| &*c.name).collect();
        for s in ["speak", "show", "set", "voices"] {
            assert!(sub.contains(&s), "missing {s}");
        }
    }
}
