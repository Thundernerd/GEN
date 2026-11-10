//! SQLite persistence: items, engine tags, per-source fetch state, sent notifications and
//! the engines, outlets and sources themselves.
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
    r#"
-- Runtime on/off switches for sources, overriding `enabled` in sources.toml.
CREATE TABLE source_overrides (
    source_id  TEXT PRIMARY KEY,
    enabled    INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
"#,
    r#"
-- Engines and outlets added in the web UI. `config` is the JSON of EngineConfig/OutletConfig.
CREATE TABLE custom_owners (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('engine', 'outlet')),
    config     TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Sources added in the web UI. `config` is the JSON of SourceConfig.
CREATE TABLE custom_sources (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL,
    config     TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Engines, outlets and sources from sources.toml that were removed in the web UI.
CREATE TABLE removed (
    kind       TEXT NOT NULL CHECK (kind IN ('owner', 'source')),
    id         TEXT NOT NULL,
    removed_at TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
"#,
    r#"
-- Engines, outlets and sources live only in the database now; sources.toml is gone.
ALTER TABLE custom_owners RENAME TO owners;
ALTER TABLE custom_sources RENAME TO sources;
DROP TABLE removed;

-- Small key/value facts about the database, e.g. whether the defaults were seeded.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#,
    r#"
-- Notification channels are managed on the website. `config` is the JSON of ChannelConfig;
-- rows without one only record when a since-removed gen.toml channel was first used.
ALTER TABLE notify_channels ADD COLUMN config TEXT;
ALTER TABLE notify_channels ADD COLUMN last_ok TEXT;
ALTER TABLE notify_channels ADD COLUMN last_error TEXT;
ALTER TABLE notify_channels ADD COLUMN last_error_at TEXT;
"#,
    r#"
-- A preview image per item: from the source, or the linked page's og:image, which is
-- looked up once when notifying (image_checked).
ALTER TABLE items ADD COLUMN image_url TEXT;
ALTER TABLE items ADD COLUMN image_checked INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
-- Full-text search over item titles and summaries, kept in step with items by triggers.
CREATE VIRTUAL TABLE items_fts USING fts5(
    title, summary, content = 'items', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER items_fts_insert AFTER INSERT ON items BEGIN
    INSERT INTO items_fts (rowid, title, summary) VALUES (new.id, new.title, new.summary);
END;
CREATE TRIGGER items_fts_delete AFTER DELETE ON items BEGIN
    INSERT INTO items_fts (items_fts, rowid, title, summary)
    VALUES ('delete', old.id, old.title, old.summary);
END;
CREATE TRIGGER items_fts_update AFTER UPDATE OF title, summary ON items BEGIN
    INSERT INTO items_fts (items_fts, rowid, title, summary)
    VALUES ('delete', old.id, old.title, old.summary);
    INSERT INTO items_fts (rowid, title, summary) VALUES (new.id, new.title, new.summary);
END;
INSERT INTO items_fts (items_fts) VALUES ('rebuild');
"#,
];

/// A configured notification channel with its last delivery result.
#[derive(Debug, Clone)]
pub struct StoredChannel {
    pub name: String,
    /// JSON of `ChannelConfig`.
    pub config: String,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

/// Filters for listing items. `Default` lists the newest non-duplicate items.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    pub engine: Option<String>,
    pub kind: Option<ItemKind>,
    pub source_id: Option<String>,
    /// Full-text search over title and summary: every word must match.
    pub q: Option<String>,
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

    /// Runtime on/off overrides per source id.
    pub fn source_overrides(&self) -> anyhow::Result<HashMap<String, bool>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id, enabled FROM source_overrides")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_source_override(&self, source_id: &str, enabled: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO source_overrides (source_id, enabled, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (source_id) DO UPDATE SET
                 enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![source_id, enabled, Utc::now()],
        )?;
        Ok(())
    }

    // -- engines, outlets and sources --------------------------------------

    /// Engines and outlets, oldest first: (id, kind, config JSON).
    pub fn owners(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, config FROM owners ORDER BY created_at, rowid")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Sources, oldest first: (source id, owner id, config JSON).
    pub fn sources(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, owner_id, config FROM sources ORDER BY created_at, rowid")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds an engine or outlet together with `sources`: (source id, config JSON).
    pub fn add_owner(
        &mut self,
        id: &str,
        kind: &str,
        config: &str,
        sources: &[(String, String)],
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO owners (id, kind, config, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, kind, config, Utc::now()],
        )?;
        for (source_id, source_config) in sources {
            insert_source(&tx, source_id, id, source_config)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replaces an engine's or outlet's settings (not its sources).
    pub fn update_owner(&self, id: &str, config: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE owners SET config = ?2 WHERE id = ?1",
            params![id, config],
        )?;
        Ok(())
    }

    pub fn add_source(&self, id: &str, owner_id: &str, config: &str) -> anyhow::Result<()> {
        insert_source(&self.conn, id, owner_id, config)?;
        Ok(())
    }

    /// Replaces a stored source's settings. Returns false if there is no such source.
    pub fn update_source(&self, id: &str, config: &str) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE sources SET config = ?2 WHERE id = ?1",
            params![id, config],
        )?;
        Ok(changed > 0)
    }

    /// Removes a source with its items, fetch state and override.
    pub fn remove_source(&mut self, id: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM sources WHERE id = ?1", [id])?;
        purge_source_data(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Removes an engine or outlet together with `source_ids` (its current sources) and
    /// their data. An engine's tags on other sources' items are removed too.
    pub fn remove_owner(
        &mut self,
        id: &str,
        is_engine: bool,
        source_ids: &[String],
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM owners WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM sources WHERE owner_id = ?1", [id])?;
        for source_id in source_ids {
            purge_source_data(&tx, source_id)?;
        }
        if is_engine {
            tx.execute("DELETE FROM item_engines WHERE engine_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Ids of every source that has items, fetch state or an override stored.
    pub fn stored_source_ids(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id FROM items UNION SELECT source_id FROM source_state
             UNION SELECT source_id FROM source_overrides ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes the items, fetch state and overrides of `source_ids`.
    pub fn purge_sources(&mut self, source_ids: &[String]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        for source_id in source_ids {
            purge_source_data(&tx, source_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn delete_meta(&self, key: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
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
                                    first_seen, sort_date, story_key, duplicate_of, notify,
                                    image_url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
                    it.image_url,
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
                image_url: it.image_url.clone(),
                image_checked: false,
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
        if let Some(fts) = q.q.as_deref().and_then(fts_query) {
            args.push(Box::new(fts));
            sql.push_str(&format!(
                " AND i.id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?{})",
                args.len()
            ));
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

    // -- notification channels ----------------------------------------------

    /// Configured channels, oldest first.
    pub fn channels(&self) -> anyhow::Result<Vec<StoredChannel>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, config, last_ok, last_error, last_error_at FROM notify_channels
             WHERE config IS NOT NULL ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredChannel {
                name: r.get(0)?,
                config: r.get(1)?,
                last_ok: r.get(2)?,
                last_error: r.get(3)?,
                last_error_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds a channel starting now, so it isn't sent items from before. Returns false if a
    /// channel with this name exists.
    pub fn add_channel(&self, name: &str, config: &str) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO notify_channels (name, created_at, config) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET config = excluded.config,
                 created_at = excluded.created_at, last_ok = NULL, last_error = NULL,
                 last_error_at = NULL
             WHERE notify_channels.config IS NULL",
            params![name, Utc::now(), config],
        )?;
        Ok(changed > 0)
    }

    /// Adds a channel imported from gen.toml, keeping when it was first used so its pending
    /// items are neither lost nor sent again. Returns false if a channel with this name exists.
    pub fn import_channel(&self, name: &str, config: &str) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO notify_channels (name, created_at, config) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET config = excluded.config
             WHERE notify_channels.config IS NULL",
            params![name, Utc::now(), config],
        )?;
        Ok(changed > 0)
    }

    /// Replaces a channel's settings. Returns false if there is no such channel.
    pub fn update_channel(&self, name: &str, config: &str) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE notify_channels SET config = ?2 WHERE name = ?1 AND config IS NOT NULL",
            params![name, config],
        )?;
        Ok(changed > 0)
    }

    /// Removes a channel and its delivery history.
    pub fn remove_channel(&mut self, name: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM notifications_sent WHERE channel = ?1", [name])?;
        tx.execute("DELETE FROM notify_channels WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_channel_ok(&self, name: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE notify_channels SET last_ok = ?2, last_error = NULL, last_error_at = NULL
             WHERE name = ?1",
            params![name, Utc::now()],
        )?;
        Ok(())
    }

    pub fn record_channel_error(&self, name: &str, error: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE notify_channels SET last_error = ?2, last_error_at = ?3 WHERE name = ?1",
            params![name, error, Utc::now()],
        )?;
        Ok(())
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

    pub fn item(&self, id: i64) -> anyhow::Result<Option<Item>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ITEM_SELECT} WHERE i.id = ?1"))?;
        Ok(stmt.query_row([id], item_from_row).optional()?)
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

    /// Records the image found on an item's page (or that there is none), so the page
    /// isn't fetched again.
    pub fn set_item_image(&self, item_id: i64, image_url: Option<&str>) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE items SET image_url = COALESCE(?2, image_url), image_checked = 1
             WHERE id = ?1",
            params![item_id, image_url],
        )?;
        Ok(())
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
        (SELECT group_concat(engine_id, ',') FROM item_engines e WHERE e.item_id = i.id),
        i.image_url, i.image_checked
    FROM items i";

/// Turns free text from a search box into an FTS5 query: each word becomes a quoted
/// prefix term, so FTS5 operators and punctuation are taken literally and every word must
/// match. `None` when there are no words.
fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn insert_source(
    conn: &Connection,
    id: &str,
    owner_id: &str,
    config: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sources (id, owner_id, config, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, owner_id, config, Utc::now()],
    )?;
    Ok(())
}

/// Deletes a source's items (with their engine tags and sent notifications), fetch state
/// and override. Items of other sources that were duplicates of these become primaries.
fn purge_source_data(conn: &Connection, source_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET duplicate_of = NULL
         WHERE duplicate_of IN (SELECT id FROM items WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute("DELETE FROM items WHERE source_id = ?1", [source_id])?;
    conn.execute("DELETE FROM source_state WHERE source_id = ?1", [source_id])?;
    conn.execute(
        "DELETE FROM source_overrides WHERE source_id = ?1",
        [source_id],
    )?;
    Ok(())
}

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
        image_url: r.get(12)?,
        image_checked: r.get(13)?,
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
            image_url: None,
        }
    }

    #[test]
    fn stores_item_images_and_lookups() {
        let mut store = Store::open_in_memory().unwrap();
        let mut with_image = new_item("a", "1", "one");
        with_image.image_url = Some("https://example.org/1.png".into());
        let stored = store
            .insert_items(&[with_image, new_item("a", "2", "two")])
            .unwrap();
        let read = |id| store.item(id).unwrap().unwrap();
        let first = read(stored[0].id);
        assert_eq!(
            first.image_url.as_deref(),
            Some("https://example.org/1.png")
        );
        assert!(!first.image_checked);
        store
            .set_item_image(stored[1].id, Some("https://example.org/og.png"))
            .unwrap();
        let second = read(stored[1].id);
        assert_eq!(
            second.image_url.as_deref(),
            Some("https://example.org/og.png")
        );
        assert!(second.image_checked);
        // Finding nothing keeps a known image and still marks the item looked at.
        store.set_item_image(stored[0].id, None).unwrap();
        let first = read(stored[0].id);
        assert_eq!(
            first.image_url.as_deref(),
            Some("https://example.org/1.png")
        );
        assert!(first.image_checked);
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

    fn search(store: &Store, q: &str) -> Vec<String> {
        let q = ItemQuery {
            q: Some(q.into()),
            ..Default::default()
        };
        let mut guids: Vec<_> = store
            .query_items(&q)
            .unwrap()
            .into_iter()
            .map(|i| i.guid)
            .collect();
        guids.sort();
        guids
    }

    #[test]
    fn searches_titles_and_summaries() {
        let mut store = Store::open_in_memory().unwrap();
        let mut a = new_item("a", "1", "");
        a.title = "Godot 4.3 released with Jolt physics".into();
        let mut b = new_item("a", "2", "");
        b.title = "Weekly roundup".into();
        b.summary = Some("New lighting in Unity 6; Godot gets a shader editor".into());
        let mut c = new_item("a", "3", "");
        c.title = "Stride community meeting".into();
        store.insert_items(&[a, b, c]).unwrap();

        assert_eq!(search(&store, "godot"), ["1", "2"]);
        assert_eq!(search(&store, "Jolt"), ["1"]);
        assert_eq!(search(&store, "shader"), ["2"]);
        // Every word must match.
        assert_eq!(search(&store, "godot physics"), ["1"]);
        assert!(search(&store, "bevy").is_empty());
    }

    #[test]
    fn search_takes_any_text_literally() {
        let mut store = Store::open_in_memory().unwrap();
        let mut a = new_item("a", "1", "");
        a.title = "Unity 6.1 \"Beta\" and C# NEAR the editor".into();
        store.insert_items(&[a]).unwrap();

        assert_eq!(search(&store, "uni"), ["1"], "words match as prefixes");
        assert_eq!(search(&store, "\"beta"), ["1"]);
        assert_eq!(search(&store, "NEAR("), ["1"]);
        assert_eq!(search(&store, "c# OR"), Vec::<String>::new());
        assert_eq!(search(&store, "6.1"), ["1"]);
        assert_eq!(search(&store, "*"), Vec::<String>::new());
        // Blank text is no filter at all.
        assert_eq!(search(&store, "   "), ["1"]);
    }

    #[test]
    fn upgrading_indexes_existing_items() {
        // A database from before search, with an item in it.
        let conn = Connection::open_in_memory().unwrap();
        let before_search = MIGRATIONS.len() - 1;
        for sql in &MIGRATIONS[..before_search] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", before_search as i64)
            .unwrap();
        let mut store = Store { conn };
        let mut a = new_item("a", "1", "");
        a.title = "Bevy 0.15 released".into();
        store.insert_items(&[a]).unwrap();

        let store = Store::init(store.conn).unwrap();
        assert_eq!(search(&store, "bevy"), ["1"]);
    }

    #[test]
    fn purged_items_leave_the_search_index() {
        let mut store = Store::open_in_memory().unwrap();
        let mut a = new_item("a", "1", "");
        a.title = "Flax 1.9 released".into();
        store.insert_items(&[a]).unwrap();
        store.purge_sources(&["a".into()]).unwrap();
        // The next item reuses the purged row id.
        let mut b = new_item("b", "2", "");
        b.title = "Stride 4.2 released".into();
        store.insert_items(&[b]).unwrap();

        assert!(search(&store, "flax").is_empty());
        assert_eq!(search(&store, "released"), ["2"]);
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
    fn stores_and_removes_sources() {
        let mut store = Store::open_in_memory().unwrap();
        let hn = ("stride:hn:stride".to_string(), "{}".to_string());
        store
            .add_owner("stride", "engine", "{}", std::slice::from_ref(&hn))
            .unwrap();
        store.add_source("stride:unity", "stride", "{}").unwrap();
        assert_eq!(store.owners().unwrap().len(), 1);
        assert_eq!(
            store.sources().unwrap()[0],
            (hn.0.clone(), "stride".to_string(), hn.1)
        );

        store.remove_source("stride:unity").unwrap();
        assert_eq!(store.sources().unwrap().len(), 1);
        // A failing source insert leaves no half-added owner behind.
        let dup = [(hn.0.clone(), "{}".into()), (hn.0, "{}".into())];
        assert!(store.add_owner("x", "engine", "{}", &dup).is_err());
        assert_eq!(store.owners().unwrap().len(), 1);
    }

    #[test]
    fn purges_sources_and_keeps_meta() {
        let mut store = Store::open_in_memory().unwrap();
        store.insert_items(&[new_item("a", "1", "one")]).unwrap();
        store.record_fetch_ok("b", None, None).unwrap();
        store.set_source_override("c", false).unwrap();
        assert_eq!(store.stored_source_ids().unwrap(), ["a", "b", "c"]);
        store.purge_sources(&["a".into(), "c".into()]).unwrap();
        assert_eq!(store.stored_source_ids().unwrap(), ["b"]);

        assert_eq!(store.meta("seeded").unwrap(), None);
        store.set_meta("seeded", "1").unwrap();
        store.set_meta("seeded", "2").unwrap();
        assert_eq!(store.meta("seeded").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn removing_a_source_purges_its_data() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store
            .insert_items(&[new_item("a", "1", "godot 4 8")])
            .unwrap();
        let dup = store
            .insert_items(&[new_item("b", "x", "godot 4 8")])
            .unwrap();
        assert_eq!(dup[0].duplicate_of, Some(first[0].id));
        store.record_fetch_ok("a", None, None).unwrap();
        store.set_source_override("a", false).unwrap();

        store.remove_source("a").unwrap();
        let listed = store.query_items(&ItemQuery::default()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_id, "b");
        assert_eq!(
            listed[0].duplicate_of, None,
            "the duplicate became the primary"
        );
        assert!(!store.source_state("a").unwrap().is_seeded());
        assert!(store.source_overrides().unwrap().is_empty());
    }

    #[test]
    fn removing_an_engine_drops_its_tags() {
        let mut store = Store::open_in_memory().unwrap();
        store.insert_items(&[new_item("a", "1", "one")]).unwrap();
        store.insert_items(&[new_item("b", "2", "two")]).unwrap();
        store.add_owner("godot", "engine", "{}", &[]).unwrap();

        store
            .remove_owner("godot", true, &["a".to_string()])
            .unwrap();
        assert!(store.owners().unwrap().is_empty());
        let listed = store.query_items(&ItemQuery::default()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].engines.is_empty());
    }

    #[test]
    fn stores_notification_channels() {
        let mut store = Store::open_in_memory().unwrap();
        // A gen.toml channel used before channels moved into the database.
        let first_used = store.register_channel("phone").unwrap();
        assert!(store.channels().unwrap().is_empty(), "no config yet");

        assert!(store.import_channel("phone", "{\"a\":1}").unwrap());
        assert_eq!(
            store.register_channel("phone").unwrap(),
            first_used,
            "history kept"
        );
        assert!(
            !store.import_channel("phone", "{}").unwrap(),
            "already configured"
        );
        assert!(!store.add_channel("phone", "{}").unwrap());

        assert!(store.add_channel("chat", "{}").unwrap());
        assert!(store.update_channel("chat", "{\"b\":2}").unwrap());
        assert!(!store.update_channel("nope", "{}").unwrap());
        store.record_channel_error("chat", "HTTP 404").unwrap();
        let chat = store.channels().unwrap().pop().unwrap();
        assert_eq!(
            (chat.name.as_str(), chat.config.as_str()),
            ("chat", "{\"b\":2}")
        );
        assert_eq!(chat.last_error.as_deref(), Some("HTTP 404"));
        store.record_channel_ok("chat").unwrap();
        let chat = store.channels().unwrap().pop().unwrap();
        assert!(chat.last_ok.is_some() && chat.last_error.is_none());

        let items = store
            .insert_items(&[NewItem {
                notify: true,
                ..new_item("a", "1", "one")
            }])
            .unwrap();
        store.mark_notified(items[0].id, "chat").unwrap();
        store.remove_channel("chat").unwrap();
        assert_eq!(store.channels().unwrap().len(), 1);
        // Re-added, it starts afresh instead of picking up its old history.
        assert!(store.add_channel("chat", "{}").unwrap());
        assert!(store.register_channel("chat").unwrap() > items[0].first_seen);
    }

    #[test]
    fn stores_source_overrides() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.source_overrides().unwrap().is_empty());
        store.set_source_override("a", false).unwrap();
        store.set_source_override("b", true).unwrap();
        store.set_source_override("a", true).unwrap();
        let overrides = store.source_overrides().unwrap();
        assert_eq!(overrides.get("a"), Some(&true));
        assert_eq!(overrides.get("b"), Some(&true));
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
