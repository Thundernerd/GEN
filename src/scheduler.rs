//! Background polling: one task per source, each polling on its own interval, with a global
//! concurrency limit and exponential backoff on errors. A single notifier task delivers
//! pending notifications from the store, so failed sends are retried and nothing is lost
//! across restarts.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::task::{AbortHandle, JoinSet};

use crate::config::{AppConfig, ChannelConfig, ChannelTarget, ResolvedSource};
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

/// Spawns the poller supervisor and the notifier task. Dropping the returned `JoinSet`
/// stops them, and with the supervisor every polling task.
pub fn spawn(pipeline: Pipeline, app: Arc<AppConfig>) -> JoinSet<()> {
    let wake = pipeline.notify_wake.clone();
    let poller = Poller {
        pipeline: pipeline.clone(),
        app: app.clone(),
        permits: Arc::new(Semaphore::new(app.polling.concurrency.max(1))),
        wake: wake.clone(),
    };
    let notifier = Notifier {
        pipeline,
        app,
        wake,
    };
    let mut tasks = JoinSet::new();
    tasks.spawn(supervise(poller));
    tasks.spawn(async move { notifier.run().await });
    tasks
}

/// Keeps one polling task per source of an enabled owner (disabled sources wait until
/// switched on), starting and stopping tasks whenever the registry changes.
async fn supervise(poller: Poller) {
    let mut changes = poller.pipeline.registry.subscribe();
    let mut tasks = JoinSet::new();
    let mut running: HashMap<String, (ResolvedSource, AbortHandle)> = HashMap::new();
    let mut startup = true;
    loop {
        let wanted = changes.borrow_and_update().config.manageable_sources();
        let (start, stop) = plan(&running, wanted);
        for id in stop {
            if let Some((_, task)) = running.remove(&id) {
                task.abort();
            }
        }
        for source in start {
            // Spread the first polls at startup; sources added later are polled right away.
            let delay = if startup {
                spread(&source.id, STARTUP_SPREAD)
            } else {
                tracing::debug!(source = %source.id, "poller started");
                Duration::ZERO
            };
            let task = tasks.spawn(poller.clone().run(source.clone(), delay));
            running.insert(source.id.clone(), (source, task));
        }
        if startup {
            let enabled = running
                .values()
                .filter(|(s, _)| poller.pipeline.toggles.is_enabled(s))
                .count();
            tracing::info!(sources = enabled, "scheduler started");
            startup = false;
        }
        tokio::select! {
            changed = changes.changed() => if changed.is_err() { return },
            // Reap finished (aborted) tasks so the set doesn't grow.
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

/// Compares the running pollers with the wanted sources. Returns the sources to start
/// (new or changed) and the ids to stop (removed or changed).
fn plan<T>(
    running: &HashMap<String, (ResolvedSource, T)>,
    wanted: Vec<ResolvedSource>,
) -> (Vec<ResolvedSource>, Vec<String>) {
    let wanted_ids: HashSet<&str> = wanted.iter().map(|s| s.id.as_str()).collect();
    let mut stop: Vec<String> = running
        .keys()
        .filter(|id| !wanted_ids.contains(id.as_str()))
        .cloned()
        .collect();
    let mut start = Vec::new();
    for source in wanted {
        match running.get(&source.id) {
            Some((current, _)) if *current == source => {}
            Some(_) => {
                stop.push(source.id.clone());
                start.push(source);
            }
            None => start.push(source),
        }
    }
    stop.sort();
    (start, stop)
}

#[derive(Clone)]
struct Poller {
    pipeline: Pipeline,
    app: Arc<AppConfig>,
    permits: Arc<Semaphore>,
    /// Woken when new notifiable items were stored.
    wake: Arc<Notify>,
}

impl Poller {
    async fn run(self, source: ResolvedSource, initial_delay: Duration) {
        let interval = Duration::from_secs(
            60 * source
                .interval_minutes
                .unwrap_or(self.app.polling.default_interval_minutes)
                .max(1),
        );
        tokio::time::sleep(initial_delay).await;

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
    app: Arc<AppConfig>,
    wake: Arc<Notify>,
}

impl Notifier {
    async fn run(self) {
        loop {
            self.run_once().await;
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(NOTIFY_RETRY_INTERVAL) => {}
            }
        }
    }

    /// One pass over every enabled channel, recording each one's delivery result.
    async fn run_once(&self) {
        // Read every run, so channels edited on the website take effect right away.
        let channels = match self.pipeline.with_store(|s| notify::load_channels(s)).await {
            Ok(channels) => channels,
            Err(err) => {
                tracing::error!("loading notify channels failed: {err:#}");
                Vec::new()
            }
        };
        for channel in channels.iter().map(|c| &c.config).filter(|c| c.enabled) {
            let name = channel.name.clone();
            let result = match channel.resolved() {
                Ok(resolved) => self.deliver(&resolved).await,
                Err(err) => Err(err),
            };
            let record = match &result {
                Ok(true) => Some(None),
                Ok(false) => None,
                Err(err) => {
                    tracing::error!(channel = %name, "notification failed: {err:#}");
                    Some(Some(format!("{err:#}")))
                }
            };
            if let Some(error) = record {
                let stored = self
                    .pipeline
                    .with_store(move |s| match error {
                        None => s.record_channel_ok(&name),
                        Some(e) => s.record_channel_error(&name, &e),
                    })
                    .await;
                if let Err(err) = stored {
                    tracing::error!("recording a delivery result failed: {err:#}");
                }
            }
        }
    }
    /// Sends one channel's pending items. Each item is marked as handled right after it is
    /// sent (or skipped by the rules), so a failure only leaves unsent items pending.
    /// Returns whether anything was sent.
    async fn deliver(&self, channel: &ChannelConfig) -> anyhow::Result<bool> {
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
            return Ok(false);
        }
        let (matching, skipped): (Vec<Item>, Vec<Item>) = pending
            .into_iter()
            .partition(|i| notify::matches(&channel.rules, i));
        self.mark(channel, &skipped).await?;

        let snapshot = self.pipeline.registry.current();
        let ctx = NotifyContext {
            http: &self.pipeline.http,
            sources: &snapshot.config,
            base_url: &self.app.server.base_url,
        };
        let digest = matches!(channel.target, ChannelTarget::Email { digest: true, .. });
        let batches: Vec<&[Item]> = if digest {
            vec![matching.as_slice()]
        } else {
            matching.chunks(1).collect()
        };
        let mut sent = false;
        for batch in batches.into_iter().filter(|b| !b.is_empty()) {
            notify::send(&ctx, channel, batch).await?;
            self.mark(channel, batch).await?;
            sent = true;
            tracing::info!(channel = %channel.name, items = batch.len(), "notified");
        }
        Ok(sent)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Owner, SourceSpec};

    fn source(id: &str, interval: Option<u64>) -> ResolvedSource {
        ResolvedSource {
            id: id.into(),
            owner: Owner::Engine("godot".into()),
            owner_name: "Godot".into(),
            enabled: true,
            category: None,
            interval_minutes: interval,
            keywords: Vec::new(),
            require_engine_match: false,
            spec: SourceSpec::Hn { query: id.into() },
        }
    }

    /// A channel stored in the database (as the website does) is delivered on the next
    /// notifier run, and the result is recorded on the channel.
    #[tokio::test]
    async fn delivers_to_channels_from_the_store() {
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A one-shot HTTP sink that records the request.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sink = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = conn.read(&mut buf).await.unwrap();
            conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let mut store = crate::store::Store::open_in_memory().unwrap();
        let registry = crate::registry::SourceRegistry::load(&store).unwrap();
        let channel = format!(
            r#"{{"name":"phone","kind":"ntfy","server":"http://{addr}","topic":"gen-test"}}"#
        );
        store.add_channel("phone", &channel).unwrap();
        // Notifiable items must arrive after the channel was added.
        tokio::time::sleep(Duration::from_millis(5)).await;
        store
            .insert_items(&[crate::model::NewItem {
                source_id: "godot:github:godotengine/godot".into(),
                guid: "4.8".into(),
                engines: vec!["godot".into()],
                kind: crate::model::ItemKind::Release,
                title: "Godot 4.8".into(),
                url: None,
                summary: None,
                published: None,
                story_key: "godot 4 8".into(),
                notify: true,
            }])
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
        };
        let notifier = Notifier {
            pipeline: pipeline.clone(),
            app: Arc::new(AppConfig::default()),
            wake: pipeline.notify_wake.clone(),
        };
        notifier.run_once().await;

        let request = sink.await.unwrap();
        assert!(request.starts_with("POST /gen-test "), "{request}");
        assert!(request.contains("Godot 4.8"), "{request}");
        let channels = notify::load_channels(&pipeline.store.lock().unwrap()).unwrap();
        assert!(channels[0].last_ok.is_some() && channels[0].last_error.is_none());
    }

    #[test]
    fn plan_starts_new_stops_removed_and_restarts_changed() {
        let running: HashMap<String, (ResolvedSource, ())> = ["kept", "changed", "removed"]
            .into_iter()
            .map(|id| (id.to_string(), (source(id, None), ())))
            .collect();
        let wanted = vec![
            source("kept", None),
            source("changed", Some(5)),
            source("new", None),
        ];
        let (start, stop) = plan(&running, wanted);
        let started: Vec<_> = start.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(started, ["changed", "new"]);
        assert_eq!(stop, ["changed", "removed"]);
        assert_eq!(start[0].interval_minutes, Some(5));
    }
}
