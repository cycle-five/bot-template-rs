//! Speech-to-text via any OpenAI-compatible `/v1/audio/transcriptions`
//! endpoint.
//!
//! One Rust client covers every provider that speaks the OpenAI contract:
//! [lemonfox.ai](https://lemonfox.ai), [OpenAI](https://platform.openai.com),
//! [Groq](https://groq.com), [Fireworks](https://fireworks.ai), and the
//! self-hosted [Speaches](https://github.com/speaches-ai/speaches) sidecar
//! (formerly `faster-whisper-server`). Swapping providers is a config
//! change — different `STT_BASE_URL` + `STT_API_KEY` + `STT_MODEL` — not a
//! rebuild.
//!
//! The backend reads audio bytes (any format Whisper accepts: wav, mp3,
//! ogg/opus, m4a, flac, webm — convenient since `/record` already writes
//! Opus-in-Ogg) and returns the transcript with optional per-segment
//! timing.

use std::sync::Arc;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::reply::{self, Reply};
use crate::{Context, Error};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Runtime-mutable STT settings. Persisted to `config/stt.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    /// Base URL ending in `/v1` (e.g. `http://speaches:8000/v1`,
    /// `https://api.lemonfox.ai/v1`, `https://api.openai.com/v1`).
    pub base_url: String,
    /// Bearer token. Sent as `Authorization: Bearer …` when non-empty.
    /// Self-hosted speaches doesn't need one by default.
    #[serde(default)]
    pub api_key: String,
    /// Model name. Provider-dependent: `whisper-1` for OpenAI/lemonfox,
    /// `Systran/faster-whisper-large-v3` or similar for speaches.
    pub model: String,
    /// ISO-639-1 language hint sent with every request. Empty = auto-detect.
    #[serde(default)]
    pub default_language: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8000/v1".to_string(),
            api_key: String::new(),
            model: "whisper-1".to_string(),
            default_language: String::new(),
        }
    }
}

impl SttConfig {
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            base_url: std::env::var("STT_BASE_URL").unwrap_or(d.base_url),
            api_key: std::env::var("STT_API_KEY").unwrap_or(d.api_key),
            model: std::env::var("STT_MODEL").unwrap_or(d.model),
            default_language: std::env::var("STT_LANGUAGE").unwrap_or(d.default_language),
        }
    }
}

// ---------------------------------------------------------------------------
// Trait + types
// ---------------------------------------------------------------------------

pub struct TranscribeOpts {
    pub language: Option<String>,
    pub prompt: Option<String>,
    /// Filename hint sent as the multipart `Content-Disposition`. Whisper
    /// providers use it to disambiguate audio formats when the bytes are
    /// ambiguous, so pass the real name from the attachment when possible.
    pub filename: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transcript {
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // exposed for callers; internal UI renders .text only
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // see Transcript.segments
pub struct Segment {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

#[async_trait]
pub trait SttBackend: Send + Sync + 'static {
    async fn transcribe(
        &self,
        audio: &[u8],
        opts: &TranscribeOpts,
    ) -> Result<Transcript, Error>;
}

// ---------------------------------------------------------------------------
// HTTP backend (OpenAI-compatible)
// ---------------------------------------------------------------------------

pub struct HttpSttBackend {
    config: Arc<RwLock<SttConfig>>,
    http: reqwest::Client,
}

impl HttpSttBackend {
    #[must_use]
    pub fn new(config: SttConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            http: reqwest::Client::new(),
        }
    }

    pub async fn config(&self) -> SttConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, cfg: SttConfig) {
        *self.config.write().await = cfg;
    }
}

#[async_trait]
impl SttBackend for HttpSttBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        opts: &TranscribeOpts,
    ) -> Result<Transcript, Error> {
        let cfg = self.config.read().await.clone();
        let url = format!(
            "{}/audio/transcriptions",
            cfg.base_url.trim_end_matches('/')
        );

        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name(opts.filename.clone())
            .mime_str(guess_mime(&opts.filename))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", cfg.model.clone())
            .text("response_format", "verbose_json");

        let language = opts
            .language
            .clone()
            .or_else(|| non_empty(&cfg.default_language));
        if let Some(lang) = language {
            form = form.text("language", lang);
        }
        if let Some(prompt) = &opts.prompt {
            form = form.text("prompt", prompt.clone());
        }

        let mut req = self.http.post(&url).multipart(form);
        if !cfg.api_key.is_empty() {
            req = req.bearer_auth(&cfg.api_key);
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("stt backend returned {status}: {body}").into());
        }

        // `verbose_json` supplies `segments` + `language`; plain `json`
        // providers (some resellers ignore `response_format`) return only
        // `text`. `#[serde(default)]` on the richer fields lets us accept
        // both shapes.
        let transcript: Transcript = resp.json().await?;
        Ok(transcript)
    }
}

fn guess_mime(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    match lower.rsplit('.').next() {
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "opus") => "audio/ogg",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("flac") => "audio/flac",
        Some("webm") => "audio/webm",
        Some("mpga" | "mpeg") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn is_audio_attachment(a: &serenity::Attachment) -> bool {
    if let Some(ct) = a.content_type.as_deref() {
        if ct.starts_with("audio/") || ct == "video/webm" {
            return true;
        }
    }
    let name = a.filename.to_ascii_lowercase();
    matches!(
        name.rsplit('.').next(),
        Some(
            "wav" | "mp3" | "ogg" | "opus" | "flac" | "m4a" | "aac"
                | "mp4" | "mpga" | "mpeg" | "webm"
        )
    )
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Speech-to-text: transcribe an audio file via the configured STT provider.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("transcribe", "show", "set"),
    subcommand_required
)]
pub async fn stt(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Transcribe the attached audio file.
#[poise::command(slash_command, prefix_command, rename = "transcribe")]
pub async fn transcribe(
    ctx: Context<'_>,
    #[description = "Audio file to transcribe"] file: serenity::Attachment,
    #[description = "Language hint (ISO-639-1, e.g. en, fr)"] language: Option<String>,
    #[description = "Prompt / context hint"] prompt: Option<String>,
) -> Result<(), Error> {
    ctx.defer().await?;
    transcribe_attachment(&ctx, &file, language, prompt).await
}

/// Right-click / "Apps → Transcribe attachment" on a message with audio.
#[poise::command(context_menu_command = "Transcribe attachment", guild_only)]
pub async fn transcribe_message(
    ctx: Context<'_>,
    #[description = "Message with an audio attachment"] msg: serenity::Message,
) -> Result<(), Error> {
    ctx.defer().await?;
    let Some(att) = msg.attachments.iter().find(|a| is_audio_attachment(a)) else {
        reply::send(
            &ctx,
            Reply::new()
                .content("That message has no audio attachments.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };
    transcribe_attachment(&ctx, att, None, None).await
}

async fn transcribe_attachment(
    ctx: &Context<'_>,
    att: &serenity::Attachment,
    language: Option<String>,
    prompt: Option<String>,
) -> Result<(), Error> {
    let audio = match att.download().await {
        Ok(b) => b,
        Err(e) => {
            reply::send(
                ctx,
                Reply::new()
                    .content(format!("Couldn't download `{}`: {e}", att.filename))
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        }
    };

    let opts = TranscribeOpts {
        language,
        prompt,
        filename: att.filename.clone(),
    };
    let transcript = match ctx.data().stt.transcribe(&audio, &opts).await {
        Ok(t) => t,
        Err(e) => {
            reply::send(
                ctx,
                Reply::new()
                    .content(format!("Transcription failed: {e}"))
                    .ephemeral(true)
                    .delete_invoker(true),
            )
            .await?;
            return Ok(());
        }
    };

    send_transcript(ctx, &transcript, &att.filename).await
}

async fn send_transcript(
    ctx: &Context<'_>,
    transcript: &Transcript,
    source_filename: &str,
) -> Result<(), Error> {
    // Discord content limit is 2000; leave room for the header.
    const INLINE_BUDGET: usize = 1800;

    let lang_tag = transcript
        .language
        .as_deref()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    let header = format!("**Transcript of `{source_filename}`{lang_tag}:**");

    if transcript.text.len() <= INLINE_BUDGET {
        reply::send(
            ctx,
            Reply::new()
                .content(format!("{header}\n{}", transcript.text))
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    // Long transcript: inline a head preview, attach the full text.
    let preview: String = transcript.text.chars().take(INLINE_BUDGET).collect();
    let body = format!(
        "{header}\n{preview}\n\n_…truncated, full transcript attached ({} chars)_",
        transcript.text.len()
    );
    let attachment = serenity::CreateAttachment::bytes(
        transcript.text.as_bytes().to_vec(),
        format!("{source_filename}.txt"),
    );
    reply::send(
        ctx,
        Reply::new()
            .content(body)
            .attachment(attachment)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Show the current STT config (api key redacted).
#[poise::command(
    slash_command,
    prefix_command,
    rename = "show",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn show(ctx: Context<'_>) -> Result<(), Error> {
    let cfg = ctx.data().stt.config().await;
    let auth = if cfg.api_key.is_empty() { "(none)" } else { "***" };
    let lang = if cfg.default_language.is_empty() {
        "(auto)".to_string()
    } else {
        cfg.default_language
    };
    let body = format!(
        "**STT config**\n\
         base_url: `{}`\n\
         model: `{}`\n\
         default_language: `{}`\n\
         auth: `{}`",
        cfg.base_url, cfg.model, lang, auth
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

/// (Admin) Update one or more STT settings. Persists to `config/stt.yaml`.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "set",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn set(
    ctx: Context<'_>,
    #[description = "Base URL ending in /v1"] base_url: Option<String>,
    #[description = "Bearer API key (empty to clear)"] api_key: Option<String>,
    #[description = "Model name"] model: Option<String>,
    #[description = "Default language (ISO-639-1; empty to auto-detect)"]
    default_language: Option<String>,
) -> Result<(), Error> {
    let mut cfg = ctx.data().stt.config().await;
    if let Some(v) = base_url {
        cfg.base_url = v;
    }
    if let Some(v) = api_key {
        cfg.api_key = v;
    }
    if let Some(v) = model {
        cfg.model = v;
    }
    if let Some(v) = default_language {
        cfg.default_language = v;
    }
    ctx.data().stt.set_config(cfg).await;

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
            .content("STT config updated.")
            .ephemeral(true)
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_config_default() {
        let d = SttConfig::default();
        assert_eq!(d.model, "whisper-1");
        assert!(d.default_language.is_empty());
    }

    #[test]
    fn stt_config_round_trip() {
        let cfg = SttConfig {
            base_url: "https://api.lemonfox.ai/v1".into(),
            api_key: "sk-abc".into(),
            model: "whisper-1".into(),
            default_language: "en".into(),
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: SttConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.base_url, cfg.base_url);
        assert_eq!(back.model, cfg.model);
        assert_eq!(back.default_language, cfg.default_language);
    }

    #[tokio::test]
    async fn http_backend_config_round_trips() {
        let b = HttpSttBackend::new(SttConfig::default());
        let mut cfg = b.config().await;
        cfg.model = "whisper-large-v3".into();
        b.set_config(cfg).await;
        assert_eq!(b.config().await.model, "whisper-large-v3");
    }

    #[test]
    fn guess_mime_maps_common_extensions() {
        assert_eq!(guess_mime("a.wav"), "audio/wav");
        assert_eq!(guess_mime("b.OGG"), "audio/ogg");
        assert_eq!(guess_mime("c.opus"), "audio/ogg");
        assert_eq!(guess_mime("d.mp3"), "audio/mpeg");
        assert_eq!(guess_mime("e.flac"), "audio/flac");
        assert_eq!(guess_mime("unknown"), "application/octet-stream");
    }

    #[test]
    fn transcript_accepts_minimal_json() {
        let j = r#"{"text":"hello"}"#;
        let t: Transcript = serde_json::from_str(j).unwrap();
        assert_eq!(t.text, "hello");
        assert!(t.language.is_none());
        assert!(t.segments.is_empty());
    }

    #[test]
    fn transcript_accepts_verbose_json() {
        let j = r#"{
          "text":"hello world",
          "language":"en",
          "segments":[{"start":0.0,"end":1.5,"text":"hello"},
                      {"start":1.5,"end":2.5,"text":"world"}]
        }"#;
        let t: Transcript = serde_json::from_str(j).unwrap();
        assert_eq!(t.text, "hello world");
        assert_eq!(t.language.as_deref(), Some("en"));
        assert_eq!(t.segments.len(), 2);
    }

    #[test]
    fn stt_commands_defined() {
        let cmd = stt();
        assert_eq!(cmd.name, "stt");
        assert!(cmd.guild_only);
        assert!(cmd.subcommand_required);
        let sub: Vec<&str> = cmd.subcommands.iter().map(|c| c.name.as_str()).collect();
        for s in ["transcribe", "show", "set"] {
            assert!(sub.contains(&s), "missing {s}");
        }
    }
}
