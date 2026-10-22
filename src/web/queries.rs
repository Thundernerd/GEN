//! Read queries the web UI needs beyond `Store`'s own methods.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::SourcesConfig;
use crate::model::{Item, ItemKind};
use crate::store::{ItemQuery, Store};

use super::view::{Lookup, mentions_csharp};

/// Rows fetched per round when filtering in Rust (tier / C# filters).
const SCAN_BATCH: u32 = 500;

/// Item list filters shared by the timeline, the feed and the JSON API.
#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    pub engine: Option<String>,
    pub kind: Option<ItemKind>,
    /// Only items tagged with an engine of this tier.
    pub tier: Option<u8>,
    /// Only items tagged with a C#-capable engine, or outlet items mentioning C#.
    pub csharp: bool,
}

impl ItemFilter {
    fn needs_scan(&self) -> bool {
        self.tier.is_some() || self.csharp
    }

    /// The part of the filter the store can't express.
    fn matches(&self, item: &Item, sources: &SourcesConfig, lookup: &Lookup) -> bool {
        let tier_of = |id: &String| sources.engine(id).map(|e| e.tier);
        if let Some(tier) = self.tier
            && !item.engines.iter().any(|e| tier_of(e) == Some(tier))
        {
            return false;
        }
        if self.csharp {
            let csharp_engine = item
                .engines
                .iter()
                .any(|e| tier_of(e).is_some_and(|t| t <= 2));
            let outlet_mention = lookup.outlet_sources.contains(&item.source_id)
                && (mentions_csharp(&item.title)
                    || item.summary.as_deref().is_some_and(mentions_csharp));
            if !csharp_engine && !outlet_mention {
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
