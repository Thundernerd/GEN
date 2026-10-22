//! HTTP server: HTML pages, the combined Atom feed and a JSON API.
//!
//! Templates are loaded at runtime from `server.templates_dir` (and reloaded when they
//! change), so the look can be tweaked without recompiling.

mod feed;
mod queries;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use chrono::Utc;
use minijinja::Environment;
use minijinja::value::Serde;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

use crate::config::{AppConfig, Owner, SourceSpec, SourcesConfig};
use crate::model::{Item, ItemKind, SourceState};
use crate::store::Store;

use self::queries::{EngineStats, ItemFilter};
use self::view::{DateView, ItemView, Lookup, query_string};

/// Items per HTML page.
const PAGE_SIZE: u32 = 50;
/// Items in `/feed.xml`.
const FEED_SIZE: u32 = 100;
/// Default and maximum `limit` for `/api/items`.
const API_DEFAULT_LIMIT: u32 = 50;
const API_MAX_LIMIT: u32 = 200;

const TIER_LABELS: [(u8, &str); 3] = [
    (1, "Major C# engines"),
    (2, "Niche C# engines"),
    (3, "Other notable engines"),
];

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub sources: Arc<SourcesConfig>,
    pub config: Arc<AppConfig>,
    templates: Arc<Environment<'static>>,
    lookup: Arc<Lookup>,
}

impl AppState {
    /// Builds the state and sets up the template environment from `server.templates_dir`.
    pub fn new(
        store: Arc<Mutex<Store>>,
        sources: SourcesConfig,
        config: AppConfig,
    ) -> anyhow::Result<Self> {
        let dir = config.server.templates_dir.clone();
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(&dir));
        // Fail at startup rather than on the first request when the directory is wrong.
        env.get_template("base.html")
            .with_context(|| format!("loading templates from {}", dir.display()))?;
        Ok(AppState {
            store,
            lookup: Arc::new(Lookup::new(&sources)),
            sources: Arc::new(sources),
            config: Arc::new(config),
            templates: Arc::new(env),
        })
    }

    /// Runs a read on the blocking pool (see `Pipeline::with_store`).
    async fn with_store<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let guard = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            f(&guard)
        })
        .await?
    }

    /// Lists items matching `filter` together with each item's duplicate source ids.
    async fn items_with_dups(
        &self,
        filter: ItemFilter,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<(Vec<Item>, HashMap<i64, Vec<String>>)> {
        let (sources, lookup) = (self.sources.clone(), self.lookup.clone());
        self.with_store(move |s| {
            let items = queries::list_items(s, &sources, &lookup, &filter, limit, offset)?;
            let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
            let dups = s.duplicate_sources(&ids)?;
            Ok((items, dups))
        })
        .await
    }

    fn base_url(&self) -> &str {
        self.config.server.base_url.trim_end_matches('/')
    }

    fn render(&self, name: &str, ctx: serde_json::Value) -> Result<Response, AppError> {
        let html = self
            .templates
            .get_template(name)
            .and_then(|t| t.render(Serde(&ctx)))
            .with_context(|| format!("rendering {name}"))?;
        Ok(Html(html).into_response())
    }

    /// Turns a handler result into a response, rendering `error.html` on failure.
    fn respond(&self, result: Result<Response, AppError>) -> Response {
        let err = match result {
            Ok(resp) => return resp,
            Err(err) => err,
        };
        let (status, message) = match &err {
            AppError::NotFound(what) => (StatusCode::NOT_FOUND, what.clone()),
            AppError::Internal(e) => {
                tracing::error!("request failed: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong on our side.".to_string(),
                )
            }
        };
        let ctx = json!({
            "nav": "",
            "status": status.as_u16(),
            "reason": status.canonical_reason().unwrap_or("Error"),
            "message": message,
        });
        match self.render("error.html", ctx) {
            Ok(resp) => (status, resp).into_response(),
            Err(AppError::Internal(e)) => {
                tracing::error!("rendering error page failed: {e:#}");
                (status, message).into_response()
            }
            Err(AppError::NotFound(_)) => (status, message).into_response(),
        }
    }

    fn item_views(&self, items: &[Item], dups: &HashMap<i64, Vec<String>>) -> Vec<ItemView> {
        let now = Utc::now();
        items
            .iter()
            .map(|i| ItemView::new(i, &self.lookup, dups, now))
            .collect()
    }
}

enum AppError {
    NotFound(String),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        AppError::Internal(err.into())
    }
}

type PageResult = Result<Response, AppError>;

pub fn router(state: AppState) -> Router {
    let static_files = ServeDir::new(&state.config.server.static_dir);
    Router::new()
        .route("/", get(index))
        .route("/engines", get(engines))
        .route("/engine/{id}", get(engine))
        .route("/sources", get(sources))
        .route("/feed.xml", get(feed))
        .route("/api/items", get(api_items))
        .route("/health", get(health))
        .nest_service("/static", static_files)
        .fallback(not_found)
        .layer(CompressionLayer::new())
        .with_state(state)
}

// -- query parameters ---------------------------------------------------------

/// Raw list query parameters. Everything is a string so bad values are ignored, not 400s.
#[derive(Debug, Default, Deserialize)]
struct ListParams {
    engine: Option<String>,
    kind: Option<String>,
    tier: Option<String>,
    csharp: Option<String>,
    page: Option<String>,
    limit: Option<String>,
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl ListParams {
    fn filter(&self) -> ItemFilter {
        ItemFilter {
            engine: non_empty(&self.engine),
            kind: non_empty(&self.kind).and_then(|k| ItemKind::parse(&k)),
            tier: non_empty(&self.tier).and_then(|t| t.parse().ok()),
            csharp: matches!(
                non_empty(&self.csharp).as_deref(),
                Some("1" | "on" | "true" | "yes")
            ),
        }
    }

    /// 1-based page number.
    fn page(&self) -> u32 {
        non_empty(&self.page)
            .and_then(|p| p.parse().ok())
            .unwrap_or(1)
            .max(1)
    }
}

/// Query string pairs that reproduce a filter, for links that keep the current filters.
fn filter_pairs(f: &ItemFilter) -> Vec<(&'static str, Option<String>)> {
    vec![
        ("engine", f.engine.clone()),
        ("kind", f.kind.map(|k| k.as_str().to_string())),
        ("tier", f.tier.map(|t| t.to_string())),
        ("csharp", f.csharp.then(|| "1".to_string())),
    ]
}

/// Previous/next page links for list pages.
fn pagination(
    path: &str,
    pairs: &[(&str, Option<String>)],
    page: u32,
    has_more: bool,
) -> serde_json::Value {
    let link = |p: u32| {
        let mut all = pairs.to_vec();
        all.push(("page", (p > 1).then(|| p.to_string())));
        format!("{path}{}", query_string(&all))
    };
    json!({
        "page": page,
        "prev": (page > 1).then(|| link(page - 1)),
        "next": has_more.then(|| link(page + 1)),
    })
}

fn kind_names() -> Vec<&'static str> {
    ItemKind::ALL.iter().map(|k| k.as_str()).collect()
}

// -- handlers -----------------------------------------------------------------

async fn health() -> &'static str {
    "ok"
}

async fn not_found(State(st): State<AppState>) -> Response {
    st.respond(Err(AppError::NotFound(
        "There is no page at this address.".to_string(),
    )))
}

async fn index(State(st): State<AppState>, Query(params): Query<ListParams>) -> Response {
    let result = index_page(&st, params).await;
    st.respond(result)
}

async fn index_page(st: &AppState, params: ListParams) -> PageResult {
    let filter = params.filter();
    let page = params.page();
    let (mut items, dups) = st
        .items_with_dups(filter.clone(), PAGE_SIZE + 1, (page - 1) * PAGE_SIZE)
        .await?;
    let has_more = items.len() > PAGE_SIZE as usize;
    items.truncate(PAGE_SIZE as usize);

    let pairs = filter_pairs(&filter);
    let engines: Vec<_> = st
        .sources
        .enabled_engines()
        .map(|e| json!({"id": e.id, "name": e.name, "tier": e.tier}))
        .collect();
    let feed_pairs: Vec<_> = pairs[..2].to_vec();
    st.render(
        "index.html",
        json!({
            "nav": "latest",
            "items": st.item_views(&items, &dups),
            "engines": engines,
            "kinds": kind_names(),
            "filter": {
                "engine": filter.engine,
                "kind": filter.kind.map(ItemKind::as_str),
                "tier": filter.tier,
                "csharp": filter.csharp,
                "active": filter.engine.is_some() || filter.kind.is_some()
                    || filter.tier.is_some() || filter.csharp,
            },
            "pagination": pagination("/", &pairs, page, has_more),
            "feed_url": format!("/feed.xml{}", query_string(&feed_pairs)),
        }),
    )
}

async fn engines(State(st): State<AppState>) -> Response {
    let result = engines_page(&st).await;
    st.respond(result)
}

async fn engines_page(st: &AppState) -> PageResult {
    let stats = st.with_store(queries::engine_stats).await?;
    let now = Utc::now();
    let tiers: Vec<_> = TIER_LABELS
        .iter()
        .map(|&(tier, label)| {
            let engines: Vec<_> = st
                .sources
                .enabled_engines()
                .filter(|e| e.tier == tier)
                .map(|e| {
                    let s = stats.get(&e.id).cloned().unwrap_or(EngineStats {
                        items: 0,
                        latest: None,
                    });
                    json!({
                        "id": e.id,
                        "name": e.name,
                        "csharp": e.csharp,
                        "homepage": e.homepage,
                        "items": s.items,
                        "latest": s.latest.map(|d| DateView::new(d, now)),
                    })
                })
                .collect();
            json!({"tier": tier, "label": label, "engines": engines})
        })
        .collect();
    st.render("engines.html", json!({"nav": "engines", "tiers": tiers}))
}

async fn engine(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Response {
    let result = engine_page(&st, id, params).await;
    st.respond(result)
}

async fn engine_page(st: &AppState, id: String, params: ListParams) -> PageResult {
    let Some(engine) = st.sources.engine(&id) else {
        return Err(AppError::NotFound(format!("Unknown engine “{id}”.")));
    };
    let filter = ItemFilter {
        engine: Some(engine.id.clone()),
        kind: params.filter().kind,
        ..Default::default()
    };
    let page = params.page();
    let (mut items, dups) = st
        .items_with_dups(filter.clone(), PAGE_SIZE + 1, (page - 1) * PAGE_SIZE)
        .await?;
    let has_more = items.len() > PAGE_SIZE as usize;
    items.truncate(PAGE_SIZE as usize);

    let states = st.with_store(|s| s.source_states()).await?;
    let now = Utc::now();
    let sources: Vec<_> = st
        .sources
        .enabled_sources()
        .into_iter()
        .filter(|s| s.owner == Owner::Engine(engine.id.clone()))
        .map(|s| source_row(st, &s, states.get(&s.id), now))
        .collect();

    let base = format!("/engine/{}", engine.id);
    let tabs: Vec<_> = std::iter::once(None)
        .chain(ItemKind::ALL.into_iter().map(Some))
        .map(|k| {
            json!({
                "label": k.map_or("all", ItemKind::as_str),
                "url": format!("{base}{}", query_string(&[("kind", k.map(|k| k.as_str().to_string()))])),
                "active": k == filter.kind,
            })
        })
        .collect();
    let kind_pair = [("kind", filter.kind.map(|k| k.as_str().to_string()))];
    st.render(
        "engine.html",
        json!({
            "nav": "engines",
            "engine": {
                "id": engine.id,
                "name": engine.name,
                "tier": engine.tier,
                "tier_label": TIER_LABELS.iter().find(|t| t.0 == engine.tier).map(|t| t.1),
                "csharp": engine.csharp,
                "homepage": engine.homepage,
                "enabled": engine.enabled,
            },
            "sources": sources,
            "tabs": tabs,
            "items": st.item_views(&items, &dups),
            "pagination": pagination(&base, &kind_pair, page, has_more),
            "feed_url": format!("/feed.xml?engine={}", url::form_urlencoded::byte_serialize(engine.id.as_bytes()).collect::<String>()),
        }),
    )
}

async fn sources(State(st): State<AppState>) -> Response {
    let result = sources_page(&st).await;
    st.respond(result)
}

async fn sources_page(st: &AppState) -> PageResult {
    let states = st.with_store(|s| s.source_states()).await?;
    let now = Utc::now();
    let rows: Vec<_> = st
        .sources
        .enabled_sources()
        .iter()
        .map(|s| source_row(st, s, states.get(&s.id), now))
        .collect();
    let count = |status: &str| rows.iter().filter(|r| r["status"] == status).count();
    let summary = json!({
        "total": rows.len(),
        "ok": count("ok"),
        "failing": count("failing"),
        "pending": count("pending"),
    });
    st.render(
        "sources.html",
        json!({"nav": "sources", "sources": rows, "summary": summary}),
    )
}

/// One source with its health, for the engine and sources pages.
fn source_row(
    st: &AppState,
    s: &crate::config::ResolvedSource,
    state: Option<&SourceState>,
    now: chrono::DateTime<Utc>,
) -> serde_json::Value {
    let state = state.cloned().unwrap_or_default();
    let status = if state.consecutive_failures > 0 {
        "failing"
    } else if state.last_ok.is_some() {
        "ok"
    } else {
        "pending"
    };
    let (kind, target) = match &s.spec {
        SourceSpec::Feed { url, .. } => ("feed", url.clone()),
        SourceSpec::Github { repo, mode, .. } => (
            "github",
            format!("{repo} ({})", format!("{mode:?}").to_lowercase()),
        ),
        SourceSpec::Unity { .. } => ("unity", "Unity releases API".to_string()),
        SourceSpec::Hn { query } => ("hn", query.clone()),
        SourceSpec::Html { url, .. } => ("html", url.clone()),
    };
    let target_url = match &s.spec {
        SourceSpec::Feed { url, .. } | SourceSpec::Html { url, .. } => Some(url.clone()),
        SourceSpec::Github { repo, .. } => Some(format!("https://github.com/{repo}")),
        _ => None,
    };
    let (owner_kind, owner_id) = match &s.owner {
        Owner::Engine(id) => ("engine", id.clone()),
        Owner::Outlet(id) => ("outlet", id.clone()),
    };
    json!({
        "id": s.id,
        "owner": s.owner_name,
        "owner_kind": owner_kind,
        "owner_id": owner_id,
        "kind": kind,
        "target": target,
        "target_url": target_url,
        "category": s.category.map(ItemKind::as_str),
        "interval": s.interval_minutes.unwrap_or(st.config.polling.default_interval_minutes),
        "status": status,
        "last_ok": state.last_ok.map(|d| DateView::new(d, now)),
        "last_error": state.last_error,
        "failures": state.consecutive_failures,
    })
}

async fn feed(State(st): State<AppState>, Query(params): Query<ListParams>) -> Response {
    let result = feed_response(&st, params).await;
    st.respond(result)
}

async fn feed_response(st: &AppState, params: ListParams) -> PageResult {
    let full = params.filter();
    let filter = ItemFilter {
        engine: full.engine,
        kind: full.kind,
        ..Default::default()
    };
    let (items, _) = st.items_with_dups(filter.clone(), FEED_SIZE, 0).await?;
    let mut title = "GEN — Game Engine News".to_string();
    if let Some(e) = &filter.engine {
        title.push_str(&format!(" — {}", st.lookup.engine_name(e)));
    }
    if let Some(k) = filter.kind {
        title.push_str(&format!(" ({})", k.as_str()));
    }
    let query = query_string(&filter_pairs(&filter));
    let feed = feed::build(&items, &st.lookup, st.base_url(), &query, &title);
    Ok((
        [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
        feed.to_string(),
    )
        .into_response())
}

/// An item in `/api/items`.
#[derive(Serialize)]
struct ApiItem<'a> {
    #[serde(flatten)]
    item: &'a Item,
    /// Owner names of other sources that carried the same story.
    also_on: Vec<String>,
}

async fn api_items(State(st): State<AppState>, Query(params): Query<ListParams>) -> Response {
    let result = api_items_response(&st, params).await;
    st.respond(result)
}

async fn api_items_response(st: &AppState, params: ListParams) -> PageResult {
    let limit = non_empty(&params.limit)
        .and_then(|l| l.parse::<u32>().ok())
        .unwrap_or(API_DEFAULT_LIMIT)
        .clamp(1, API_MAX_LIMIT);
    let page = params.page();
    let (items, dups) = st
        .items_with_dups(params.filter(), limit, (page - 1) * limit)
        .await?;
    let out: Vec<_> = items
        .iter()
        .map(|item| ApiItem {
            item,
            also_on: st.lookup.also_on(item, dups.get(&item.id)),
        })
        .collect();
    Ok(Json(json!({"items": out, "page": page, "limit": limit})).into_response())
}
