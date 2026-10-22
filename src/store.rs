//! SQLite persistence: items, engine tags, per-source fetch state and sent notifications.
//!
//! `Store` is synchronous; async code shares it as `Arc<Mutex<Store>>` and calls it from
//! `tokio::task::spawn_blocking` (queries are small and fast).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{Item, ItemKind, NewItem, SourceState};

/// Items with the same `story_key` within this window are treated as the same story.
pub const DUPLICATE_WINDOW_DAYS: i64 = 7;

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE items (
    id           INTEGER PRIMARY KEY,
    source_id    TEXT NOT NULL,
    guid         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    title        TEXT NOT NULL,
    url          TEXT,
    summary      TEXT,
    published    TEXT,
    first_seen   TEXT NOT NULL,
    sort_date    TEXT NOT NULL,
    story_key    TEXT NOT NULL,
    duplicate_of INTEGER REFERENCES items(id),
    UNIQUE (source_id, guid)
);
CREATE INDEX items_sort_date ON items (sort_date DESC);
CREATE INDEX items_story_key ON items (story_key);

CREATE TABLE item_engines (
    item_id   INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    engine_id TEXT NOT NULL,
    PRIMARY KEY (item_id, engine_id)
);
CREATE INDEX item_engines_engine ON item_engines (engine_id);

CREATE TABLE source_state (
    source_id            TEXT PRIMARY KEY,
    etag                 TEXT,
    last_modified        TEXT,
    last_ok              TEXT,
    last_error           TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE notifications_sent (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    sent_at TEXT NOT NULL,
    PRIMARY KEY (item_id, channel)
);
"#,
    r#"
-- 1 when the item arrived after its source was seeded, i.e. it should be notified.
ALTER TABLE items ADD COLUMN notify INTEGER NOT NULL DEFAULT 0;
CREATE INDEX items_notify ON items (notify, first_seen);

-- When each notification channel was first used, so new channels don't replay history.
CREATE TABLE notify_channels (
    name       TEXT PRIMARY KEY,
    created_at TEXT NOT NULL
);
"#,
];

/// Filters for listing items. `Default` lists the newest non-duplicate items.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    pub engine: Option<String>,
    pub kind: Option<ItemKind>,
    pub source_id: Option<String>,
    pub include_duplicates: bool,
    /// Defaults to 50 when 0.
    pub limit: u32,
    pub offset: u32,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            conn.pragma_update(None, "user_version", i as i64 + 1)?;
        }
        Ok(Store { conn })
    }

    /// Raw connection for read-only queries that don't warrant a method here.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // -- source state -------------------------------------------------------

    pub fn source_state(&self, source_id: &str) -> anyhow::Result<SourceState> {
        Ok(self
            .conn
            .query_row(
                "SELECT etag, last_modified, last_ok, last_error, consecutive_failures
                 FROM source_state WHERE source_id = ?1",
                [source_id],
                source_state_from_row,
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn source_states(&self) -> anyhow::Result<HashMap<String, SourceState>> {
        let mut stmt = self.conn.prepare(
            "SELECT etag, last_modified, last_ok, last_error, consecutive_failures, source_id
             FROM source_state",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(5)?, source_state_from_row(r)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a successful fetch. Validators are only replaced when the response sent new ones.
    pub fn record_fetch_ok(
        &self,
        source_id: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO source_state (source_id, etag, last_modified, last_ok, last_error, consecutive_failures)
             VALUES (?1, ?2, ?3, ?4, NULL, 0)
             ON CONFLICT (source_id) DO UPDATE SET
                 etag = COALESCE(excluded.etag, etag),
                 last_modified = COALESCE(excluded.last_modified, last_modified),
                 last_ok = excluded.last_ok,
                 last_error = NULL,
                 consecutive_failures = 0",
            params![source_id, etag, last_modified, Utc::now()],
        )?;
        Ok(())
    }

    pub fn record_fetch_error(&self, source_id: &str, error: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO source_state (source_id, last_error, consecutive_failures)
             VALUES (?1, ?2, 1)
             ON CONFLICT (source_id) DO UPDATE SET
                 last_error = excluded.last_error,
                 consecutive_failures = consecutive_failures + 1",
            params![source_id, error],
        )?;
        Ok(())
    }

    // -- items --------------------------------------------------------------

    /// Inserts items, skipping ones already stored for the same source. Returns the newly
    /// stored items; ones whose story was already stored from another source within
    /// `DUPLICATE_WINDOW_DAYS` come back with `duplicate_of` set.
    pub fn insert_items(&mut self, items: &[NewItem]) -> anyhow::Result<Vec<Item>> {
        let now = Utc::now();
        let window_start = now - Duration::days(DUPLICATE_WINDOW_DAYS);
        let tx = self.conn.transaction()?;
        let mut inserted = Vec::new();
        for it in items {
            // Future dates (feed typos) would otherwise pin an item to the top.
            let sort_date = it.published.map_or(now, |p| p.min(now));
            let duplicate_of: Option<i64> = if it.story_key.is_empty() {
                None
            } else {
                tx.query_row(
                    "SELECT id FROM items
                     WHERE story_key = ?1 AND source_id != ?2 AND duplicate_of IS NULL
                       AND sort_date >= ?3
                     ORDER BY id LIMIT 1",
                    params![it.story_key, it.source_id, window_start],
                    |r| r.get(0),
                )
                .optional()?
            };
            let changed = tx.execute(
                "INSERT INTO items (source_id, guid, kind, title, url, summary, published,
                                    first_seen, sort_date, story_key, duplicate_of, notify)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (source_id, guid) DO NOTHING",
                params![
                    it.source_id,
                    it.guid,
                    it.kind.as_str(),
                    it.title,
                    it.url,
                    it.summary,
                    it.published,
                    now,
                    sort_date,
                    it.story_key,
                    duplicate_of,
                    it.notify,
                ],
            )?;
            if changed == 0 {
                continue;
            }
            let id = tx.last_insert_rowid();
            for engine in &it.engines {
                tx.execute(
                    "INSERT OR IGNORE INTO item_engines (item_id, engine_id) VALUES (?1, ?2)",
                    params![id, engine],
                )?;
            }
            inserted.push(Item {
                id,
                source_id: it.source_id.clone(),
                guid: it.guid.clone(),
                engines: it.engines.clone(),
                kind: it.kind,
                title: it.title.clone(),
                url: it.url.clone(),
                summary: it.summary.clone(),
                published: it.published,
                first_seen: now,
                story_key: it.story_key.clone(),
                duplicate_of,
            });
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Newest first.
    pub fn query_items(&self, q: &ItemQuery) -> anyhow::Result<Vec<Item>> {
        let mut sql = format!("{ITEM_SELECT} WHERE 1 = 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !q.include_duplicates {
            sql.push_str(" AND i.duplicate_of IS NULL");
        }
        if let Some(engine) = &q.engine {
            args.push(Box::new(engine.clone()));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM item_engines e WHERE e.item_id = i.id AND e.engine_id = ?{})",
                args.len()
            ));
        }
        if let Some(kind) = q.kind {
            args.push(Box::new(kind.as_str()));
            sql.push_str(&format!(" AND i.kind = ?{}", args.len()));
        }
        if let Some(source_id) = &q.source_id {
            args.push(Box::new(source_id.clone()));
            sql.push_str(&format!(" AND i.source_id = ?{}", args.len()));
        }
        let limit = if q.limit == 0 { 50 } else { q.limit };
        sql.push_str(&format!(
            " ORDER BY i.sort_date DESC, i.id DESC LIMIT {limit} OFFSET {}",
            q.offset
        ));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), item_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// For each given item id, the source ids of items marked as its duplicates.
    pub fn duplicate_sources(&self, ids: &[i64]) -> anyhow::Result<HashMap<i64, Vec<String>>> {
        let mut out: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT source_id FROM items WHERE duplicate_of = ?1 ORDER BY id")?;
        for &id in ids {
            let sources = stmt
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if !sources.is_empty() {
                out.insert(id, sources);
            }
        }
        Ok(out)
    }

    // -- notifications ------------------------------------------------------

    /// Records a channel's first use (idempotent) and returns that time.
    pub fn register_channel(&self, name: &str) -> anyhow::Result<DateTime<Utc>> {
        self.conn.execute(
            "INSERT OR IGNORE INTO notify_channels (name, created_at) VALUES (?1, ?2)",
            params![name, Utc::now()],
        )?;
        Ok(self.conn.query_row(
            "SELECT created_at FROM notify_channels WHERE name = ?1",
            [name],
            |r| r.get(0),
        )?)
    }

    /// Notifiable, non-duplicate items first seen at or after `since` that haven't been
    /// handled for `channel` yet, oldest first.
    pub fn pending_notifications(
        &self,
        channel: &str,
        since: DateTime<Utc>,
        limit: u32,
    ) -> anyhow::Result<Vec<Item>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{ITEM_SELECT}
             WHERE i.notify = 1 AND i.duplicate_of IS NULL AND i.first_seen >= ?2
               AND NOT EXISTS (SELECT 1 FROM notifications_sent n
                               WHERE n.item_id = i.id AND n.channel = ?1)
             ORDER BY i.first_seen, i.id LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![channel, since, limit], item_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Marks an item as handled for a channel (sent, or skipped by the channel's rules).
    /// Returns false if it was already marked.
    pub fn mark_notified(&self, item_id: i64, channel: &str) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO notifications_sent (item_id, channel, sent_at) VALUES (?1, ?2, ?3)",
            params![item_id, channel, Utc::now()],
        )?;
        Ok(changed > 0)
    }
}

const ITEM_SELECT: &str = "SELECT i.id, i.source_id, i.guid, i.kind, i.title, i.url, i.summary,
        i.published, i.first_seen, i.story_key, i.duplicate_of,
        (SELECT group_concat(engine_id, ',') FROM item_engines e WHERE e.item_id = i.id)
    FROM items i";

fn item_from_row(r: &Row) -> rusqlite::Result<Item> {
    let kind: String = r.get(3)?;
    let engines: Option<String> = r.get(11)?;
    Ok(Item {
        id: r.get(0)?,
        source_id: r.get(1)?,
        guid: r.get(2)?,
        kind: ItemKind::parse(&kind).unwrap_or(ItemKind::News),
        title: r.get(4)?,
        url: r.get(5)?,
        summary: r.get(6)?,
        published: r.get::<_, Option<DateTime<Utc>>>(7)?,
        first_seen: r.get(8)?,
        story_key: r.get(9)?,
        duplicate_of: r.get(10)?,
        engines: engines
            .map(|s| s.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

fn source_state_from_row(r: &Row) -> rusqlite::Result<SourceState> {
    Ok(SourceState {
        etag: r.get(0)?,
        last_modified: r.get(1)?,
        last_ok: r.get(2)?,
        last_error: r.get(3)?,
        consecutive_failures: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_item(source: &str, guid: &str, story: &str) -> NewItem {
        NewItem {
            source_id: source.into(),
            guid: guid.into(),
            engines: vec!["godot".into()],
            kind: ItemKind::Release,
            title: format!("Title {guid}"),
            url: Some(format!("https://example.com/{guid}")),
            summary: None,
            published: Some(Utc::now()),
            story_key: story.into(),
            notify: false,
        }
    }

    #[test]
    fn dedups_same_source_and_marks_cross_source_duplicates() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store
            .insert_items(&[new_item("a", "1", "godot 4 8"), new_item("a", "2", "other")])
            .unwrap();
        assert_eq!(first.len(), 2);
        // Same items again: nothing new.
        assert!(
            store
                .insert_items(&[new_item("a", "1", "godot 4 8")])
                .unwrap()
                .is_empty()
        );
        // Same story from another source: stored as duplicate.
        let dup = store
            .insert_items(&[new_item("b", "x", "godot 4 8")])
            .unwrap();
        assert_eq!(dup[0].duplicate_of, Some(first[0].id));

        let listed = store.query_items(&ItemQuery::default()).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].engines, vec!["godot".to_string()]);
        let dups = store.duplicate_sources(&[first[0].id]).unwrap();
        assert_eq!(dups[&first[0].id], vec!["b".to_string()]);
    }

    #[test]
    fn filters_by_engine_and_kind() {
        let mut store = Store::open_in_memory().unwrap();
        let mut news = new_item("a", "1", "");
        news.kind = ItemKind::News;
        news.engines = vec!["unity".into()];
        store.insert_items(&[news, new_item("a", "2", "")]).unwrap();
        let q = ItemQuery {
            engine: Some("unity".into()),
            ..Default::default()
        };
        assert_eq!(store.query_items(&q).unwrap().len(), 1);
        let q = ItemQuery {
            kind: Some(ItemKind::Release),
            ..Default::default()
        };
        assert_eq!(store.query_items(&q).unwrap()[0].guid, "2");
    }

    #[test]
    fn tracks_source_state_and_notifications() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(!store.source_state("s").unwrap().is_seeded());
        store.record_fetch_error("s", "boom").unwrap();
        assert_eq!(store.source_state("s").unwrap().consecutive_failures, 1);
        store.record_fetch_ok("s", Some("\"etag\""), None).unwrap();
        store
            .record_fetch_ok("s", None, Some("Wed, 21 Oct 2026 07:28:00 GMT"))
            .unwrap();
        let st = store.source_state("s").unwrap();
        assert!(st.is_seeded());
        assert_eq!(st.etag.as_deref(), Some("\"etag\""));
        assert_eq!(st.consecutive_failures, 0);

        let item = store
            .insert_items(&[new_item("s", "1", "")])
            .unwrap()
            .remove(0);
        assert!(store.mark_notified(item.id, "discord").unwrap());
        assert!(!store.mark_notified(item.id, "discord").unwrap());
    }

    #[test]
    fn pending_notifications_are_durable_until_marked() {
        let mut store = Store::open_in_memory().unwrap();
        let notifiable = |guid: &str, story: &str| NewItem {
            notify: true,
            ..new_item("a", guid, story)
        };
        let backfill = new_item("a", "0", "");
        let stored = store
            .insert_items(&[
                backfill,
                notifiable("1", "story one x"),
                notifiable("2", ""),
            ])
            .unwrap();
        // A cross-source duplicate is never pending.
        store
            .insert_items(&[NewItem {
                notify: true,
                ..new_item("b", "x", "story one x")
            }])
            .unwrap();

        let since = Utc::now() - Duration::hours(1);
        let pending = |s: &Store| -> Vec<String> {
            s.pending_notifications("discord", since, 10)
                .unwrap()
                .into_iter()
                .map(|i| i.guid)
                .collect()
        };
        assert_eq!(pending(&store), vec!["1", "2"]);
        store.mark_notified(stored[1].id, "discord").unwrap();
        assert_eq!(pending(&store), vec!["2"]);
        // Other channels are independent; items before `since` are ignored.
        assert_eq!(
            store
                .pending_notifications("ntfy", since, 10)
                .unwrap()
                .len(),
            2
        );
        let future = Utc::now() + Duration::hours(1);
        assert!(
            store
                .pending_notifications("ntfy", future, 10)
                .unwrap()
                .is_empty()
        );

        let first = store.register_channel("ntfy").unwrap();
        assert_eq!(store.register_channel("ntfy").unwrap(), first);
    }

    #[test]
    fn clamps_future_dates_for_sorting() {
        let mut store = Store::open_in_memory().unwrap();
        let mut future = new_item("a", "future", "");
        future.published = Some(Utc::now() + Duration::days(365));
        let mut recent = new_item("a", "recent", "");
        recent.published = Some(Utc::now() - Duration::minutes(1));
        store.insert_items(&[recent]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.insert_items(&[future]).unwrap();
        let listed = store.query_items(&ItemQuery::default()).unwrap();
        // The future item sorts as "now" (its insert time), not a year ahead...
        assert_eq!(listed[0].guid, "future");
        // ...and keeps its original published date for display.
        assert!(listed[0].published.unwrap() > Utc::now());
    }
}
