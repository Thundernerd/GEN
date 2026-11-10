//! Health alerts: one message to every channel with `health_alerts` on when a source
//! starts failing.
//!
//! A failed fetch that moves its source into `failing` queues an alert per channel in the
//! store ([`after_fetch_error`]); the notifier sends them ([`deliver`]), so an alert that
//! can't be sent is retried like an item.

use chrono::{DateTime, Duration, Utc};

use super::{NotifyContext, TextMessage};
use crate::config::{ChannelConfig, ResolvedSource, SourceSpec};
use crate::health::{Health, SourceHistory, starts_failing};
use crate::model::SourceState;
use crate::pipeline::Pipeline;
use crate::store::{Store, StoredHealthAlert};

/// Alerts that couldn't be sent for this long are given up on.
const MAX_AGE: Duration = Duration::days(3);

/// After a failed fetch has been recorded: queues an alert for every enabled channel with
/// health alerts on, if the fetch moved the source into `failing`. `before` is the
/// source's state before the fetch. Returns whether any alert was queued.
pub fn after_fetch_error(
    store: &Store,
    source: &ResolvedSource,
    before: &SourceState,
    enabled: bool,
    interval_minutes: u64,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // The newest item only tells ok from stale, which both count as working here.
    let history = |state| SourceHistory {
        state,
        latest_item: None,
    };
    let after = history(store.source_state(&source.id)?);
    let health_before = history(before.clone()).health(enabled, interval_minutes, now);
    let health_after = after.health(enabled, interval_minutes, now);
    if !starts_failing(health_before, health_after) {
        return Ok(false);
    }
    let reason = after.reason(Health::Failing, now);
    let label = describe(source);
    let mut queued = false;
    for channel in super::load_channels(store)? {
        let channel = channel.config;
        if channel.enabled && channel.health_alerts {
            store.add_health_alert(&channel.name, &source.id, &label, &reason)?;
            queued = true;
        }
    }
    Ok(queued)
}

/// Sends a channel's queued alerts, each deleted once sent. A channel that has health
/// alerts off by now drops them. Returns whether anything was sent.
pub async fn deliver(
    pipeline: &Pipeline,
    ctx: &NotifyContext<'_>,
    channel: &ChannelConfig,
) -> anyhow::Result<bool> {
    let name = channel.name.clone();
    let since = Utc::now() - MAX_AGE;
    let pending = pipeline
        .with_store(move |s| s.pending_health_alerts(&name, since))
        .await?;
    let mut sent = false;
    for alert in pending {
        if channel.health_alerts {
            super::send_text(ctx, channel, &message(ctx.base_url, &alert)).await?;
            sent = true;
            tracing::info!(channel = %channel.name, source = %alert.source_id, "sent a health alert");
        }
        pipeline
            .with_store(move |s| s.remove_health_alert(alert.id))
            .await?;
    }
    Ok(sent)
}

/// The alert as a message, linking to the unhealthy sources on the Sources page.
pub fn message(base_url: &str, alert: &StoredHealthAlert) -> TextMessage {
    TextMessage {
        title: "GEN: a source is failing".into(),
        text: format!("GEN: {} is failing ({})", alert.source, alert.reason),
        link: format!(
            "{}/sources?health=unhealthy",
            base_url.trim_end_matches('/')
        ),
    }
}

/// The source as people know it, e.g. "Godot feed https://godotengine.org/rss.xml".
pub fn describe(source: &ResolvedSource) -> String {
    let owner = &source.owner_name;
    match &source.spec {
        SourceSpec::Feed { url, .. } => format!("{owner} feed {url}"),
        SourceSpec::Html { url, .. } => format!("{owner} page {url}"),
        SourceSpec::Github { repo, .. } => format!("{owner} GitHub repository {repo}"),
        SourceSpec::Unity { .. } => "Unity release API".to_string(),
        SourceSpec::Hn { query } => format!("{owner} Hacker News search “{query}”"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    use super::*;
    use crate::config::{ChannelConfig, ChannelTarget};
    use crate::notify::{NotifyContext, load_channels};
    use crate::pipeline::{Pipeline, PollSettings};
    use crate::store::Store;

    /// A feed that answers its first request and returns 404 after that.
    async fn feed_that_breaks() -> std::net::SocketAddr {
        let feed = include_str!("../../tests/fixtures/feed/godot.xml");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for n in 0.. {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8 * 1024];
                let _ = conn.read(&mut buf).await;
                let reply = if n == 0 {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/rss+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{feed}",
                        feed.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .into()
                };
                let _ = conn.write_all(reply.as_bytes()).await;
            }
        });
        addr
    }

    /// An ntfy server that accepts everything and records each request it got.
    async fn recording_ntfy() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                record
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let reply = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                let _ = conn.write_all(reply.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn the_third_failure_in_a_row_alerts_opted_in_channels_once() {
        let feed = feed_that_breaks().await;
        let (ntfy, requests) = recording_ntfy().await;
        let mut store = Store::open_in_memory().unwrap();
        let config = format!(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            csharp = "first-class"
              [[engine.source]]
              kind = "feed"
              url = "http://{feed}/rss.xml"
              category = "news"
            "#
        );
        let registry = crate::registry::SourceRegistry::with_owners(&mut store, &config);
        let source = registry.current().config.all_sources().remove(0);
        for (name, health_alerts) in [("admin", true), ("news", false)] {
            let channel = ChannelConfig {
                name: name.into(),
                enabled: true,
                rules: Default::default(),
                health_alerts,
                target: ChannelTarget::Ntfy {
                    server: ntfy.clone(),
                    topic: name.into(),
                    token: None,
                },
            };
            let json = serde_json::to_string(&channel).unwrap();
            assert!(store.add_channel(name, &json).unwrap());
        }
        let pipeline = Pipeline {
            http: crate::fetch::Http::new("gen-test", None).unwrap(),
            store: Arc::new(Mutex::new(store)),
            registry: Arc::new(registry),
            toggles: Arc::new(crate::toggles::SourceToggles::default()),
            notify_wake: Arc::new(Notify::new()),
            settings: Arc::new(PollSettings::new(60)),
            fetches: Arc::new(crate::scheduler::FetchRequests::default()),
        };
        let snapshot = pipeline.registry.current();
        let ctx = NotifyContext {
            http: &pipeline.http,
            sources: &snapshot.config,
            base_url: "https://gen.example.org/",
            discord_bot: None,
        };
        let deliver_all = || async {
            let channels = pipeline.with_store(|s| load_channels(s)).await.unwrap();
            for channel in channels {
                deliver(&pipeline, &ctx, &channel.config).await.unwrap();
            }
        };

        pipeline.poll(&source).await.unwrap();
        for _ in 0..2 {
            pipeline.poll(&source).await.unwrap_err();
        }
        deliver_all().await;
        assert!(
            requests.lock().unwrap().is_empty(),
            "retrying doesn't alert"
        );

        // The third failure: the source starts failing.
        pipeline.poll(&source).await.unwrap_err();
        deliver_all().await;
        // It stays failing.
        pipeline.poll(&source).await.unwrap_err();
        deliver_all().await;

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "{requests:#?}");
        let alert = &requests[0];
        assert!(alert.starts_with("POST /admin "), "{alert}");
        assert!(
            alert.contains(&format!("Godot feed http://{feed}/rss.xml is failing")),
            "{alert}"
        );
        assert!(
            alert.contains("3 failures in a row, last OK just now: unexpected status 404"),
            "{alert}"
        );
        assert!(
            alert.contains("https://gen.example.org/sources?health=unhealthy"),
            "{alert}"
        );
    }
}
