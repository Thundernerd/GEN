//! Background polling: one task per source, each polling on its own interval, with a global
//! concurrency limit, exponential backoff on errors and notifications for new items.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{AppConfig, ResolvedSource, SourcesConfig};
use crate::fetch::FetchError;
use crate::model::Item;
use crate::notify::{self, NotifyContext};
use crate::pipeline::Pipeline;

/// Longest wait between polls of a failing source.
const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
/// Initial polls are spread over this window so startup doesn't hit every host at once.
const STARTUP_SPREAD: Duration = Duration::from_secs(60);

/// Spawns a polling task per enabled source. Dropping the returned `JoinSet` stops them.
pub fn spawn(pipeline: Pipeline, sources: Arc<SourcesConfig>, app: Arc<AppConfig>) -> JoinSet<()> {
    let permits = Arc::new(Semaphore::new(app.polling.concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for source in sources.enabled_sources() {
        let ctx = TaskContext {
            pipeline: pipeline.clone(),
            sources: sources.clone(),
            app: app.clone(),
            permits: permits.clone(),
        };
        tasks.spawn(async move { ctx.run(source).await });
    }
    tracing::info!(sources = tasks.len(), "scheduler started");
    tasks
}

struct TaskContext {
    pipeline: Pipeline,
    sources: Arc<SourcesConfig>,
    app: Arc<AppConfig>,
    permits: Arc<Semaphore>,
}

impl TaskContext {
    async fn run(self, source: ResolvedSource) {
        let interval = Duration::from_secs(
            60 * source
                .interval_minutes
                .unwrap_or(self.app.polling.default_interval_minutes)
                .max(1),
        );
        tokio::time::sleep(spread(&source.id, STARTUP_SPREAD)).await;

        let mut failures: u32 = 0;
        loop {
            let result = {
                let Ok(_permit) = self.permits.acquire().await else {
                    return;
                };
                self.pipeline.poll(&source).await
            };
            let delay = match result {
                Ok(poll) => {
                    failures = 0;
                    if !poll.new_items.is_empty() {
                        tracing::info!(source = %source.id, new = poll.new_items.len(), "new items");
                    }
                    if poll.was_seeded {
                        self.notify(&poll.new_items).await;
                    } else if !poll.new_items.is_empty() {
                        tracing::info!(source = %source.id, "first fetch: stored without notifying");
                    }
                    interval
                }
                Err(err) => {
                    failures += 1;
                    let backoff = interval
                        .saturating_mul(1 << failures.min(5))
                        .min(MAX_BACKOFF);
                    let retry_after = match err.downcast_ref::<FetchError>() {
                        Some(FetchError::RateLimited { retry_after, .. }) => *retry_after,
                        _ => None,
                    };
                    let delay = retry_after.map_or(backoff, |r| r.max(interval));
                    tracing::warn!(source = %source.id, failures, retry_in = ?delay, "poll failed: {err:#}");
                    delay
                }
            };
            tokio::time::sleep(delay + spread(&source.id, interval / 20)).await;
        }
    }

    /// Sends new, non-duplicate items to every channel whose rules match them.
    async fn notify(&self, items: &[Item]) {
        let fresh: Vec<&Item> = items.iter().filter(|i| i.duplicate_of.is_none()).collect();
        if fresh.is_empty() {
            return;
        }
        let ctx = NotifyContext {
            http: &self.pipeline.http,
            sources: &self.sources,
            base_url: &self.app.server.base_url,
        };
        for channel in self.app.notify.channels.iter().filter(|c| c.enabled) {
            let matching: Vec<Item> = fresh
                .iter()
                .filter(|i| notify::matches(&channel.rules, i))
                .map(|i| (*i).clone())
                .collect();
            if matching.is_empty() {
                continue;
            }
            match notify::send(&ctx, channel, &matching).await {
                Ok(()) => {
                    let name = channel.name.clone();
                    let ids: Vec<i64> = matching.iter().map(|i| i.id).collect();
                    let marked = self
                        .pipeline
                        .with_store(move |s| {
                            ids.iter()
                                .try_for_each(|id| s.mark_notified(*id, &name).map(|_| ()))
                        })
                        .await;
                    if let Err(err) = marked {
                        tracing::error!(channel = %channel.name, "recording notifications failed: {err:#}");
                    }
                    tracing::info!(channel = %channel.name, items = matching.len(), "notified");
                }
                Err(err) => tracing::error!(channel = %channel.name, "notification failed: {err}"),
            }
        }
    }
}

/// Deterministic per-source offset in `[0, max)`, varied per call by the current time.
fn spread(source_id: &str, max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    source_id.hash(&mut hasher);
    std::time::SystemTime::now().hash(&mut hasher);
    Duration::from_millis(hasher.finish() % max.as_millis().max(1) as u64)
}
