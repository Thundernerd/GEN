mod catalog;
mod classify;
mod config;
mod discord_bot;
mod fetch;
mod health;
mod model;
mod notify;
mod pipeline;
mod registry;
mod scheduler;
mod store;
mod time_ago;
mod toggles;
mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use tracing_subscriber::EnvFilter;

use crate::config::AppConfig;
use crate::fetch::Http;
use crate::pipeline::Pipeline;
use crate::registry::SourceRegistry;
use crate::store::Store;
use crate::toggles::SourceToggles;

#[derive(Parser)]
#[command(name = "gen", version, about = "GEN — Game Engine News aggregator")]
struct Cli {
    /// App settings (server, database, polling, notifications).
    #[arg(
        long,
        global = true,
        default_value = "config/gen.toml",
        env = "GEN_CONFIG"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server (default).
    Serve,
    /// Poll sources once and print what's new. Sends no notifications.
    Fetch {
        /// Only poll sources whose id contains this text.
        #[arg(long)]
        source: Option<String>,
    },
    /// Validate the config file and list the sources stored in the database.
    CheckConfig,
    /// Add an engine or outlet from the built-in catalog, with its sources, to the database.
    Add {
        /// Catalog id of the engine or outlet, e.g. `bevy` or `gamefromscratch`.
        id: String,
    },
    /// Send a sample item to one notification channel, ignoring its rules.
    NotifyTest {
        /// Channel name, as shown on the Notifications page.
        #[arg(long)]
        channel: String,
        /// Which example to send: release, prerelease, news, outlet, untagged, long, or
        /// `all` for every one at once.
        #[arg(long, default_value = "release")]
        sample: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let app = AppConfig::load(&cli.config)?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::CheckConfig => check_config(&app),
        Command::Add { id } => add(&app, &id),
        Command::Fetch { source } => fetch_once(&app, source.as_deref()).await,
        Command::Serve => serve(app).await,
        Command::NotifyTest { channel, sample } => notify_test(&app, &channel, &sample).await,
    }
}

fn check_config(app: &AppConfig) -> anyhow::Result<()> {
    let pipeline = build_pipeline(app)?;
    let snapshot = pipeline.registry.current();
    let sources = &snapshot.config;
    let (channels, history) = {
        let store = pipeline
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
        (
            notify::load_channels(&store)?,
            health::FetchHistory::load(&store)?,
        )
    };
    let all = sources.all_sources();
    let enabled = sources.enabled_sources();
    println!(
        "{} engines ({} enabled), {} outlets, {} sources ({} enabled), {} notify channels",
        sources.engines.len(),
        sources.enabled_engines().count(),
        sources.outlets.len(),
        all.len(),
        enabled.len(),
        channels.len(),
    );
    for s in &enabled {
        println!("  {}", s.id);
    }

    let now = chrono::Utc::now();
    let unhealthy: Vec<_> = sources
        .manageable_sources()
        .into_iter()
        .filter_map(|s| {
            let history = history.source(&s.id);
            let interval = s
                .interval_minutes
                .unwrap_or_else(|| pipeline.settings.default_interval());
            let health = history.health(pipeline.toggles.is_enabled(&s), interval, now);
            health
                .is_unhealthy()
                .then(|| (s.id, health, history.reason(health, now)))
        })
        .collect();
    if !unhealthy.is_empty() {
        println!("{} unhealthy sources:", unhealthy.len());
        for (id, health, reason) in &unhealthy {
            println!("  {:<8} {id}  ({reason})", health.as_str());
        }
    }
    if let Some(warning) = github_token_warning(app, &pipeline) {
        println!("\nwarning: {warning}");
    }
    Ok(())
}

fn add(app: &AppConfig, id: &str) -> anyhow::Result<()> {
    let pipeline = build_pipeline(app)?;
    let mut store = pipeline
        .store
        .lock()
        .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
    let added = add_catalog_owner(&pipeline.registry, &mut store, id)?;
    println!("added {id} with {} sources", added.len());
    for source in &added {
        // `fetch` skips these; they can be switched on from the Sources page.
        let off = if source.enabled {
            ""
        } else {
            "  (switched off)"
        };
        println!("  {}{off}", source.id);
    }
    Ok(())
}

/// Adds the catalog engine or outlet `id` with its catalog sources, as the Add button under
/// Suggestions does. Returns the added sources, sorted by id.
fn add_catalog_owner(
    registry: &SourceRegistry,
    store: &mut Store,
    id: &str,
) -> anyhow::Result<Vec<config::ResolvedSource>> {
    let owner = catalog::owner(id)
        .ok_or_else(|| anyhow::anyhow!("“{id}” is not a suggested engine or outlet."))?;
    // `apply` refuses this too, but its "is already used" is worded for the web form.
    if registry.current().has_owner(id) {
        anyhow::bail!("“{id}” is already in the database.");
    }
    registry.apply(store, registry::SourceEdit::AddOwner(owner))?;
    let mut added: Vec<_> = registry
        .current()
        .config
        .all_sources()
        .into_iter()
        .filter(|s| s.owner.id() == id)
        .collect();
    added.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(added)
}

/// [`fetch::github::token_warning`] for the sources that are switched on.
fn github_token_warning(app: &AppConfig, pipeline: &Pipeline) -> Option<String> {
    let sources: Vec<_> = pipeline
        .registry
        .current()
        .config
        .manageable_sources()
        .into_iter()
        .filter(|s| pipeline.toggles.is_enabled(s))
        .collect();
    fetch::github::token_warning(
        &sources,
        pipeline.settings.default_interval(),
        app.polling.github_token.is_some(),
    )
}

async fn fetch_once(app: &AppConfig, filter: Option<&str>) -> anyhow::Result<()> {
    let pipeline = build_pipeline(app)?;
    let selected: Vec<_> = pipeline
        .registry
        .current()
        .config
        .manageable_sources()
        .into_iter()
        .filter(|s| pipeline.toggles.is_enabled(s))
        .filter(|s| filter.is_none_or(|f| s.id.contains(f)))
        .collect();
    let results: Vec<_> = futures::stream::iter(selected)
        .map(|s| {
            let pipeline = pipeline.clone();
            async move {
                let res = pipeline.poll(&s).await;
                (s, res)
            }
        })
        .buffer_unordered(app.polling.concurrency.max(1))
        .collect()
        .await;

    let (mut ok, mut failed, mut skipped, mut new_total) = (0, 0, 0, 0);
    for (source, res) in results {
        match res {
            Ok(r) if r.not_modified => {
                ok += 1;
                println!("{:<60} not modified", source.id);
            }
            Ok(r) => {
                ok += 1;
                new_total += r.new_items.len();
                println!("{:<60} {} new", source.id, r.new_items.len());
                for item in &r.new_items {
                    let dup = if item.duplicate_of.is_some() {
                        " (duplicate)"
                    } else {
                        ""
                    };
                    println!(
                        "    [{}] [{}] {}{dup}",
                        item.kind.as_str(),
                        item.engines.join(","),
                        item.title
                    );
                }
            }
            Err(err)
                if matches!(
                    err.downcast_ref::<fetch::FetchError>(),
                    Some(fetch::FetchError::CoolingDown { .. })
                ) =>
            {
                skipped += 1;
                println!("{:<60} skipped: {err:#}", source.id);
            }
            Err(err) => {
                failed += 1;
                println!("{:<60} ERROR {err:#}", source.id);
            }
        }
    }
    let skipped = if skipped > 0 {
        format!(", {skipped} skipped")
    } else {
        String::new()
    };
    println!("\n{ok} ok, {failed} failed{skipped}, {new_total} new items");
    Ok(())
}

async fn notify_test(app: &AppConfig, channel: &str, sample: &str) -> anyhow::Result<()> {
    let samples = notify::samples();
    let items: Vec<_> = samples
        .iter()
        .filter(|s| sample == "all" || s.key == sample)
        .map(|s| s.item.clone())
        .collect();
    if items.is_empty() {
        let keys: Vec<_> = samples.iter().map(|s| s.key).collect();
        anyhow::bail!("no example `{sample}` (try: {}, all)", keys.join(", "));
    }
    let pipeline = build_pipeline(app)?;
    let channels = pipeline.with_store(|s| notify::load_channels(s)).await?;
    let Some(cfg) = channels
        .iter()
        .map(|c| &c.config)
        .find(|c| c.name == channel)
    else {
        let names: Vec<_> = channels.iter().map(|c| c.config.name.as_str()).collect();
        anyhow::bail!(
            "no notify channel named `{channel}` (configured: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        );
    };
    let snapshot = pipeline.registry.current();
    let bot = notify::discord::BotApi::from_config(app);
    let ctx = notify::NotifyContext {
        http: &pipeline.http,
        sources: &snapshot.config,
        base_url: &app.server.base_url,
        discord_bot: bot.as_ref(),
    };
    notify::send(&ctx, &cfg.resolved()?, &items)
        .await
        .with_context(|| format!("sending test notification to `{channel}`"))?;
    println!("sent test notification to `{channel}`");
    Ok(())
}

async fn serve(app: AppConfig) -> anyhow::Result<()> {
    let pipeline = build_pipeline(&app)?;
    if let Some(warning) = github_token_warning(&app, &pipeline) {
        tracing::warn!("{warning}");
    }
    let mut state = web::AppState::new(&pipeline, app)?;
    let bot_status = discord_bot::BotStatusHandle::default();
    // Like the scheduler, the bot stops when `serve` returns.
    let bot = discord_bot::spawn(pipeline.clone(), &state.config, bot_status.clone())
        .map(|task| AbortOnDrop(task.abort_handle()));
    if bot.is_some() {
        state.bot = Some(bot_status);
    }
    let listener = tokio::net::TcpListener::bind(&state.config.server.bind)
        .await
        .with_context(|| format!("binding {}", state.config.server.bind))?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    // Dropping the JoinSet when `serve` returns aborts the polling tasks.
    let _scheduler = scheduler::spawn(pipeline, state.config.clone());
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shut down");
    Ok(())
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Resolves on Ctrl-C, or on SIGTERM (sent by `docker stop` and `systemctl stop`).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Opens the database (seeding the default engines into a new one) and loads the sources.
fn build_pipeline(app: &AppConfig) -> anyhow::Result<Pipeline> {
    let mut store = Store::open(&app.database.path)?;
    let registry = SourceRegistry::load(&store)?;
    catalog::seed_once(&registry, &mut store)?;
    catalog::backfill_icons_once(&registry, &mut store)?;
    notify::import_once(app, &store)?;
    let toggles = SourceToggles::new(store.source_overrides()?);
    // The default interval set on the website wins over gen.toml.
    let default_interval = match store.meta(pipeline::DEFAULT_INTERVAL_KEY)? {
        Some(v) => v.parse().unwrap_or(app.polling.default_interval_minutes),
        None => app.polling.default_interval_minutes,
    };
    Ok(Pipeline {
        http: Http::new(&app.polling.user_agent, app.polling.github_token.clone())?,
        store: Arc::new(Mutex::new(store)),
        registry: Arc::new(registry),
        toggles: Arc::new(toggles),
        notify_wake: Arc::new(tokio::sync::Notify::new()),
        settings: Arc::new(pipeline::PollSettings::new(default_interval)),
        fetches: Arc::new(scheduler::FetchRequests::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_registry() -> (SourceRegistry, Store) {
        let store = Store::open_in_memory().unwrap();
        (SourceRegistry::load(&store).unwrap(), store)
    }

    #[test]
    fn adding_a_catalog_engine_stores_it_with_its_catalog_sources() {
        let (registry, mut store) = empty_registry();

        let added: Vec<_> = add_catalog_owner(&registry, &mut store, "bevy")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();

        assert_eq!(
            added,
            [
                "bevy:feed:https://bevy.org/atom.xml",
                "bevy:github:bevyengine/bevy"
            ]
        );
        // Stored, not just published: a fresh load from the same database sees it.
        let reloaded = SourceRegistry::load(&store).unwrap().current();
        assert!(reloaded.has_owner("bevy"));
        let mut ids: Vec<_> = reloaded
            .config
            .all_sources()
            .into_iter()
            .filter(|s| s.owner.id() == "bevy")
            .map(|s| s.id)
            .collect();
        ids.sort();
        assert_eq!(ids, added);
    }

    #[test]
    fn adding_an_unknown_id_fails_and_stores_nothing() {
        let (registry, mut store) = empty_registry();

        let err = add_catalog_owner(&registry, &mut store, "no-such-engine").unwrap_err();

        assert_eq!(
            err.to_string(),
            "“no-such-engine” is not a suggested engine or outlet."
        );
        assert!(
            !SourceRegistry::load(&store)
                .unwrap()
                .current()
                .has_owner("no-such-engine")
        );
    }

    #[test]
    fn adding_an_owner_that_is_already_present_fails() {
        let (registry, mut store) = empty_registry();
        add_catalog_owner(&registry, &mut store, "gamefromscratch").unwrap();

        let err = add_catalog_owner(&registry, &mut store, "gamefromscratch").unwrap_err();

        assert_eq!(
            err.to_string(),
            "“gamefromscratch” is already in the database."
        );
    }
}
