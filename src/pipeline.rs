//! One poll of one source: fetch -> classify -> store.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::config::ResolvedSource;
use crate::fetch::{self, FetchOutcome, Http};
use crate::model::{Item, NewItem};
use crate::registry::SourceRegistry;
use crate::store::Store;
use crate::toggles::SourceToggles;

/// Upper bound on entries taken from a single fetch.
const MAX_ITEMS_PER_FETCH: usize = 200;

#[derive(Clone)]
pub struct Pipeline {
    pub http: Http,
    pub store: Arc<Mutex<Store>>,
    pub registry: Arc<SourceRegistry>,
    pub toggles: Arc<SourceToggles>,
    /// Wakes the notifier: new notifiable items were stored, or channels changed.
    pub notify_wake: Arc<Notify>,
}

#[derive(Debug)]
pub struct PollResult {
    /// Newly stored items (including cross-source duplicates).
    pub new_items: Vec<Item>,
    /// False on the source's first successful fetch: those items are a backfill and must
    /// not trigger notifications.
    pub was_seeded: bool,
    pub not_modified: bool,
}

impl Pipeline {
    pub async fn poll(&self, source: &ResolvedSource) -> anyhow::Result<PollResult> {
        let state = self
            .with_store({
                let id = source.id.clone();
                move |s| s.source_state(&id)
            })
            .await?;
        let was_seeded = state.is_seeded();

        let outcome = match fetch::fetch_source(&self.http, source, &state).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let (id, msg) = (source.id.clone(), err.to_string());
                self.with_store(move |s| s.record_fetch_error(&id, &msg))
                    .await?;
                return Err(err.into());
            }
        };

        match outcome {
            FetchOutcome::NotModified => {
                let id = source.id.clone();
                self.with_store(move |s| s.record_fetch_ok(&id, None, None))
                    .await?;
                Ok(PollResult {
                    new_items: Vec::new(),
                    was_seeded,
                    not_modified: true,
                })
            }
            FetchOutcome::Fetched {
                items,
                etag,
                last_modified,
            } => {
                let mut items = items;
                // Newest first, so the cap keeps the most recent entries of a huge feed.
                items.sort_by_key(|i| std::cmp::Reverse(i.published));
                items.truncate(MAX_ITEMS_PER_FETCH);
                let snapshot = self.registry.current();
                let classified: Vec<_> = items
                    .into_iter()
                    .filter_map(|raw| snapshot.classifier.classify(source, raw))
                    .map(|item| NewItem {
                        notify: was_seeded,
                        ..item
                    })
                    .collect();
                let id = source.id.clone();
                let new_items = self
                    .with_store(move |s| {
                        let new = s.insert_items(&classified)?;
                        s.record_fetch_ok(&id, etag.as_deref(), last_modified.as_deref())?;
                        Ok(new)
                    })
                    .await?;
                Ok(PollResult {
                    new_items,
                    was_seeded,
                    not_modified: false,
                })
            }
        }
    }

    /// Runs a store operation on the blocking pool.
    pub async fn with_store<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Store) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            f(&mut guard)
        })
        .await?
    }
}
