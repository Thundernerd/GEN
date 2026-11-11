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
use crate::pipeline::Pipeline;
use crate::store::{Store, StoredHealthAlert};

/// Alerts that couldn't be sent for this long are given up on.
const MAX_AGE: Duration = Duration::days(3);

/// Records a failed fetch of `source` and, if it moved the source into `failing`, queues
/// an alert for every enabled channel with health alerts on. Returns whether any alert
/// was queued.
pub fn after_fetch_error(
    store: &Store,
    source: &ResolvedSource,
    error: &str,
    enabled: bool,
    interval_minutes: u64,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // The newest item only tells ok from stale, which both count as working here.
    let history = |state| SourceHistory {
        state,
        latest_item: None,
    };
    let before = store.source_state(&source.id)?;
    store.record_fetch_error(&source.id, error)?;
    let after = history(store.source_state(&source.id)?);
    let health_before = match history(before.clone()).health(enabled, interval_minutes, now) {
        // Its last fetch worked; it only looks failing because nothing fetched it for days
        // (GEN was down, or the source switched off).
        Health::Failing if before.consecutive_failures == 0 && before.last_ok.is_some() => {
            Health::Ok
        }
        health => health,
    };
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
            store.add_health_alert(&channel.name, &source.id, &label, &reason, now)?;
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
        text: format!("{} is failing ({})", alert.source_label, alert.reason),
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

    use tokio::sync::Notify;

    use super::*;
    use crate::config::{ChannelConfig, ChannelTarget};
    use crate::notify::{NotifyContext, load_channels};
    use crate::pipeline::{Pipeline, PollSettings};
    use crate::store::Store;
    use crate::test_server::{ok, respond, serve};

    /// A feed that answers its first request and returns 404 after that.
    async fn feed_that_breaks() -> std::net::SocketAddr {
        let feed = include_str!("../../tests/fixtures/feed/godot.xml");
        serve(move |n, _| {
            if n == 0 {
                respond("200 OK", &[("content-type", "application/rss+xml")], feed)
            } else {
                respond("404 Not Found", &[], "")
            }
        })
        .await
        .addr
    }

    fn feed_source() -> ResolvedSource {
        let config: crate::config::SourcesConfig = toml::from_str(
            r#"
            [[engine]]
            id = "godot"
            name = "Godot"
            tier = 1
            languages = ["GDScript", "C#", "C++"]
              [[engine.source]]
              kind = "feed"
              url = "https://godotengine.org/rss.xml"
            "#,
        )
        .unwrap();
        config.all_sources().remove(0)
    }

    fn store_with_alert_channel() -> Store {
        let store = Store::open_in_memory().unwrap();
        let channel: ChannelConfig = toml::from_str(
            r#"
            name = "admin"
            kind = "ntfy"
            topic = "admin"
            health_alerts = true
            "#,
        )
        .unwrap();
        let json = serde_json::to_string(&channel).unwrap();
        store.add_channel("admin", &json).unwrap();
        store
    }

    /// GEN was down (or the source switched off) for days: the source last fetched fine
    /// days ago and only looks failing because nothing fetched it since. Its first real
    /// failure is a change to failing.
    #[test]
    fn the_first_failure_after_a_pause_alerts() {
        let store = store_with_alert_channel();
        let source = feed_source();
        store.record_fetch_ok(&source.id, None, None).unwrap();
        let later = Utc::now() + Duration::days(5);
        assert!(after_fetch_error(&store, &source, "HTTP 404", true, 60, later).unwrap());
        let alerts = store
            .pending_health_alerts("admin", Utc::now() - Duration::days(1))
            .unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(
            alerts[0].reason,
            "1 failure in a row, last OK 5 days ago: HTTP 404"
        );

        // It stays failing.
        assert!(!after_fetch_error(&store, &source, "HTTP 404", true, 60, later).unwrap());
    }

    #[tokio::test]
    async fn the_third_failure_in_a_row_alerts_opted_in_channels_once() {
        let feed = feed_that_breaks().await;
        // An ntfy server that accepts everything.
        let ntfy_server = serve(|_, _| ok("{}")).await;
        let ntfy = ntfy_server.url();
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
        assert!(ntfy_server.requests().is_empty(), "retrying doesn't alert");

        // The third failure: the source starts failing.
        pipeline.poll(&source).await.unwrap_err();
        deliver_all().await;
        // It stays failing.
        pipeline.poll(&source).await.unwrap_err();
        deliver_all().await;

        let requests = ntfy_server.requests();
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
