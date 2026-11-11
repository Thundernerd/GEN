//! Read queries the web UI needs beyond `Store`'s own methods.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::SourcesConfig;
use crate::model::{Item, ItemKind};
use crate::store::{ItemQuery, Store};

use super::view::{Lookup, mentions_csharp};

/// The one language with a keyword rule for outlet articles, and what `?csharp=1` means.
pub const CSHARP: &str = "C#";

/// Whether two language names are the same language, ignoring case.
pub fn same_language(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Rows fetched per round when filtering in Rust (tier / language filters).
const SCAN_BATCH: u32 = 500;

/// Item list filters shared by the timeline, the feed and the JSON API.
#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    pub engine: Option<String>,
    pub kind: Option<ItemKind>,
    /// Only items tagged with an engine of this tier.
    pub tier: Option<u8>,
    /// Only items tagged with an engine programmed in this language (any case). For C#,
    /// also outlet items mentioning C# or .NET.
    pub lang: Option<String>,
    /// Full-text search over title and summary.
    pub q: Option<String>,
}

impl ItemFilter {
    /// The part of the filter `/feed.xml` takes: engine, kind and search.
    pub fn for_feed(self) -> ItemFilter {
        ItemFilter {
            engine: self.engine,
            kind: self.kind,
            q: self.q,
            ..Default::default()
        }
    }

    fn needs_scan(&self) -> bool {
        self.tier.is_some() || self.lang.is_some()
    }

    /// The part of the filter the store can't express.
    fn matches(&self, item: &Item, sources: &SourcesConfig, lookup: &Lookup) -> bool {
        let tier_of = |id: &String| sources.engine(id).map(|e| e.tier);
        if let Some(tier) = self.tier
            && !item.engines.iter().any(|e| tier_of(e) == Some(tier))
        {
            return false;
        }
        if let Some(lang) = &self.lang {
            let lang_engine = item.engines.iter().any(|e| {
                sources
                    .engine(e)
                    .is_some_and(|e| e.languages.iter().any(|l| same_language(l, lang)))
            });
            // Only C# has a keyword rule: names like "Go" or "Rust" are too ambiguous in prose.
            let outlet_mention = same_language(lang, CSHARP)
                && lookup.outlet_sources.contains(&item.source_id)
                && (mentions_csharp(&item.title)
                    || item.summary.as_deref().is_some_and(mentions_csharp));
            if !lang_engine && !outlet_mention {
                return false;
            }
        }
        true
    }
}

/// Newest non-duplicate items matching `filter`, skipping `offset` matches.
pub fn list_items(
    store: &Store,
    sources: &SourcesConfig,
    lookup: &Lookup,
    filter: &ItemFilter,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<Item>> {
    let base = ItemQuery {
        engine: filter.engine.clone(),
        kind: filter.kind,
        q: filter.q.clone(),
        ..Default::default()
    };
    if !filter.needs_scan() {
        return store.query_items(&ItemQuery {
            limit,
            offset,
            ..base
        });
    }
    let (mut out, mut skip, mut scan_offset) = (Vec::new(), offset, 0);
    loop {
        let batch = store.query_items(&ItemQuery {
            limit: SCAN_BATCH,
            offset: scan_offset,
            ..base.clone()
        })?;
        let fetched = batch.len();
        for item in batch {
            if !filter.matches(&item, sources, lookup) {
                continue;
            }
            if skip > 0 {
                skip -= 1;
                continue;
            }
            out.push(item);
            if out.len() >= limit as usize {
                return Ok(out);
            }
        }
        if fetched < SCAN_BATCH as usize {
            return Ok(out);
        }
        scan_offset += SCAN_BATCH;
    }
}

/// Per-engine item count and latest item date.
#[derive(Debug, Clone)]
pub struct EngineStats {
    pub items: i64,
    pub latest: Option<DateTime<Utc>>,
}

/// Stats for every engine that has at least one non-duplicate item.
pub fn engine_stats(store: &Store) -> anyhow::Result<HashMap<String, EngineStats>> {
    let mut stmt = store.conn().prepare(
        "SELECT e.engine_id, COUNT(*), MAX(i.sort_date)
         FROM item_engines e JOIN items i ON i.id = e.item_id
         WHERE i.duplicate_of IS NULL
         GROUP BY e.engine_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            EngineStats {
                items: r.get(1)?,
                latest: r.get(2)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = r#"
[[engine]]
id = "godot"
name = "Godot"
tier = 3
languages = ["GDScript", "C#", "C++"]
  [[engine.source]]
  kind = "github"
  repo = "godotengine/godot"
  category = "release"

[[engine]]
id = "bevy"
name = "Bevy"
tier = 1
languages = ["Rust"]
  [[engine.source]]
  kind = "feed"
  url = "https://bevyengine.org/news/rss.xml"
  category = "news"

[[outlet]]
id = "gfs"
name = "GameFromScratch"
  [[outlet.source]]
  kind = "feed"
  url = "https://gamefromscratch.com/feed/"
  category = "news"
"#;
    // Tiers are the opposite of the catalog's, so a filter by tier would fail these tests.
    const GODOT_SRC: &str = "godot:github:godotengine/godot";
    const BEVY_SRC: &str = "bevy:feed:https://bevyengine.org/news/rss.xml";
    const GFS_SRC: &str = "gfs:feed:https://gamefromscratch.com/feed/";

    fn item(source: &str, engines: &[&str], title: &str) -> Item {
        Item {
            id: 1,
            source_id: source.into(),
            guid: title.into(),
            engines: engines.iter().map(|e| e.to_string()).collect(),
            kind: ItemKind::News,
            title: title.into(),
            url: None,
            summary: None,
            published: None,
            first_seen: Utc::now(),
            story_key: title.into(),
            duplicate_of: None,
            image_url: None,
            image_checked: false,
        }
    }

    /// Whether a filter on `lang` keeps `item`.
    fn keeps(lang: &str, item: &Item) -> bool {
        let sources: SourcesConfig = toml::from_str(SOURCES).unwrap();
        let lookup = Lookup::new(&sources);
        let filter = ItemFilter {
            lang: Some(lang.into()),
            ..Default::default()
        };
        filter.matches(item, &sources, &lookup)
    }

    #[test]
    fn language_filter_keeps_items_of_engines_with_that_language() {
        assert!(keeps("Rust", &item(BEVY_SRC, &["bevy"], "Bevy 0.17")));
        assert!(!keeps("Rust", &item(GODOT_SRC, &["godot"], "Godot 4.8")));
    }

    #[test]
    fn csharp_filter_ignores_case_and_keeps_outlet_items_mentioning_dotnet() {
        let godot = item(GODOT_SRC, &["godot"], "Godot 4.8");
        for lang in ["C#", "c#"] {
            assert!(keeps(lang, &godot), "{lang}");
        }
        let article = Item {
            summary: Some("Builds on .NET 10".into()),
            ..item(GFS_SRC, &[], "A new engine appears")
        };
        assert!(keeps("c#", &article));
    }

    #[test]
    fn other_languages_have_no_keyword_rule_for_outlet_items() {
        let article = item(GFS_SRC, &[], "Why we rewrote our engine in Rust");
        assert!(!keeps("Rust", &article));
    }
}
