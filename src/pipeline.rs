//! One poll of one source: fetch -> classify -> store.

use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, watch};

use crate::config::ResolvedSource;
use crate::fetch::{self, FetchError, FetchOutcome, Http};
use crate::model::{Item, NewItem};
use crate::registry::SourceRegistry;
use crate::scheduler::FetchRequests;
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
    pub settings: Arc<PollSettings>,
    /// "Fetch now" requests for the running pollers.
    pub fetches: Arc<FetchRequests>,
}

/// `meta` key holding the default polling interval set on the website.
pub const DEFAULT_INTERVAL_KEY: &str = "default_interval_minutes";

/// Polling settings that can change while GEN runs.
pub struct PollSettings {
    default_interval: watch::Sender<u64>,
}

impl PollSettings {
    pub fn new(default_interval_minutes: u64) -> Self {
        PollSettings {
            default_interval: watch::Sender::new(default_interval_minutes.max(1)),
        }
    }

    /// Minutes between polls of a source without its own interval.
    pub fn default_interval(&self) -> u64 {
        *self.default_interval.borrow()
    }

    pub fn set_default_interval(&self, minutes: u64) {
        self.default_interval.send_replace(minutes.max(1));
    }

    /// Marked changed whenever the default interval changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.default_interval.subscribe()
    }
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
            // Skipped while the host cools down from a rate limit: not this source's
            // failure, so its stored record stays as it is.
            Err(err @ FetchError::CoolingDown { .. }) => return Err(err.into()),
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
