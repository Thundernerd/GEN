//! Background polling: one task per source, each polling on its own interval, with a global
//! concurrency limit and exponential backoff on errors. A single notifier task delivers
//! pending notifications from the store, so failed sends are retried and nothing is lost
//! across restarts.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::config::{AppConfig, ChannelConfig, ChannelTarget, ResolvedSource, SourcesConfig};
use crate::fetch::FetchError;
use crate::model::Item;
use crate::notify::{self, NotifyContext};
use crate::pipeline::Pipeline;

/// Longest wait between polls of a failing source.
const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
/// Initial polls are spread over this window so startup doesn't hit every host at once.
const STARTUP_SPREAD: Duration = Duration::from_secs(60);
/// How often a disabled source re-checks its toggle without being woken.
const DISABLED_RECHECK: Duration = Duration::from_secs(5 * 60);
/// How often the notifier retries pending notifications without being woken.
const NOTIFY_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Pending notifications older than this are given up on.
const NOTIFY_MAX_AGE: chrono::TimeDelta = chrono::TimeDelta::days(3);
/// Items handled per channel per notifier run.
const NOTIFY_BATCH: u32 = 100;

/// Spawns a polling task per enabled source plus the notifier task. Dropping the returned
/// `JoinSet` stops them.
pub fn spawn(pipeline: Pipeline, sources: Arc<SourcesConfig>, app: Arc<AppConfig>) -> JoinSet<()> {
    let permits = Arc::new(Semaphore::new(app.polling.concurrency.max(1)));
    let wake = Arc::new(Notify::new());
    let mut tasks = JoinSet::new();
    // Every source of an enabled owner gets a task; disabled ones wait until switched on.
    for source in sources.manageable_sources() {
        let poller = Poller {
            pipeline: pipeline.clone(),
            app: app.clone(),
            permits: permits.clone(),
            wake: wake.clone(),
        };
        tasks.spawn(async move { poller.run(source).await });
    }
    let sources_count = sources
        .manageable_sources()
        .iter()
        .filter(|s| pipeline.toggles.is_enabled(s))
        .count();
    let notifier = Notifier {
        pipeline,
        sources,
        app,
        wake,
    };
    tasks.spawn(async move { notifier.run().await });
    tracing::info!(sources = sources_count, "scheduler started");
    tasks
}

struct Poller {
    pipeline: Pipeline,
    app: Arc<AppConfig>,
    permits: Arc<Semaphore>,
    /// Woken when new notifiable items were stored.
    wake: Arc<Notify>,
}

impl Poller {
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
            while !self.pipeline.toggles.is_enabled(&source) {
                // The timeout is a fallback; toggles wake waiters directly.
                tokio::select! {
                    () = self.pipeline.toggles.changed() => {}
                    () = tokio::time::sleep(DISABLED_RECHECK) => {}
                }
            }
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
                        if poll.was_seeded {
                            self.wake.notify_one();
                        } else {
                            tracing::info!(source = %source.id, "first fetch: stored without notifying");
                        }
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
                    let delay = retry_after
                        .map_or(backoff, |r| r.max(interval))
                        .min(MAX_BACKOFF);
                    tracing::warn!(source = %source.id, failures, retry_in = ?delay, "poll failed: {err:#}");
                    delay
                }
            };
            tokio::time::sleep(delay.saturating_add(spread(&source.id, interval / 20))).await;
        }
    }
}

struct Notifier {
    pipeline: Pipeline,
    sources: Arc<SourcesConfig>,
    app: Arc<AppConfig>,
    wake: Arc<Notify>,
}

impl Notifier {
    async fn run(self) {
        loop {
            for channel in self.app.notify.channels.iter().filter(|c| c.enabled) {
                if let Err(err) = self.deliver(channel).await {
                    tracing::error!(channel = %channel.name, "notification failed: {err:#}");
                }
            }
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(NOTIFY_RETRY_INTERVAL) => {}
            }
        }
    }

    /// Sends one channel's pending items. Each item is marked as handled right after it is
    /// sent (or skipped by the rules), so a failure only leaves unsent items pending.
    async fn deliver(&self, channel: &ChannelConfig) -> anyhow::Result<()> {
        let name = channel.name.clone();
        let pending = self
            .pipeline
            .with_store(move |s| {
                let registered = s.register_channel(&name)?;
                let since = registered.max(chrono::Utc::now() - NOTIFY_MAX_AGE);
                s.pending_notifications(&name, since, NOTIFY_BATCH)
            })
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        let (matching, skipped): (Vec<Item>, Vec<Item>) = pending
            .into_iter()
            .partition(|i| notify::matches(&channel.rules, i));
        self.mark(channel, &skipped).await?;

        let ctx = NotifyContext {
            http: &self.pipeline.http,
            sources: &self.sources,
            base_url: &self.app.server.base_url,
        };
        let digest = matches!(channel.target, ChannelTarget::Email { digest: true, .. });
        let batches: Vec<&[Item]> = if digest {
            vec![matching.as_slice()]
        } else {
            matching.chunks(1).collect()
        };
        for batch in batches.into_iter().filter(|b| !b.is_empty()) {
            notify::send(&ctx, channel, batch).await?;
            self.mark(channel, batch).await?;
            tracing::info!(channel = %channel.name, items = batch.len(), "notified");
        }
        Ok(())
    }

    async fn mark(&self, channel: &ChannelConfig, items: &[Item]) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let name = channel.name.clone();
        let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
        self.pipeline
            .with_store(move |s| {
                ids.iter()
                    .try_for_each(|id| s.mark_notified(*id, &name).map(|_| ()))
            })
            .await
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
