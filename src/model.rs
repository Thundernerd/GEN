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

/// What a Discord bot channel subscribes to per engine: a group of item kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    Releases,
    News,
    Videos,
    Community,
}

impl Topic {
    pub const ALL: [Topic; 4] = [
        Topic::Releases,
        Topic::News,
        Topic::Videos,
        Topic::Community,
    ];

    /// Lowercase name, also used in channel names (e.g. `unity-releases`).
    pub fn slug(self) -> &'static str {
        match self {
            Topic::Releases => "releases",
            Topic::News => "news",
            Topic::Videos => "videos",
            Topic::Community => "community",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Topic::Releases => "Releases",
            Topic::News => "News",
            Topic::Videos => "Videos",
            Topic::Community => "Community",
        }
    }

    pub fn parse(s: &str) -> Option<Topic> {
        Topic::ALL.into_iter().find(|t| t.slug() == s)
    }

    /// The item kinds this topic covers.
    pub fn kinds(self) -> &'static [ItemKind] {
        match self {
            Topic::Releases => &[ItemKind::Release, ItemKind::Prerelease],
            Topic::News => &[ItemKind::News],
            Topic::Videos => &[ItemKind::Video],
            Topic::Community => &[ItemKind::Community],
        }
    }
}

/// An entry as produced by a fetcher, before classification.
#[derive(Debug, Clone, Default)]
pub struct RawItem {
    /// Stable per-source identifier (Atom id, RSS guid, release tag, ...).
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    /// Plain-text summary (fetchers strip HTML). Paragraphs and list items are on lines
    /// of their own.
    pub summary: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// Set when the source says explicitly whether this is a prerelease (e.g. GitHub REST).
    pub prerelease: Option<bool>,
    /// A preview image the source gives (feed media, enclosure or an image in the entry).
    pub image_url: Option<String>,
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
    pub image_url: Option<String>,
    /// Normalized key used for cross-source dedup (see `classify::story_key`).
    pub story_key: String,
    /// Whether this item should be notified (false for a source's first, backfill fetch).
    pub notify: bool,
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
    /// A preview image, from the source or the linked page's `og:image`.
    pub image_url: Option<String>,
    /// The linked page was already looked at for an image (see `fetch::og`).
    pub image_checked: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_belongs_to_exactly_one_topic() {
        for kind in ItemKind::ALL {
            let topics: Vec<_> = Topic::ALL
                .into_iter()
                .filter(|t| t.kinds().contains(&kind))
                .collect();
            assert_eq!(topics.len(), 1, "{kind:?} is in {topics:?}");
        }
        for topic in Topic::ALL {
            assert_eq!(Topic::parse(topic.slug()), Some(topic));
        }
    }
}
