//! User-owned playlists: save the current queue, restore later, share via a
//! "featured" flag.
//!
//! Persistence goes through [`PlaylistStore`] so the storage backend is
//! swappable (YAML files today, SQLite/etc. later). Playlists are identified
//! by the `(owner, name)` pair.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::UserId;

use crate::music_backend::Track;
use crate::reply::{self, Reply};
use crate::{Context, Error};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Playlist {
    pub owner: u64,
    pub name: String,
    pub tracks: Vec<Track>,
    pub featured: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PlaylistSummary {
    pub owner: u64,
    pub name: String,
    pub track_count: usize,
    pub featured: bool,
    pub updated_at: DateTime<Utc>,
}

impl From<&Playlist> for PlaylistSummary {
    fn from(p: &Playlist) -> Self {
        Self {
            owner: p.owner,
            name: p.name.clone(),
            track_count: p.tracks.len(),
            featured: p.featured,
            updated_at: p.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PlaylistStore: Send + Sync + 'static {
    /// Create or replace a playlist. Re-saving a playlist preserves its
    /// `featured` flag and `created_at` timestamp.
    async fn save(
        &self,
        owner: UserId,
        name: &str,
        tracks: Vec<Track>,
    ) -> Result<Playlist, Error>;

    async fn load(&self, owner: UserId, name: &str) -> Result<Option<Playlist>, Error>;
    async fn delete(&self, owner: UserId, name: &str) -> Result<bool, Error>;
    async fn list_by_owner(&self, owner: UserId) -> Result<Vec<PlaylistSummary>, Error>;
    async fn list_featured(&self) -> Result<Vec<PlaylistSummary>, Error>;
    async fn set_featured(
        &self,
        owner: UserId,
        name: &str,
        featured: bool,
    ) -> Result<(), Error>;
}

// ---------------------------------------------------------------------------
// YAML implementation
// ---------------------------------------------------------------------------

/// File-based playlist store: one YAML file per playlist at
/// `{root}/{owner}__{slug}.yaml`. The slug sanitizes the user-provided name
/// for filesystem use; the canonical name lives inside the file.
pub struct YamlPlaylistStore {
    root: PathBuf,
}

impl YamlPlaylistStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, owner: u64, name: &str) -> PathBuf {
        self.root.join(format!("{}__{}.yaml", owner, slug(name)))
    }

    async fn ensure_dir(&self) -> Result<(), Error> {
        tokio::fs::create_dir_all(&self.root).await?;
        Ok(())
    }
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[async_trait]
impl PlaylistStore for YamlPlaylistStore {
    async fn save(
        &self,
        owner: UserId,
        name: &str,
        tracks: Vec<Track>,
    ) -> Result<Playlist, Error> {
        self.ensure_dir().await?;
        let existing = self.load(owner, name).await.ok().flatten();
        let now = Utc::now();

        let playlist = Playlist {
            owner: owner.get(),
            name: name.to_string(),
            tracks,
            featured: existing.as_ref().is_some_and(|p| p.featured),
            created_at: existing.map_or(now, |p| p.created_at),
            updated_at: now,
        };

        let yaml = serde_yaml::to_string(&playlist)?;
        tokio::fs::write(self.path_for(owner.get(), name), yaml).await?;
        Ok(playlist)
    }

    async fn load(&self, owner: UserId, name: &str) -> Result<Option<Playlist>, Error> {
        let path = self.path_for(owner.get(), name);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(serde_yaml::from_str(&content)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, owner: UserId, name: &str) -> Result<bool, Error> {
        let path = self.path_for(owner.get(), name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_by_owner(&self, owner: UserId) -> Result<Vec<PlaylistSummary>, Error> {
        self.ensure_dir().await?;
        let prefix = format!("{}__", owner.get());
        scan_playlists(&self.root, |p| p.owner == owner.get() && {
            let _ = &prefix;
            true
        })
        .await
    }

    async fn list_featured(&self) -> Result<Vec<PlaylistSummary>, Error> {
        self.ensure_dir().await?;
        scan_playlists(&self.root, |p| p.featured).await
    }

    async fn set_featured(
        &self,
        owner: UserId,
        name: &str,
        featured: bool,
    ) -> Result<(), Error> {
        let Some(mut p) = self.load(owner, name).await? else {
            return Err(format!("no such playlist: {name}").into());
        };
        p.featured = featured;
        p.updated_at = Utc::now();
        let yaml = serde_yaml::to_string(&p)?;
        tokio::fs::write(self.path_for(owner.get(), name), yaml).await?;
        Ok(())
    }
}

async fn scan_playlists<F>(root: &std::path::Path, filter: F) -> Result<Vec<PlaylistSummary>, Error>
where
    F: Fn(&Playlist) -> bool,
{
    let mut dir = tokio::fs::read_dir(root).await?;
    let mut out = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".yaml") {
            continue;
        }
        if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
            if let Ok(p) = serde_yaml::from_str::<Playlist>(&content) {
                if filter(&p) {
                    out.push(PlaylistSummary::from(&p));
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn track_to_query(t: &Track) -> String {
    t.uri
        .clone()
        .unwrap_or_else(|| format!("{} {}", t.author, t.title))
}

/// Parent command for user playlists.
#[poise::command(
    slash_command,
    prefix_command,
    guild_only,
    subcommands("save", "load", "list", "delete", "featured", "feature"),
    subcommand_required
)]
pub async fn playlist(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Save the current queue as a named playlist owned by you.
#[poise::command(slash_command, prefix_command, rename = "save")]
pub async fn save(
    ctx: Context<'_>,
    #[description = "Playlist name"]
    #[rest]
    name: String,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let backend = ctx.data().music.clone();
    let store = ctx.data().playlists.clone();

    let mut tracks = Vec::new();
    if let Some(np) = backend.now_playing(guild_id).await? {
        tracks.push(np);
    }
    tracks.extend(backend.queue_snapshot(guild_id).await?);

    if tracks.is_empty() {
        reply::send(
            &ctx,
            Reply::new()
                .content("Nothing in the queue to save.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }

    let pl = store.save(ctx.author().id, &name, tracks).await?;
    reply::send(
        &ctx,
        Reply::new()
            .content(format!(
                "Saved `{}` with {} track(s).",
                pl.name,
                pl.tracks.len()
            ))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// Load one of your playlists into the queue.
#[poise::command(slash_command, prefix_command, rename = "load")]
pub async fn load(
    ctx: Context<'_>,
    #[description = "Playlist name"]
    #[rest]
    name: String,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().ok_or("guild only")?;
    let backend = ctx.data().music.clone();
    let store = ctx.data().playlists.clone();

    let Some(pl) = store.load(ctx.author().id, &name).await? else {
        reply::send(
            &ctx,
            Reply::new()
                .content(format!("No playlist named `{name}`."))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };

    // Ensure voice connection via the invoker's current channel.
    let cache = &ctx.serenity_context().cache;
    let author_id = ctx.author().id;
    let Some(channel) = cache.guild(guild_id).and_then(|g| {
        g.voice_states
            .get(&author_id)
            .and_then(|vs| vs.channel_id)
    }) else {
        reply::send(
            &ctx,
            Reply::new()
                .content("Join a voice channel first.")
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    };
    backend
        .ensure_joined(ctx.serenity_context(), guild_id, channel)
        .await?;

    let count = pl.tracks.len();
    for t in &pl.tracks {
        let q = track_to_query(t);
        // Individual resolve failures shouldn't abort the whole playlist.
        let _ = backend.play(guild_id, &q, ctx.author().id).await;
    }

    reply::send(
        &ctx,
        Reply::new()
            .content(format!("Enqueued {count} track(s) from `{}`.", pl.name))
            .delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// List your playlists.
#[poise::command(slash_command, prefix_command, rename = "list")]
pub async fn list(ctx: Context<'_>) -> Result<(), Error> {
    let store = ctx.data().playlists.clone();
    let mut items = store.list_by_owner(ctx.author().id).await?;
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let body = if items.is_empty() {
        "You have no saved playlists.".to_string()
    } else {
        let lines: Vec<String> = items
            .iter()
            .map(|p| {
                let star = if p.featured { " ⭐" } else { "" };
                format!("- `{}`{star} ({} track(s))", p.name, p.track_count)
            })
            .collect();
        format!("**Your playlists:**\n{}", lines.join("\n"))
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

/// Delete one of your playlists.
#[poise::command(slash_command, prefix_command, rename = "delete")]
pub async fn delete(
    ctx: Context<'_>,
    #[description = "Playlist name"]
    #[rest]
    name: String,
) -> Result<(), Error> {
    let store = ctx.data().playlists.clone();
    let deleted = store.delete(ctx.author().id, &name).await?;
    let body = if deleted {
        format!("Deleted `{name}`.")
    } else {
        format!("No playlist named `{name}`.")
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

/// List featured playlists across all users.
#[poise::command(slash_command, prefix_command, rename = "featured")]
pub async fn featured(ctx: Context<'_>) -> Result<(), Error> {
    let store = ctx.data().playlists.clone();
    let items = store.list_featured().await?;

    let body = if items.is_empty() {
        "No featured playlists yet.".to_string()
    } else {
        let lines: Vec<String> = items
            .iter()
            .map(|p| {
                format!(
                    "- `{}` by <@{}> ({} track(s))",
                    p.name, p.owner, p.track_count
                )
            })
            .collect();
        format!("**⭐ Featured playlists:**\n{}", lines.join("\n"))
    };

    reply::send(
        &ctx,
        Reply::new().content(body).delete_invoker(true),
    )
    .await?;
    Ok(())
}

/// (Admin) Toggle the featured flag on any user's playlist.
#[poise::command(
    slash_command,
    prefix_command,
    rename = "feature",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn feature(
    ctx: Context<'_>,
    #[description = "Playlist owner"] owner: serenity::User,
    #[description = "Playlist name"] name: String,
    #[description = "Featured?"] featured: bool,
) -> Result<(), Error> {
    let store = ctx.data().playlists.clone();
    if let Err(e) = store.set_featured(owner.id, &name, featured).await {
        reply::send(
            &ctx,
            Reply::new()
                .content(format!("Failed: {e}"))
                .ephemeral(true)
                .delete_invoker(true),
        )
        .await?;
        return Ok(());
    }
    reply::send(
        &ctx,
        Reply::new()
            .content(format!(
                "Set featured={featured} on `{name}` (owner <@{}>).",
                owner.id.get()
            ))
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
    use std::sync::Arc;
    use tempfile::tempdir;

    fn sample_track(title: &str) -> Track {
        Track {
            title: title.into(),
            author: "A".into(),
            uri: Some(format!("https://x/{title}")),
            duration_ms: Some(1000),
            requester: Some(1),
        }
    }

    #[tokio::test]
    async fn yaml_store_save_load_round_trip() {
        let dir = tempdir().unwrap();
        let store = YamlPlaylistStore::new(dir.path());
        let owner = UserId::new(42);
        let tracks = vec![sample_track("one"), sample_track("two")];

        let saved = store.save(owner, "mix", tracks.clone()).await.unwrap();
        assert_eq!(saved.tracks.len(), 2);
        assert!(!saved.featured);

        let loaded = store.load(owner, "mix").await.unwrap().unwrap();
        assert_eq!(loaded.tracks, tracks);
        assert_eq!(loaded.name, "mix");
    }

    #[tokio::test]
    async fn yaml_store_preserves_featured_flag_on_resave() {
        let dir = tempdir().unwrap();
        let store = YamlPlaylistStore::new(dir.path());
        let owner = UserId::new(7);

        store.save(owner, "a", vec![sample_track("t")]).await.unwrap();
        store.set_featured(owner, "a", true).await.unwrap();
        store
            .save(owner, "a", vec![sample_track("u"), sample_track("v")])
            .await
            .unwrap();

        let p = store.load(owner, "a").await.unwrap().unwrap();
        assert!(p.featured, "featured flag must survive re-save");
        assert_eq!(p.tracks.len(), 2);
    }

    #[tokio::test]
    async fn yaml_store_lists_per_owner_and_featured() {
        let dir = tempdir().unwrap();
        let store = YamlPlaylistStore::new(dir.path());

        store.save(UserId::new(1), "mine-a", vec![]).await.unwrap();
        store.save(UserId::new(1), "mine-b", vec![]).await.unwrap();
        store.save(UserId::new(2), "theirs", vec![]).await.unwrap();
        store
            .set_featured(UserId::new(2), "theirs", true)
            .await
            .unwrap();

        let mine = store.list_by_owner(UserId::new(1)).await.unwrap();
        assert_eq!(mine.len(), 2);

        let featured = store.list_featured().await.unwrap();
        assert_eq!(featured.len(), 1);
        assert_eq!(featured[0].name, "theirs");
    }

    #[tokio::test]
    async fn yaml_store_delete_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = YamlPlaylistStore::new(dir.path());
        let owner = UserId::new(5);

        store.save(owner, "a", vec![]).await.unwrap();
        assert!(store.delete(owner, "a").await.unwrap());
        assert!(!store.delete(owner, "a").await.unwrap());
        assert!(store.load(owner, "a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_featured_rejects_unknown_playlist() {
        let dir = tempdir().unwrap();
        let store = YamlPlaylistStore::new(dir.path());
        assert!(
            store
                .set_featured(UserId::new(1), "missing", true)
                .await
                .is_err()
        );
    }

    #[test]
    fn playlist_commands_defined() {
        let cmd = playlist();
        assert_eq!(cmd.name, "playlist");
        assert!(cmd.guild_only);
        assert!(cmd.subcommand_required);
        let sub_names: Vec<&str> = cmd.subcommands.iter().map(|c| c.name.as_str()).collect();
        for expected in ["save", "load", "list", "delete", "featured", "feature"] {
            assert!(sub_names.contains(&expected), "missing subcommand {expected}");
        }
    }

    #[test]
    fn track_to_query_prefers_uri() {
        let with_uri = Track {
            title: "T".into(),
            author: "A".into(),
            uri: Some("https://x".into()),
            duration_ms: None,
            requester: None,
        };
        assert_eq!(track_to_query(&with_uri), "https://x");

        let without = Track { uri: None, ..with_uri };
        assert_eq!(track_to_query(&without), "A T");
    }

    #[test]
    fn slug_sanitizes() {
        assert_eq!(slug("Road Trip!"), "road_trip_");
        assert_eq!(slug("abc-123"), "abc_123");
    }

    #[test]
    fn store_is_dyn_compatible() {
        fn _assert(_: &Arc<dyn PlaylistStore>) {}
    }
}
