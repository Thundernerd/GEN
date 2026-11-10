//! Background polling: one task per source, each polling on its own interval, with a global
//! concurrency limit and exponential backoff on errors. A single notifier task delivers
//! pending notifications from the store, so failed sends are retried and nothing is lost
//! across restarts. With `[database] retention_days` set, a third task prunes old items daily.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

use crate::config::{AppConfig, ChannelConfig, ChannelTarget, ResolvedSource};
use crate::fetch::{self, FetchError};
use crate::model::Item;
use crate::notify::discord::BotApi;
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
/// How often items past `[database] retention_days` are deleted.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Items handled per channel per notifier run.
const NOTIFY_BATCH: u32 = 100;

/// The outcome of a requested fetch.
#[derive(Debug, Clone)]
pub struct FetchReport {
    pub new_items: usize,
    pub not_modified: bool,
    pub error: Option<String>,
}

impl FetchReport {
    fn failed(error: impl Into<String>) -> Self {
        FetchReport {
            new_items: 0,
            not_modified: false,
            error: Some(error.into()),
        }
    }
}

type FetchReply = oneshot::Sender<FetchReport>;

/// "Fetch now" requests, routed to the poller of each running source, and what the
/// pollers are doing right now, for the progress shown on the Sources page.
#[derive(Default)]
pub struct FetchRequests {
    pollers: Mutex<HashMap<String, mpsc::Sender<FetchReply>>>,
    activity: Mutex<Activity>,
}

#[derive(Default)]
struct Activity {
    /// Sources waiting for a fetch slot (`false`) or fetching (`true`).
    sources: HashMap<String, bool>,
    batch: Option<BatchProgress>,
    next_batch: u64,
}

/// How far a "Fetch all now" has got.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct BatchProgress {
    pub total: usize,
    pub done: usize,
    pub new_items: usize,
    pub failed: usize,
    #[serde(skip)]
    id: u64,
}

/// What the pollers are doing, as shown on the Sources page.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ActivitySnapshot {
    pub fetching: Vec<String>,
    pub queued: Vec<String>,
    pub batch: Option<BatchProgress>,
}

/// Tracks a "Fetch all now" until dropped.
pub struct Batch<'a> {
    requests: &'a FetchRequests,
    id: u64,
}

impl Batch<'_> {
    /// Counts one finished fetch of the batch.
    pub fn finished(&self, report: Option<&FetchReport>) {
        let mut activity = self.requests.activity();
        if let Some(b) = activity.batch.as_mut().filter(|b| b.id == self.id) {
            b.done += 1;
            match report {
                Some(r) if r.error.is_none() => b.new_items += r.new_items,
                _ => b.failed += 1,
            }
        }
    }
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        let mut activity = self.requests.activity();
        if activity.batch.as_ref().is_some_and(|b| b.id == self.id) {
            activity.batch = None;
        }
    }
}

impl FetchRequests {
    fn register(&self, source_id: &str) -> mpsc::Receiver<FetchReply> {
        let (tx, rx) = mpsc::channel(4);
        self.lock().insert(source_id.to_string(), tx);
        rx
    }

    fn unregister(&self, source_id: &str) {
        self.lock().remove(source_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::Sender<FetchReply>>> {
        self.pollers.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn activity(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks a source as waiting for a fetch slot, fetching, or (`None`) idle.
    fn set_state(&self, source_id: &str, fetching: Option<bool>) {
        let mut activity = self.activity();
        match fetching {
            Some(f) => activity.sources.insert(source_id.to_string(), f),
            None => activity.sources.remove(source_id),
        };
    }

    /// Starts counting a "Fetch all now" of `total` sources, replacing any earlier one.
    pub fn start_batch(&self, total: usize) -> Batch<'_> {
        let mut activity = self.activity();
        activity.next_batch += 1;
        let id = activity.next_batch;
        activity.batch = Some(BatchProgress {
            total,
            id,
            ..Default::default()
        });
        Batch { requests: self, id }
    }

    pub fn snapshot(&self) -> ActivitySnapshot {
        let activity = self.activity();
        let (mut fetching, mut queued): (Vec<_>, Vec<_>) =
            activity.sources.iter().partition(|(_, f)| **f);
        fetching.sort();
        queued.sort();
        let ids = |v: Vec<(&String, &bool)>| v.into_iter().map(|(id, _)| id.clone()).collect();
        ActivitySnapshot {
            fetching: ids(fetching),
            queued: ids(queued),
            batch: activity.batch.clone(),
        }
    }

    /// Asks a source's poller to fetch now. The receiver gets the result once the fetch is
    /// done; `None` if the source has no running poller.
    pub fn request(&self, source_id: &str) -> Option<oneshot::Receiver<FetchReport>> {
        let tx = self.lock().get(source_id)?.clone();
        let (reply, rx) = oneshot::channel();
        // A full queue means fetches are already waiting; the reply then reports that.
        if let Err(mpsc::error::TrySendError::Full(reply)) = tx.try_send(reply) {
            let _ = reply.send(FetchReport::failed("a fetch is already queued"));
        }
        Some(rx)
    }
}

/// Spawns the poller supervisor and the notifier task. Dropping the returned `JoinSet`
/// stops them, and with the supervisor every polling task.
pub fn spawn(pipeline: Pipeline, app: Arc<AppConfig>) -> JoinSet<()> {
    let wake = pipeline.notify_wake.clone();
    let mut tasks = JoinSet::new();
    if let Some(retention) = app.database.retention() {
        tasks.spawn(prune(pipeline.clone(), retention));
    }
    let poller = Poller {
        pipeline: pipeline.clone(),
        permits: Arc::new(Semaphore::new(app.polling.concurrency.max(1))),
        wake: wake.clone(),
    };
    let notifier = Notifier {
        pipeline,
        bot: BotApi::from_config(&app),
        app,
        wake,
    };
    tasks.spawn(supervise(poller));
    tasks.spawn(async move { notifier.run().await });
    tasks
}

/// Deletes items older than `retention`, at startup and then once a day.
async fn prune(pipeline: Pipeline, retention: chrono::TimeDelta) {
    loop {
        let cutoff = chrono::Utc::now() - retention;
        match pipeline.with_store(move |s| s.prune_items(cutoff)).await {
            Ok(0) => {}
            Ok(pruned) => tracing::info!(pruned, "pruned items past retention"),
            Err(err) => tracing::warn!("pruning items failed: {err:#}"),
        }
        tokio::time::sleep(PRUNE_INTERVAL).await;
    }
}

/// Keeps one polling task per source of an enabled owner (disabled sources wait until
/// switched on), starting and stopping tasks whenever the registry changes.
async fn supervise(poller: Poller) {
    let mut changes = poller.pipeline.registry.subscribe();
    let fetches = poller.pipeline.fetches.clone();
    let mut tasks = JoinSet::new();
    let mut running: HashMap<String, (ResolvedSource, AbortHandle)> = HashMap::new();
    let mut startup = true;
    loop {
        let wanted = changes.borrow_and_update().config.manageable_sources();
        let (start, stop) = plan(&running, wanted);
        for id in stop {
            if let Some((_, task)) = running.remove(&id) {
                task.abort();
                fetches.unregister(&id);
                fetches.set_state(&id, None);
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
            let requests = fetches.register(&source.id);
            let task = tasks.spawn(poller.clone().run(source.clone(), delay, requests));
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
    permits: Arc<Semaphore>,
    /// Woken when new notifiable items were stored.
    wake: Arc<Notify>,
}

impl Poller {
    async fn run(
        self,
        source: ResolvedSource,
        initial_delay: Duration,
        mut requests: mpsc::Receiver<FetchReply>,
    ) {
        let settings = self.pipeline.settings.clone();
        let mut defaults = settings.subscribe();
        let interval = || {
            let minutes = source
                .interval_minutes
                .unwrap_or_else(|| settings.default_interval());
            Duration::from_secs(60 * minutes.max(1))
        };
        let mut next = Instant::now() + initial_delay;
        let mut last_poll: Option<Instant> = None;
        let mut failures: u32 = 0;
        loop {
            // Wait for the next poll, a "fetch now" request, or a new default interval.
            let reply = loop {
                tokio::select! {
                    () = tokio::time::sleep_until(next) => break None,
                    Some(reply) = requests.recv() => break Some(reply),
                    Ok(()) = defaults.changed(), if source.interval_minutes.is_none() => {
                        // Reschedule from the last poll; a failing source keeps its backoff.
                        if let (Some(last), 0) = (last_poll, failures) {
                            next = last + interval() + spread(&source.id, interval() / 20);
                        }
                    }
                }
            };
            let mut reply = reply;
            while !self.pipeline.toggles.is_enabled(&source) {
                if let Some(r) = reply.take() {
                    let _ = r.send(FetchReport::failed("the source is switched off"));
                }
                // The timeout is a fallback; toggles wake waiters directly.
                tokio::select! {
                    () = self.pipeline.toggles.changed() => {}
                    () = tokio::time::sleep(DISABLED_RECHECK) => {}
                    Some(r) = requests.recv() => {
                        let _ = r.send(FetchReport::failed("the source is switched off"));
                    }
                }
            }
            let requested = reply.is_some();
            let fetches = &self.pipeline.fetches;
            fetches.set_state(&source.id, Some(false));
            let (result, took_ms) = {
                let Ok(_permit) = self.permits.acquire().await else {
                    fetches.set_state(&source.id, None);
                    return;
                };
                fetches.set_state(&source.id, Some(true));
                let started = Instant::now();
                tracing::debug!(source = %source.id, requested, "fetching");
                let result = self.pipeline.poll(&source).await;
                fetches.set_state(&source.id, None);
                (result, started.elapsed().as_millis() as u64)
            };
            last_poll = Some(Instant::now());
            let interval = interval();
            let (delay, report) = match result {
                Ok(poll) => {
                    failures = 0;
                    tracing::info!(
                        source = %source.id,
                        new = poll.new_items.len(),
                        unchanged = poll.not_modified,
                        requested,
                        took_ms,
                        "fetched"
                    );
                    if !poll.new_items.is_empty() {
                        if poll.was_seeded {
                            self.wake.notify_one();
                        } else {
                            tracing::info!(source = %source.id, "first fetch: stored without notifying");
                        }
                    }
                    let report = FetchReport {
                        new_items: poll.new_items.len(),
                        not_modified: poll.not_modified,
                        error: None,
                    };
                    (interval, report)
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
                    tracing::warn!(source = %source.id, failures, requested, took_ms, retry_in = ?delay, "poll failed: {err:#}");
                    (delay, FetchReport::failed(format!("{err:#}")))
                }
            };
            if let Some(reply) = reply {
                let _ = reply.send(report);
            }
            next = Instant::now() + delay.saturating_add(spread(&source.id, interval / 20));
        }
    }
}

struct Notifier {
    pipeline: Pipeline,
    /// For Discord bot channels.
    bot: Option<BotApi>,
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
        // The search rule needs the search index, so the store answers it for all pending
        // items at once; `notify::accepts` checks the other rules.
        let found = match &channel.rules.search {
            Some(search) => {
                let ids: Vec<i64> = pending.iter().map(|i| i.id).collect();
                let search = search.clone();
                Some(
                    self.pipeline
                        .with_store(move |s| s.matching_search(&ids, &search))
                        .await?,
                )
            }
            None => None,
        };
        let (mut matching, skipped): (Vec<Item>, Vec<Item>) = pending.into_iter().partition(|i| {
            notify::accepts(channel, i) && found.as_ref().is_none_or(|f| f.contains(&i.id))
        });
        self.mark(channel, &skipped).await?;
        let style = match &channel.target {
            ChannelTarget::Discord { style, .. } | ChannelTarget::DiscordBot { style, .. } => {
                Some(*style)
            }
            _ => None,
        };
        if style.is_some_and(notify::discord::shows_images) {
            self.find_images(&mut matching).await;
        }

        let snapshot = self.pipeline.registry.current();
        let ctx = NotifyContext {
            http: &self.pipeline.http,
            sources: &snapshot.config,
            base_url: &self.app.server.base_url,
            discord_bot: self.bot.as_ref(),
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

    /// Looks up the `og:image` of items whose source gave no image, once per item: the
    /// result (also none) is stored, so later runs and other channels reuse it.
    async fn find_images(&self, items: &mut [Item]) {
        for item in items.iter_mut() {
            let Some(url) = item.url.clone() else {
                continue;
            };
            if item.image_url.is_some() || item.image_checked || !worth_an_image_lookup(item, &url)
            {
                continue;
            }
            let image = fetch::og::og_image(&self.pipeline.http, &url).await;
            tracing::info!(
                item = item.id,
                found = image.is_some(),
                "looked up a preview image"
            );
            let (id, stored) = (item.id, image.clone());
            let result = self
                .pipeline
                .with_store(move |s| s.set_item_image(id, stored.as_deref()))
                .await;
            if let Err(err) = result {
                tracing::error!("storing a preview image failed: {err:#}");
            }
            item.image_url = image;
            item.image_checked = true;
        }
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

/// Pages that have no useful preview image: Hacker News threads, and GitHub releases
/// and tags (GitHub's image is a generic repository card).
fn worth_an_image_lookup(item: &Item, url: &str) -> bool {
    !item.source_id.contains(":hn:")
        && !url.starts_with("https://news.ycombinator.com/")
        && !(url.starts_with("https://github.com/")
            && (url.contains("/releases/") || url.contains("/tags")))
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

    /// "Fetch now" makes a poller fetch right away, long before its next poll, and reports
    /// what it found. A request for a switched-off source is answered without fetching.
    #[tokio::test]
    async fn fetches_on_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let feed = include_str!("../tests/fixtures/feed/godot.xml");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8 * 1024];
                let _ = conn.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/rss+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{feed}",
                    feed.len()
                );
                let _ = conn.write_all(reply.as_bytes()).await;
            }
        });

        let mut store = crate::store::Store::open_in_memory().unwrap();
        let config = format!(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            csharp = "first-class"
              [[engine.source]]
              kind = "feed"
              url = "http://{addr}/rss.xml"
              category = "news"
            "#
        );
        let registry = crate::registry::SourceRegistry::with_owners(&mut store, &config);
        let source = registry.current().config.all_sources().remove(0);
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let poller = Poller {
            pipeline: pipeline.clone(),
            permits: Arc::new(Semaphore::new(1)),
            wake: pipeline.notify_wake.clone(),
        };
        let requests = pipeline.fetches.register(&source.id);
        // The first scheduled poll is an hour away.
        let task = tokio::spawn(poller.run(source.clone(), Duration::from_secs(3600), requests));

        let fetch = || async {
            let rx = pipeline
                .fetches
                .request(&source.id)
                .expect("poller is running");
            tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .unwrap()
                .unwrap()
        };
        let first = fetch().await;
        assert_eq!((first.new_items, first.error.as_deref()), (4, None));
        let second = fetch().await;
        assert_eq!(
            (second.new_items, second.error.as_deref()),
            (0, None),
            "already stored"
        );

        pipeline.toggles.set(&source.id, false);
        let off = fetch().await;
        assert!(off.error.unwrap().contains("switched off"));
        assert!(pipeline.fetches.request("unknown").is_none());
        assert_eq!(
            pipeline.fetches.snapshot(),
            ActivitySnapshot::default(),
            "idle after fetching"
        );
        task.abort();
    }

    /// The Sources page sees which sources are queued or fetching, and how far a "Fetch
    /// all now" has got, until it ends.
    #[test]
    fn tracks_fetch_activity() {
        let fetches = FetchRequests::default();
        fetches.set_state("b", Some(true));
        fetches.set_state("a", Some(false));
        fetches.set_state("c", Some(true));
        let snap = fetches.snapshot();
        assert_eq!(snap.fetching, ["b", "c"]);
        assert_eq!(snap.queued, ["a"]);
        fetches.set_state("b", None);
        assert_eq!(fetches.snapshot().fetching, ["c"]);

        let old = fetches.start_batch(5);
        let batch = fetches.start_batch(3);
        batch.finished(Some(&FetchReport {
            new_items: 2,
            not_modified: false,
            error: None,
        }));
        batch.finished(Some(&FetchReport::failed("HTTP 503")));
        batch.finished(None);
        old.finished(None);
        let progress = fetches.snapshot().batch.unwrap();
        assert_eq!(
            (
                progress.total,
                progress.done,
                progress.new_items,
                progress.failed
            ),
            (3, 3, 2, 2),
            "an older batch counts nothing"
        );
        drop(old);
        assert!(
            fetches.snapshot().batch.is_some(),
            "only its own batch ends"
        );
        drop(batch);
        assert!(fetches.snapshot().batch.is_none());
    }

    /// A channel stored in the database (as the website does) is delivered on the next
    /// notifier run, and the result is recorded on the channel.
    #[tokio::test]
    async fn delivers_to_channels_from_the_store() {
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
                image_url: None,
            }])
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let notifier = Notifier {
            pipeline: pipeline.clone(),
            bot: None,
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

    /// A channel with a search rule is sent only the pending items that match it, as the
    /// site's search would; the others are marked handled so they aren't looked at again.
    #[tokio::test]
    async fn delivers_only_items_matching_a_channel_search() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16 * 1024];
                let n = conn.read(&mut buf).await.unwrap();
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                conn.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            }
        });

        let mut store = crate::store::Store::open_in_memory().unwrap();
        let registry = crate::registry::SourceRegistry::load(&store).unwrap();
        let channel = format!(
            r#"{{"name":"phone","kind":"ntfy","server":"http://{addr}","topic":"gen-test",
                "rules":{{"search":"godot shader"}}}}"#
        );
        store.add_channel("phone", &channel).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let item = |guid: &str, title: &str, summary: Option<&str>| crate::model::NewItem {
            source_id: format!("x:feed:{guid}"),
            guid: guid.into(),
            engines: vec!["godot".into()],
            kind: crate::model::ItemKind::News,
            title: title.into(),
            url: None,
            summary: summary.map(Into::into),
            published: None,
            story_key: guid.into(),
            notify: true,
            image_url: None,
        };
        store
            .insert_items(&[
                item("a", "Godot shaders explained", None),
                item("b", "Godot 4.8 released", None),
                item("c", "Weekly roundup", Some("A new GODOT Shader editor")),
                item("d", "Unity shader graph", None),
            ])
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let notifier = Notifier {
            pipeline: pipeline.clone(),
            bot: None,
            app: Arc::new(AppConfig::default()),
            wake: pipeline.notify_wake.clone(),
        };
        notifier.run_once().await;

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(
            requests[0].contains("Godot shaders explained"),
            "{requests:?}"
        );
        assert!(requests[1].contains("Weekly roundup"), "{requests:?}");
        let store = pipeline.store.lock().unwrap();
        let since = chrono::Utc::now() - chrono::Duration::days(1);
        assert!(
            store
                .pending_notifications("phone", since, 10)
                .unwrap()
                .is_empty(),
            "non-matching items are marked handled too"
        );
    }

    /// A Discord bot channel is posted only the items of its subscriptions, each once even
    /// when several subscriptions match, with the bot's token.
    #[tokio::test]
    async fn delivers_subscribed_items_to_bot_channels() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // An HTTP sink that records every request until the test ends.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16 * 1024];
                let n = conn.read(&mut buf).await.unwrap();
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                conn.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                )
                .await
                .unwrap();
            }
        });

        let mut store = crate::store::Store::open_in_memory().unwrap();
        let registry = crate::registry::SourceRegistry::load(&store).unwrap();
        let channel = r#"{"name":"discord-godot","kind":"discordbot","guild_id":1,"channel_id":42,
            "subscriptions":[{"engine":"godot","topic":"releases"},{"engine":"unity","topic":"releases"}]}"#;
        store.add_channel("discord-godot", channel).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let item = |guid: &str, engines: &[&str], kind, title: &str| crate::model::NewItem {
            source_id: format!("x:feed:{guid}"),
            guid: guid.into(),
            engines: engines.iter().map(|e| e.to_string()).collect(),
            kind,
            title: title.into(),
            url: None,
            summary: None,
            published: None,
            story_key: guid.into(),
            notify: true,
            image_url: None,
        };
        use crate::model::ItemKind::{News, Release};
        store
            .insert_items(&[
                item("a", &["godot", "unity"], Release, "Godot and Unity ship"),
                item("b", &["godot"], News, "Godot news, not subscribed"),
                item("c", &["bevy"], Release, "Bevy release, not subscribed"),
            ])
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let notifier = Notifier {
            pipeline: pipeline.clone(),
            bot: Some(BotApi {
                token: "t0ken".into(),
                api_base: format!("http://{addr}"),
            }),
            app: Arc::new(AppConfig::default()),
            wake: pipeline.notify_wake.clone(),
        };
        notifier.run_once().await;
        notifier.run_once().await;

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with("POST /channels/42/messages "));
        assert!(requests[0].contains("Godot and Unity ship"));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: bot t0ken")
        );
        let channels = notify::load_channels(&pipeline.store.lock().unwrap()).unwrap();
        assert!(channels[0].last_ok.is_some() && channels[0].last_error.is_none());
    }

    /// Before posting with an image style, the notifier looks up the item page's
    /// og:image once and stores it; the message carries it.
    #[tokio::test]
    async fn looks_up_preview_images_once() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 32 * 1024];
                let n = conn.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = if request.starts_with("GET /post") {
                    r#"<head><meta property="og:image" content="/hero.png"></head>"#
                } else {
                    "{}"
                };
                seen.lock().unwrap().push(request);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
        });

        let mut store = crate::store::Store::open_in_memory().unwrap();
        let registry = crate::registry::SourceRegistry::load(&store).unwrap();
        let channel = format!(
            r#"{{"name":"chat","kind":"discord","webhook_url":"http://{addr}/hook","style":"polished"}}"#
        );
        store.add_channel("chat", &channel).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let stored = store
            .insert_items(&[crate::model::NewItem {
                source_id: "x:feed:https://example.org/feed".into(),
                guid: "a".into(),
                engines: vec!["godot".into()],
                kind: crate::model::ItemKind::News,
                title: "Godot news".into(),
                url: Some(format!("http://{addr}/post")),
                summary: None,
                published: None,
                story_key: "a".into(),
                notify: true,
                image_url: None,
            }])
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let notifier = Notifier {
            pipeline: pipeline.clone(),
            bot: None,
            app: Arc::new(AppConfig::default()),
            wake: pipeline.notify_wake.clone(),
        };
        notifier.run_once().await;

        let hero = format!("http://{addr}/hero.png");
        let item = pipeline
            .store
            .lock()
            .unwrap()
            .item(stored[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(item.image_url.as_deref(), Some(hero.as_str()));
        assert!(item.image_checked);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "{requests:?}");
            assert!(requests[0].starts_with("GET /post "));
            assert!(requests[1].starts_with("POST /hook?with_components=true "));
            assert!(requests[1].contains(&hero), "{}", requests[1]);
        }

        // Looked up already: no second page fetch.
        let mut again = vec![item];
        again[0].image_url = None;
        notifier.find_images(&mut again).await;
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn skips_image_lookups_for_hacker_news_and_github_releases() {
        let mut item = crate::notify::sample_item();
        assert!(worth_an_image_lookup(
            &item,
            "https://godotengine.org/article/x"
        ));
        assert!(!worth_an_image_lookup(
            &item,
            "https://github.com/godotengine/godot/releases/tag/4.8-stable"
        ));
        item.source_id = "gfs:hn:godot".into();
        assert!(!worth_an_image_lookup(&item, "https://example.org/"));
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

    /// With `retention_days` set, the scheduler prunes old items (at startup, then daily).
    #[tokio::test]
    async fn prunes_items_past_retention() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let registry = crate::registry::SourceRegistry::load(&store).unwrap();
        let item = |guid: &str, days: i64| crate::model::NewItem {
            source_id: "godot:feed".into(),
            guid: guid.into(),
            engines: vec!["godot".into()],
            kind: crate::model::ItemKind::News,
            title: guid.into(),
            url: None,
            summary: None,
            published: Some(chrono::Utc::now() - chrono::Duration::days(days)),
            story_key: String::new(),
            notify: false,
            image_url: None,
        };
        let ago = |days| chrono::Utc::now() - chrono::Duration::days(days);
        store
            .insert_items_at(&[item("old", 400)], ago(400))
            .unwrap();
        store
            .insert_items_at(&[item("recent", 10)], ago(10))
            .unwrap();
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(crate::pipeline::PollSettings::new(60)),
            fetches: Arc::new(FetchRequests::default()),
        };
        let mut app = AppConfig::default();
        app.database.retention_days = Some(365);
        let _tasks = spawn(pipeline.clone(), Arc::new(app));

        let guids = || {
            let store = pipeline.store.lock().unwrap();
            let items = store.query_items(&Default::default()).unwrap();
            items.into_iter().map(|i| i.guid).collect::<Vec<_>>()
        };
        for _ in 0..200 {
            if guids().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(guids(), ["recent"]);
    }
}
