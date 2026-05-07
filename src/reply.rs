//! Unified reply abstraction for prefix, slash, and context-menu commands.
//!
//! Commands build a [`Reply`] and hand it to [`send`]. The dispatcher picks
//! the right Discord-side path, honors `ephemeral` where possible (and
//! degrades to a short auto-delete for prefix commands), and enforces
//! one-live-message-per-[`Slot`] semantics via delete-and-resend.

use std::borrow::Cow;
use std::time::Duration;

use poise::CreateReply;
use poise::serenity_prelude as serenity;
use serenity::{CreateAttachment, CreateEmbed, GenericChannelId, Http, MessageId};
use tracing::debug;

use crate::{Context, Error};

/// A logical "panel" for the replace-previous behavior. Sending into a slot
/// deletes the slot's previous message (if any) before posting the new one.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Slot {
    #[cfg(feature = "music-core")]
    NowPlaying,
    #[cfg(feature = "music-core")]
    QueueView,
    #[cfg(feature = "music-core")]
    Status,
    BotStatus,
    Generic(Cow<'static, str>),
}

#[derive(Debug, Clone, Copy)]
pub struct TrackedMessage {
    pub channel_id: GenericChannelId,
    pub message_id: MessageId,
}

impl TrackedMessage {
    pub async fn delete(&self, http: &Http) -> serenity::Result<()> {
        http.delete_message(self.channel_id, self.message_id, None).await
    }
}

#[derive(Default)]
pub struct Reply {
    content: Option<String>,
    embeds: Vec<CreateEmbed<'static>>,
    attachments: Vec<CreateAttachment<'static>>,
    ephemeral: bool,
    slot: Option<Slot>,
    auto_delete: Option<Duration>,
    delete_invoker: bool,
}

#[allow(dead_code)]
impl Reply {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn content(mut self, s: impl Into<String>) -> Self {
        self.content = Some(s.into());
        self
    }

    #[must_use]
    pub fn embed(mut self, e: CreateEmbed<'static>) -> Self {
        self.embeds.push(e);
        self
    }

    #[must_use]
    pub fn attachment(mut self, a: CreateAttachment<'static>) -> Self {
        self.attachments.push(a);
        self
    }

    #[must_use]
    pub fn ephemeral(mut self, v: bool) -> Self {
        self.ephemeral = v;
        self
    }

    #[must_use]
    pub fn slot(mut self, s: Slot) -> Self {
        self.slot = Some(s);
        self
    }

    #[must_use]
    pub fn auto_delete(mut self, d: Duration) -> Self {
        self.auto_delete = Some(d);
        self
    }

    #[must_use]
    pub fn delete_invoker(mut self, v: bool) -> Self {
        self.delete_invoker = v;
        self
    }
}

const PREFIX_EPHEMERAL_FALLBACK: Duration = Duration::from_secs(10);

/// Send a reply. Returns the tracked message handle, or `None` if the reply
/// was ephemeral (ephemeral messages can't be managed via plain HTTP delete).
///
/// # Errors
/// Forwards errors from [`poise::Context::send`] or the follow-up
/// message fetch.
pub async fn send(
    ctx: &Context<'_>,
    mut reply: Reply,
) -> Result<Option<TrackedMessage>, Error> {
    let is_prefix = matches!(ctx, poise::Context::Prefix(_));

    // Ephemeral on prefix commands isn't a real Discord feature — degrade to
    // a short auto-delete so the "temporary" intent is approximately kept.
    if reply.ephemeral && is_prefix {
        reply.ephemeral = false;
        reply.slot = None;
        if reply.auto_delete.is_none() {
            reply.auto_delete = Some(PREFIX_EPHEMERAL_FALLBACK);
        }
    }

    let http = ctx.serenity_context().http.clone();
    let guild_id = ctx.guild_id();

    if let (Some(slot), Some(gid)) = (reply.slot.as_ref(), guild_id)
        && let Some((_, old)) = ctx.data().tracked_messages.remove(&(gid, slot.clone())) {
            let http_clone = http.clone();
            tokio::spawn(async move {
                if let Err(e) = old.delete(&http_clone).await {
                    debug!(
                        target: "bot_template_rs::reply",
                        error = %e,
                        "failed to delete previous slotted message"
                    );
                }
            });
        }

    if reply.delete_invoker
        && let poise::Context::Prefix(pctx) = ctx {
            let msg = pctx.msg.clone();
            let http_clone = http.clone();
            tokio::spawn(async move {
                if let Err(e) = msg.delete(&http_clone, None).await {
                    // Typically missing MANAGE_MESSAGES — intentionally quiet.
                    debug!(
                        target: "bot_template_rs::reply",
                        error = %e,
                        "couldn't delete invoker message (missing MANAGE_MESSAGES?)"
                    );
                }
            });
        }

    let mut create = CreateReply::default().ephemeral(reply.ephemeral);
    if let Some(c) = reply.content {
        create = create.content(c);
    }
    for e in reply.embeds {
        create = create.embed(e);
    }
    for a in reply.attachments {
        create = create.attachment(a);
    }

    let handle = ctx.send(create).await?;

    if reply.ephemeral {
        return Ok(None);
    }

    let msg = handle.into_message().await?;
    let tracked = TrackedMessage {
        channel_id: msg.channel_id,
        message_id: msg.id,
    };

    if let (Some(slot), Some(gid)) = (reply.slot, guild_id) {
        ctx.data().tracked_messages.insert((gid, slot), tracked);
    }

    if let Some(delay) = reply.auto_delete {
        let http_clone = http.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(e) = tracked.delete(&http_clone).await {
                debug!(
                    target: "bot_template_rs::reply",
                    error = %e,
                    "auto-delete failed"
                );
            }
        });
    }

    Ok(Some(tracked))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_new_is_empty() {
        let r = Reply::new();
        assert!(r.content.is_none());
        assert!(r.embeds.is_empty());
        assert!(r.attachments.is_empty());
        assert!(!r.ephemeral);
        assert!(r.slot.is_none());
        assert!(r.auto_delete.is_none());
        assert!(!r.delete_invoker);
    }

    #[test]
    fn reply_builder_chains_into_a_complete_payload() {
        let r = Reply::new()
            .content("hello")
            .ephemeral(true)
            .auto_delete(Duration::from_secs(5))
            .delete_invoker(true)
            .slot(Slot::BotStatus);
        assert_eq!(r.content.as_deref(), Some("hello"));
        assert!(r.ephemeral);
        assert_eq!(r.auto_delete, Some(Duration::from_secs(5)));
        assert!(r.delete_invoker);
        assert!(matches!(r.slot, Some(Slot::BotStatus)));
    }

    #[test]
    fn slot_generic_is_unique_per_label() {
        let a = Slot::Generic("foo".into());
        let b = Slot::Generic("foo".into());
        let c = Slot::Generic("bar".into());
        assert_eq!(a, b, "same label slots compare equal");
        assert_ne!(a, c, "different labels don't collide");
    }
}
