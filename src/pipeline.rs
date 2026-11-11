//! One poll of one source: fetch -> classify -> store.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::sync::{Notify, watch};

use crate::config::ResolvedSource;
use crate::fetch::{self, FetchError, FetchOutcome, Http};
use crate::model::{Item, NewItem};
use crate::notify::alert;
use crate::registry::SourceRegistry;
use crate::scheduler::FetchRequests;
use crate::store::Store;
use crate::toggles::SourceToggles;

/// Upper bound on entries taken from a single fetch.
const MAX_ITEMS_PER_FETCH: usize = 200;

/// How far before a source's newest stored item a new entry may be dated and still be
/// notified, for feeds that list entries a little out of order. Older entries (e.g. ones
/// that came into view because the source's window grew) are stored but not notified.
const NOTIFY_GRACE: chrono::TimeDelta = chrono::TimeDelta::days(2);

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
        let (state, newest_published) = self
            .with_store({
                let id = source.id.clone();
                move |s| Ok((s.source_state(&id)?, s.newest_published(&id)?))
            })
            .await?;
        let was_seeded = state.is_seeded();
        let notify_cutoff = newest_published.map(|newest| newest - NOTIFY_GRACE);

        let outcome = match fetch::fetch_source(&self.http, source, &state).await {
            Ok(outcome) => outcome,
            // Skipped while the host cools down from a rate limit: not this source's
            // failure, so its stored record stays as it is.
            Err(err @ FetchError::CoolingDown { .. }) => return Err(err.into()),
            Err(err) => {
                let msg = err.to_string();
                let enabled = self.toggles.is_enabled(source);
                let interval = source
                    .interval_minutes
                    .unwrap_or_else(|| self.settings.default_interval());
                let source = source.clone();
                let alerted = self
                    .with_store(move |s| {
                        alert::after_fetch_error(s, &source, &msg, enabled, interval, Utc::now())
                    })
                    .await?;
                if alerted {
                    self.notify_wake.notify_one();
                }
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
                        notify: was_seeded
                            && !matches!(
                                (item.published, notify_cutoff),
                                (Some(published), Some(cutoff)) if published < cutoff
                            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{respond, serve};

    /// An RSS feed with one entry per `(guid, pubDate)`.
    fn rss(entries: &[(&str, &str)]) -> String {
        let items: String = entries
            .iter()
            .map(|(guid, date)| {
                format!(
                    "<item><title>Godot news {guid}</title><guid>{guid}</guid>\
                     <link>https://godotengine.org/{guid}</link><pubDate>{date}</pubDate></item>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Godot</title>\
             <link>https://godotengine.org</link><description>news</description>{items}\
             </channel></rss>"
        )
    }

    /// A pipeline over a fresh store with one Godot feed source, which answers its first
    /// fetch with `first` and every later fetch with `later`.
    async fn feed_source(first: String, later: String) -> (Pipeline, ResolvedSource) {
        let addr = serve(move |index, _| {
            let body = if index == 0 { &first } else { &later };
            respond("200 OK", &[("content-type", "application/rss+xml")], body)
        })
        .await
        .addr;
        let mut store = Store::open_in_memory().unwrap();
        let config = format!(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            languages = ["GDScript", "C#", "C++"]
              [[engine.source]]
              kind = "feed"
              url = "http://{addr}/rss.xml"
              category = "news"
            "#
        );
        let registry = SourceRegistry::with_owners(&mut store, &config);
        let source = registry.current().config.all_sources().remove(0);
        let pipeline = Pipeline {
            http: Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        (pipeline, source)
    }

    /// The titles of the stored items that will be sent to notification channels.
    async fn notified(pipeline: &Pipeline) -> Vec<String> {
        let mut titles: Vec<String> = pipeline
            .with_store(|s| s.pending_notifications("test", chrono::DateTime::UNIX_EPOCH, 100))
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        titles.sort();
        titles
    }

    /// Once a source has items, an entry dated well before its newest one (here because
    /// the source's window grew) is stored but not notified. A newer entry still is, and so
    /// is one listed a little out of order.
    #[tokio::test]
    async fn older_entries_coming_into_view_are_stored_but_not_notified() {
        let first = rss(&[("day10", "Sat, 10 Oct 2026 12:00:00 GMT")]);
        let second = rss(&[
            ("day11", "Sun, 11 Oct 2026 12:00:00 GMT"),
            ("day10", "Sat, 10 Oct 2026 12:00:00 GMT"),
            ("day9", "Fri, 09 Oct 2026 12:00:00 GMT"),
            ("day3", "Sat, 03 Oct 2026 12:00:00 GMT"),
        ]);
        let (pipeline, source) = feed_source(first, second).await;

        let seeded = pipeline.poll(&source).await.unwrap();
        assert_eq!(seeded.new_items.len(), 1);
        assert!(
            notified(&pipeline).await.is_empty(),
            "the backfill isn't notified"
        );

        let widened = pipeline.poll(&source).await.unwrap();
        let mut stored: Vec<_> = widened.new_items.into_iter().map(|i| i.title).collect();
        stored.sort();
        assert_eq!(
            stored,
            ["Godot news day11", "Godot news day3", "Godot news day9"]
        );
        assert_eq!(
            notified(&pipeline).await,
            ["Godot news day11", "Godot news day9"]
        );
    }

    /// An entry dated in the future (a feed typo) doesn't hold back the source's later
    /// entries: a source's newest item counts as no newer than when it was fetched.
    #[tokio::test]
    async fn a_future_dated_entry_does_not_silence_new_ones() {
        let first = rss(&[
            ("typo", "Fri, 10 Oct 2036 12:00:00 GMT"),
            ("old", "Sat, 10 Oct 2026 12:00:00 GMT"),
        ]);
        let second = rss(&[("new", &Utc::now().to_rfc2822())]);
        let (pipeline, source) = feed_source(first, second).await;

        pipeline.poll(&source).await.unwrap();
        pipeline.poll(&source).await.unwrap();
        assert_eq!(notified(&pipeline).await, ["Godot news new"]);
    }

    /// A source whose items were all deleted by retention still knows its newest one, so a
    /// wider window doesn't notify its older entries.
    #[tokio::test]
    async fn pruned_items_still_count_as_the_newest() {
        let first = rss(&[("day10", "Sat, 10 Oct 2026 12:00:00 GMT")]);
        let second = rss(&[
            ("day11", "Sun, 11 Oct 2026 12:00:00 GMT"),
            ("day3", "Sat, 03 Oct 2026 12:00:00 GMT"),
        ]);
        let (pipeline, source) = feed_source(first, second).await;

        pipeline.poll(&source).await.unwrap();
        let pruned = pipeline
            .with_store(|s| s.prune_items(Utc::now() + chrono::TimeDelta::days(1)))
            .await
            .unwrap();
        assert_eq!(pruned, 1);
        pipeline.poll(&source).await.unwrap();
        assert_eq!(notified(&pipeline).await, ["Godot news day11"]);
    }
}
