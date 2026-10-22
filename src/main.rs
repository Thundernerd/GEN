// Temporary while modules are stubbed out; remove once the scheduler wires everything up.
#![allow(dead_code)]

mod classify;
mod config;
mod fetch;
mod model;
mod notify;
mod pipeline;
mod store;
mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use tracing_subscriber::EnvFilter;

use crate::classify::Classifier;
use crate::config::{AppConfig, SourcesConfig};
use crate::fetch::Http;
use crate::pipeline::Pipeline;
use crate::store::Store;

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
    /// Engines, outlets and their sources.
    #[arg(
        long,
        global = true,
        default_value = "config/sources.toml",
        env = "GEN_SOURCES"
    )]
    sources: PathBuf,
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
    /// Validate the config files and list the sources.
    CheckConfig,
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
    let sources = SourcesConfig::load(&cli.sources)?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::CheckConfig => check_config(&app, &sources),
        Command::Fetch { source } => fetch_once(&app, &sources, source.as_deref()).await,
        Command::Serve => serve(app, sources).await,
    }
}

fn check_config(app: &AppConfig, sources: &SourcesConfig) -> anyhow::Result<()> {
    let all = sources.all_sources();
    let enabled = sources.enabled_sources();
    println!(
        "{} engines ({} enabled), {} outlets, {} sources ({} enabled), {} notify channels",
        sources.engines.len(),
        sources.enabled_engines().count(),
        sources.outlets.len(),
        all.len(),
        enabled.len(),
        app.notify.channels.len(),
    );
    for s in &enabled {
        println!("  {}", s.id);
    }
    Ok(())
}

async fn fetch_once(
    app: &AppConfig,
    sources: &SourcesConfig,
    filter: Option<&str>,
) -> anyhow::Result<()> {
    let pipeline = build_pipeline(app, sources)?;
    let selected: Vec<_> = sources
        .enabled_sources()
        .into_iter()
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

    let (mut ok, mut failed, mut new_total) = (0, 0, 0);
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
            Err(err) => {
                failed += 1;
                println!("{:<60} ERROR {err:#}", source.id);
            }
        }
    }
    println!("\n{ok} ok, {failed} failed, {new_total} new items");
    Ok(())
}

async fn serve(app: AppConfig, sources: SourcesConfig) -> anyhow::Result<()> {
    let pipeline = build_pipeline(&app, &sources)?;
    let state = web::AppState::new(pipeline.store.clone(), sources, app)?;
    let listener = tokio::net::TcpListener::bind(&state.config.server.bind)
        .await
        .with_context(|| format!("binding {}", state.config.server.bind))?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn build_pipeline(app: &AppConfig, sources: &SourcesConfig) -> anyhow::Result<Pipeline> {
    let store = Store::open(&app.database.path)?;
    Ok(Pipeline {
        http: Http::new(&app.polling.user_agent, app.polling.github_token.clone())?,
        store: Arc::new(Mutex::new(store)),
        classifier: Arc::new(Classifier::new(sources)),
    })
}
