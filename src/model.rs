//! Core data types shared by fetchers, the classifier, the store, notifiers and the web UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Engine identifier as written in `sources.toml` (e.g. `"godot"`).
pub type EngineId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Release,
    Prerelease,
    News,
    Video,
    Community,
}

impl ItemKind {
    pub const ALL: [ItemKind; 5] = [
        ItemKind::Release,
        ItemKind::Prerelease,
        ItemKind::News,
        ItemKind::Video,
        ItemKind::Community,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Release => "release",
            ItemKind::Prerelease => "prerelease",
            ItemKind::News => "news",
            ItemKind::Video => "video",
            ItemKind::Community => "community",
        }
    }

    pub fn parse(s: &str) -> Option<ItemKind> {
        ItemKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// An entry as produced by a fetcher, before classification.
#[derive(Debug, Clone, Default)]
pub struct RawItem {
    /// Stable per-source identifier (Atom id, RSS guid, release tag, ...).
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    /// Plain-text summary (fetchers strip HTML).
    pub summary: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// Set when the source says explicitly whether this is a prerelease (e.g. GitHub REST).
    pub prerelease: Option<bool>,
}

/// A classified item ready to be inserted into the store.
#[derive(Debug, Clone)]
pub struct NewItem {
    pub source_id: String,
    pub guid: String,
    pub engines: Vec<EngineId>,
    pub kind: ItemKind,
    pub title: String,
    pub url: Option<String>,
    pub summary: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// Normalized key used for cross-source dedup (see `classify::story_key`).
    pub story_key: String,
}

/// A stored item.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub source_id: String,
    pub guid: String,
    pub engines: Vec<EngineId>,
    pub kind: ItemKind,
    pub title: String,
    pub url: Option<String>,
    pub summary: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub first_seen: DateTime<Utc>,
    pub story_key: String,
    /// Set when the same story was already stored from another source.
    pub duplicate_of: Option<i64>,
}

impl Item {
    /// Best date to sort and display by.
    pub fn date(&self) -> DateTime<Utc> {
        self.published.unwrap_or(self.first_seen)
    }
}

/// Per-source polling state persisted between runs.
#[derive(Debug, Clone, Default)]
pub struct SourceState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl SourceState {
    /// A source counts as seeded once it has been fetched successfully at least once.
    /// Items from the first successful fetch are stored without notifying.
    pub fn is_seeded(&self) -> bool {
        self.last_ok.is_some()
    }
}
