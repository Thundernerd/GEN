//! HTTP server: HTML pages, the combined Atom feed and a JSON API.
//!
//! Templates are loaded at runtime from `server.templates_dir` (and reloaded when they
//! change), so the look can be tweaked without recompiling.

mod admin;
mod feed;
mod notifications;
mod queries;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use chrono::Utc;
use minijinja::Environment;
use minijinja::value::Serde;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

use crate::catalog;
use crate::config::{AppConfig, Owner, SourceSpec};
use crate::fetch::Http;
use crate::model::{Item, ItemKind, SourceState};
use crate::pipeline::Pipeline;
use crate::registry::{NewOwner, Snapshot, SourceRegistry};
use crate::store::Store;
use crate::toggles::SourceToggles;

use self::queries::{EngineStats, ItemFilter};
use self::view::{DateView, ItemView, query_string};

/// Items per HTML page.
const PAGE_SIZE: u32 = 50;
/// Items in `/feed.xml`.
const FEED_SIZE: u32 = 100;
/// Default and maximum `limit` for `/api/items`.
const API_DEFAULT_LIMIT: u32 = 50;
const API_MAX_LIMIT: u32 = 200;

/// Cookie holding a hash of the admin token once logged in.
const ADMIN_COOKIE: &str = "gen_admin";
const ADMIN_COOKIE_MAX_AGE: u32 = 30 * 24 * 60 * 60;

const TIER_LABELS: [(u8, &str); 3] = [
    (1, "Major C# engines"),
    (2, "Niche C# engines"),
    (3, "Other notable engines"),
];

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub registry: Arc<SourceRegistry>,
    pub config: Arc<AppConfig>,
    pub toggles: Arc<SourceToggles>,
    /// For sending test notifications.
    pub http: Http,
    /// Wakes the notifier after channels change.
    pub notify_wake: Arc<tokio::sync::Notify>,
    templates: Arc<Environment<'static>>,
}

impl AppState {
    /// Builds the state and sets up the template environment from `server.templates_dir`.
    pub fn new(pipeline: &Pipeline, config: AppConfig) -> anyhow::Result<Self> {
        let dir = config.server.templates_dir.clone();
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(&dir));
        // Fail at startup rather than on the first request when the directory is wrong.
        env.get_template("base.html")
            .with_context(|| format!("loading templates from {}", dir.display()))?;
        Ok(AppState {
            store: pipeline.store.clone(),
            registry: pipeline.registry.clone(),
            config: Arc::new(config),
            toggles: pipeline.toggles.clone(),
            http: pipeline.http.clone(),
            notify_wake: pipeline.notify_wake.clone(),
            templates: Arc::new(env),
        })
    }

    /// Runs a write on the blocking pool.
    async fn with_store_mut<T, F>(&self, f: F) -> anyhow::Result<T>
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
        let snapshot = self.registry.current();
        self.with_store(move |s| {
            let items = queries::list_items(
                s,
                &snapshot.config,
                &snapshot.lookup,
                &filter,
                limit,
                offset,
            )?;
            let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
            let dups = s.duplicate_sources(&ids)?;
            Ok((items, dups))
        })
        .await
    }

    fn admin_token(&self) -> Option<&str> {
        self.config
            .server
            .admin_token
            .as_deref()
            .filter(|t| !t.is_empty())
    }

    /// Whether the request carries a valid admin cookie.
    fn is_admin(&self, headers: &HeaderMap) -> bool {
        let Some(token) = self.admin_token() else {
            return false;
        };
        cookie(headers, ADMIN_COOKIE)
            .and_then(|v| blake3::Hash::from_hex(v).ok())
            // `blake3::Hash` equality is constant-time.
            .is_some_and(|h| h == admin_cookie_hash(token))
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
            AppError::Forbidden(what) => (StatusCode::FORBIDDEN, what.clone()),
            AppError::Invalid(what) => (StatusCode::UNPROCESSABLE_ENTITY, what.clone()),
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
            Err(AppError::NotFound(_) | AppError::Forbidden(_) | AppError::Invalid(_)) => {
                (status, message).into_response()
            }
        }
    }

    fn item_views(&self, items: &[Item], dups: &HashMap<i64, Vec<String>>) -> Vec<ItemView> {
        let now = Utc::now();
        let snapshot = self.registry.current();
        items
            .iter()
            .map(|i| ItemView::new(i, &snapshot.lookup, dups, now))
            .collect()
    }
}

enum AppError {
    NotFound(String),
    Forbidden(String),
    /// A refused change; the message is for the user.
    Invalid(String),
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
        .route("/sources/toggle", post(toggle_source))
        .route("/sources/add", post(admin::add_source))
        .route("/sources/remove", post(admin::remove_source))
        .route("/suggestions/source", post(admin::add_suggested_source))
        .route("/owners/add", post(admin::add_owner))
        .route("/owners/remove", post(admin::remove_owner))
        .route("/suggestions/owner", post(admin::add_suggested_owner))
        .route("/notifications", get(notifications::page))
        .route("/notifications/add", post(notifications::add))
        .route("/notifications/edit", post(notifications::edit))
        .route("/notifications/toggle", post(notifications::toggle))
        .route("/notifications/remove", post(notifications::remove))
        .route("/notifications/test", post(notifications::test))
        .route("/login", get(login_form).post(login))
        .route("/logout", post(logout))
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

async fn index(
    State(st): State<AppState>,
    Query(params): Query<ListParams>,
    headers: HeaderMap,
) -> Response {
    let result = index_page(&st, params, st.is_admin(&headers)).await;
    st.respond(result)
}

async fn index_page(st: &AppState, params: ListParams, admin: bool) -> PageResult {
    let filter = params.filter();
    let page = params.page();
    let (mut items, dups) = st
        .items_with_dups(
            filter.clone(),
            PAGE_SIZE + 1,
            page.saturating_sub(1).saturating_mul(PAGE_SIZE),
        )
        .await?;
    let has_more = items.len() > PAGE_SIZE as usize;
    items.truncate(PAGE_SIZE as usize);

    let pairs = filter_pairs(&filter);
    // Filter links that change one filter and keep the others.
    let link = |key: &str, value: Option<String>| {
        let mut changed = pairs.clone();
        if let Some(pair) = changed.iter_mut().find(|(k, _)| *k == key) {
            pair.1 = value;
        }
        format!("/{}", query_string(&changed))
    };
    let kind_tabs: Vec<_> = std::iter::once(None)
        .chain(ItemKind::ALL.into_iter().map(Some))
        .map(|k| {
            json!({
                "kind": k.map(ItemKind::as_str),
                "label": k.map_or("all", ItemKind::as_str),
                "url": link("kind", k.map(|k| k.as_str().to_string())),
                "active": k == filter.kind,
            })
        })
        .collect();
    let snapshot = st.registry.current();
    let engine_links: Vec<_> = std::iter::once(None)
        .chain(snapshot.config.enabled_engines().map(Some))
        .map(|e| {
            json!({
                "label": e.map_or("all engines", |e| e.name.as_str()),
                "url": link("engine", e.map(|e| e.id.clone())),
                "active": e.map(|e| &e.id) == filter.engine.as_ref(),
            })
        })
        .collect();
    let csharp_link = link("csharp", (!filter.csharp).then(|| "1".to_string()));
    let feed_pairs: Vec<_> = pairs[..2].to_vec();
    st.render(
        "index.html",
        json!({
            "nav": "latest",
            "admin": admin,
            "items": st.item_views(&items, &dups),
            "kind_tabs": kind_tabs,
            "engine_links": engine_links,
            "csharp_link": csharp_link,
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

async fn engines(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let result = engines_page(&st, st.is_admin(&headers)).await;
    st.respond(result)
}

async fn engines_page(st: &AppState, admin: bool) -> PageResult {
    let stats = st.with_store(queries::engine_stats).await?;
    let now = Utc::now();
    let snapshot = st.registry.current();
    let tiers: Vec<_> = TIER_LABELS
        .iter()
        .map(|&(tier, label)| {
            let engines: Vec<_> = snapshot
                .config
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
    st.render(
        "engines.html",
        json!({"nav": "engines", "admin": admin, "tiers": tiers}),
    )
}

async fn engine(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
    headers: HeaderMap,
) -> Response {
    let admin = st.is_admin(&headers);
    let result = engine_page(&st, id, params, admin).await;
    st.respond(result)
}

async fn engine_page(st: &AppState, id: String, params: ListParams, admin: bool) -> PageResult {
    let snapshot = st.registry.current();
    let Some(engine) = snapshot.config.engine(&id) else {
        return Err(AppError::NotFound(format!("Unknown engine “{id}”.")));
    };
    let filter = ItemFilter {
        engine: Some(engine.id.clone()),
        kind: params.filter().kind,
        ..Default::default()
    };
    let page = params.page();
    let (mut items, dups) = st
        .items_with_dups(
            filter.clone(),
            PAGE_SIZE + 1,
            page.saturating_sub(1).saturating_mul(PAGE_SIZE),
        )
        .await?;
    let has_more = items.len() > PAGE_SIZE as usize;
    items.truncate(PAGE_SIZE as usize);

    let states = st.with_store(|s| s.source_states()).await?;
    let now = Utc::now();
    let sources: Vec<_> = snapshot
        .config
        .manageable_sources()
        .into_iter()
        .filter(|s| s.owner == Owner::Engine(engine.id.clone()))
        .map(|s| source_row(st, &s, states.get(&s.id), now))
        .collect();

    let base = format!("/engine/{}", engine.id);
    let tabs: Vec<_> = std::iter::once(None)
        .chain(ItemKind::ALL.into_iter().map(Some))
        .map(|k| {
            json!({
                "kind": k.map(ItemKind::as_str),
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
            "source_count": sources.len(),
            "groups": group_by_owner(&[(Owner::Engine(engine.id.clone()), &engine.name)], &sources),
            "grouped": false,
            "admin": admin,
            "next": base.clone(),
            "tabs": tabs,
            "items": st.item_views(&items, &dups),
            "pagination": pagination(&base, &kind_pair, page, has_more),
            "feed_url": format!("/feed.xml?engine={}", url::form_urlencoded::byte_serialize(engine.id.as_bytes()).collect::<String>()),
        }),
    )
}

/// `/sources` query: optional sort column and direction (unknown values mean config
/// order), and the owner to preselect in the add-source form.
#[derive(Debug, Default, Deserialize)]
struct SourcesParams {
    sort: Option<String>,
    dir: Option<String>,
    owner: Option<String>,
}

async fn sources(
    State(st): State<AppState>,
    Query(params): Query<SourcesParams>,
    headers: HeaderMap,
) -> Response {
    let admin = st.is_admin(&headers);
    let result = sources_page(&st, admin, params, None).await;
    st.respond(result)
}

/// Renders `/sources`. `feedback` re-opens a failed admin form with its message and values.
async fn sources_page(
    st: &AppState,
    admin: bool,
    params: SourcesParams,
    feedback: Option<serde_json::Value>,
) -> PageResult {
    let states = st.with_store(|s| s.source_states()).await?;
    let now = Utc::now();
    let snapshot = st.registry.current();
    let rows: Vec<_> = snapshot
        .config
        .manageable_sources()
        .iter()
        .map(|s| source_row(st, s, states.get(&s.id), now))
        .collect();

    // Group order: config order by default, or owner name A–Z / Z–A.
    let order = match (params.sort.as_deref(), params.dir.as_deref()) {
        (Some("owner"), Some("desc")) => "desc",
        (Some("owner"), _) => "asc",
        _ => "config",
    };
    let owners_in_order = snapshot.config.manageable_owners();
    let mut groups = group_by_owner(&owners_in_order, &rows);
    if order != "config" {
        groups.sort_by_key(|g| g["owner"].as_str().unwrap_or_default().to_lowercase());
        if order == "desc" {
            groups.reverse();
        }
    }
    let order_links: Vec<_> = [
        ("config", "Config", "/sources"),
        ("asc", "A–Z", "/sources?sort=owner"),
        ("desc", "Z–A", "/sources?sort=owner&dir=desc"),
    ]
    .into_iter()
    .map(|(key, label, url)| json!({"label": label, "url": url, "active": key == order}))
    .collect();
    let next = order_links
        .iter()
        .find(|l| l["active"] == true)
        .and_then(|l| l["url"].as_str())
        .unwrap_or("/sources")
        .to_string();
    let owners = |engines: bool| -> Vec<_> {
        snapshot
            .config
            .manageable_owners()
            .into_iter()
            .filter(|(o, _)| matches!(o, Owner::Engine(_)) == engines)
            .map(|(o, name)| json!({"id": o.id(), "name": name}))
            .collect()
    };
    let suggestions = suggestions_view(&snapshot);
    let tiers: Vec<_> = TIER_LABELS
        .iter()
        .map(|(tier, label)| json!({"tier": tier, "label": label}))
        .collect();
    let count = |status: &str| rows.iter().filter(|r| r["status"] == status).count();
    let summary = json!({
        "total": rows.len(),
        "enabled": rows.len() - count("disabled"),
        "ok": count("ok"),
        "failing": count("failing"),
        "pending": count("pending"),
        "disabled": count("disabled"),
    });
    st.render(
        "sources.html",
        json!({
            "nav": "sources",
            "groups": groups,
            "grouped": true,
            "order_links": order_links,
            "summary": summary,
            "admin": admin,
            "login_available": st.admin_token().is_some(),
            "next": next,
            "owners": {"engines": owners(true), "outlets": owners(false)},
            "categories": kind_names(),
            "default_interval": st.config.polling.default_interval_minutes,
            "preselect_owner": non_empty(&params.owner),
            "suggestions": suggestions,
            "tiers": tiers,
            "feedback": feedback,
        }),
    )
}

/// The catalog's suggestions: engines grouped by tier, then outlets, then missing sources
/// of present engines and outlets.
fn suggestions_view(snapshot: &Snapshot) -> serde_json::Value {
    let suggestions = catalog::suggestions(snapshot);
    let owner_view = |name: &str, id: &str, detail: String, sources: usize| json!({"id": id, "name": name, "detail": detail, "sources": sources});
    let mut groups: Vec<_> = TIER_LABELS
        .iter()
        .map(|&(tier, label)| {
            let owners: Vec<_> = suggestions
                .owners
                .iter()
                .filter_map(|o| match o {
                    NewOwner::Engine(e) if e.tier == tier => Some(owner_view(
                        &e.name,
                        &e.id,
                        format!("C#: {}", e.csharp),
                        e.sources.len(),
                    )),
                    _ => None,
                })
                .collect();
            json!({"label": label, "owners": owners})
        })
        .collect();
    let outlets: Vec<_> = suggestions
        .owners
        .iter()
        .filter_map(|o| match o {
            NewOwner::Outlet(o) => Some(owner_view(
                &o.name,
                &o.id,
                "news outlet".into(),
                o.sources.len(),
            )),
            NewOwner::Engine(_) => None,
        })
        .collect();
    groups.push(json!({"label": "News outlets", "owners": outlets}));
    groups.retain(|g| g["owners"].as_array().is_some_and(|o| !o.is_empty()));
    let sources: Vec<_> = suggestions
        .sources
        .iter()
        .map(|s| {
            let (kind, target, _) = describe(&s.spec);
            json!({"id": s.id, "owner": s.owner_name, "kind": kind, "target": target})
        })
        .collect();
    json!({
        "total": suggestions.owners.len() + sources.len(),
        "groups": groups,
        "sources": sources,
    })
}

/// One group of source rows per owner, in the given owner order. Owners without sources
/// get an empty group, so sources can be added to them.
fn group_by_owner(owners: &[(Owner, &str)], rows: &[serde_json::Value]) -> Vec<serde_json::Value> {
    owners
        .iter()
        .map(|(owner, name)| {
            let owner_kind = match owner {
                Owner::Engine(_) => "engine",
                Owner::Outlet(_) => "outlet",
            };
            let members: Vec<_> = rows
                .iter()
                .filter(|r| r["owner_kind"] == owner_kind && r["owner_id"] == owner.id())
                .cloned()
                .collect();
            let enabled = members.iter().filter(|r| r["enabled"] == true).count();
            json!({
                "owner": name,
                "owner_kind": owner_kind,
                "owner_id": owner.id(),
                "in_catalog": catalog::has_owner(owner.id()),
                "enabled": enabled,
                "total": members.len(),
                "sources": members,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct ToggleForm {
    source_id: String,
    /// "1" to enable, anything else to disable.
    enabled: String,
    next: Option<String>,
}

async fn toggle_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ToggleForm>,
) -> Response {
    let result = toggle_source_inner(&st, &headers, form).await;
    st.respond(result)
}

async fn toggle_source_inner(st: &AppState, headers: &HeaderMap, form: ToggleForm) -> PageResult {
    if !st.is_admin(headers) {
        return Err(AppError::Forbidden(
            "Log in as admin to switch sources on or off.".into(),
        ));
    }
    if !st
        .registry
        .current()
        .config
        .manageable_sources()
        .iter()
        .any(|s| s.id == form.source_id)
    {
        return Err(AppError::NotFound(format!(
            "Unknown source “{}”.",
            form.source_id
        )));
    }
    let enabled = form.enabled == "1";
    let id = form.source_id.clone();
    st.with_store(move |s| s.set_source_override(&id, enabled))
        .await?;
    st.toggles.set(&form.source_id, enabled);
    tracing::info!(source = %form.source_id, enabled, "source toggled");
    Ok(Redirect::to(&safe_next(form.next.as_deref())).into_response())
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
    next: Option<String>,
}

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

async fn login_form(State(st): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    let result = render_login(&st, q.next.as_deref(), None);
    st.respond(result)
}

async fn login(State(st): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let next = safe_next(form.next.as_deref());
    let ok = st.admin_token().is_some_and(|token| {
        // Compare hashes so the comparison is constant-time.
        blake3::hash(form.token.as_bytes()) == blake3::hash(token.as_bytes())
    });
    if !ok {
        tracing::warn!("failed admin login");
        let result = render_login(&st, Some(&next), Some("That token is not correct."))
            .map(|resp| (StatusCode::UNAUTHORIZED, resp).into_response());
        return st.respond(result);
    }
    let token = st.admin_token().unwrap_or_default();
    let cookie = format!(
        "{ADMIN_COOKIE}={}; Path=/; Max-Age={ADMIN_COOKIE_MAX_AGE}; HttpOnly; SameSite=Strict{}",
        admin_cookie_hash(token).to_hex(),
        secure_attr(&st),
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to(&next)).into_response()
}

async fn logout(State(st): State<AppState>) -> Response {
    let cookie = format!(
        "{ADMIN_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}",
        secure_attr(&st)
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/sources")).into_response()
}

fn render_login(st: &AppState, next: Option<&str>, error: Option<&str>) -> PageResult {
    st.render(
        "login.html",
        json!({
            "nav": "sources",
            "available": st.admin_token().is_some(),
            "next": safe_next(next),
            "error": error,
        }),
    )
}

/// Value stored in the admin cookie: a hash of the token, so the token itself never sits
/// in the browser.
fn admin_cookie_hash(token: &str) -> blake3::Hash {
    blake3::hash(format!("gen-admin-cookie-v1:{token}").as_bytes())
}

fn secure_attr(st: &AppState) -> &'static str {
    if st.base_url().starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
}

/// Reads one cookie from the request headers.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// A same-site path to redirect to; anything else (absolute or protocol-relative URLs)
/// falls back to the sources page.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.to_string(),
        _ => "/sources".to_string(),
    }
}

/// One source with its health, for the engine and sources pages.
fn source_row(
    st: &AppState,
    s: &crate::config::ResolvedSource,
    state: Option<&SourceState>,
    now: chrono::DateTime<Utc>,
) -> serde_json::Value {
    let state = state.cloned().unwrap_or_default();
    let enabled = st.toggles.is_enabled(s);
    let status = if !enabled {
        "disabled"
    } else if state.consecutive_failures > 0 {
        "failing"
    } else if state.last_ok.is_some() {
        "ok"
    } else {
        "pending"
    };
    let (kind, target, target_url) = describe(&s.spec);
    let (owner_kind, owner_id) = match &s.owner {
        Owner::Engine(id) => ("engine", id.clone()),
        Owner::Outlet(id) => ("outlet", id.clone()),
    };
    json!({
        "id": s.id,
        "enabled": enabled,
        "config_enabled": s.enabled,
        "owner": s.owner_name,
        "owner_kind": owner_kind,
        "owner_id": owner_id,
        "kind": kind,
        "target": target,
        "target_url": target_url,
        "category": s.category.map(ItemKind::as_str),
        "interval": s.interval_minutes.unwrap_or(st.config.polling.default_interval_minutes),
        "in_catalog": catalog::has_source(&s.id),
        "status": status,
        "last_ok": state.last_ok.map(|d| DateView::new(d, now)),
        "last_error": state.last_error,
        "failures": state.consecutive_failures,
    })
}

/// A source's kind, what it fetches (for display) and a link to that, if any.
fn describe(spec: &SourceSpec) -> (&'static str, String, Option<String>) {
    let (kind, target) = match spec {
        SourceSpec::Feed { url, .. } => ("feed", url.clone()),
        SourceSpec::Github { repo, mode, .. } => (
            "github",
            format!("{repo} ({})", format!("{mode:?}").to_lowercase()),
        ),
        SourceSpec::Unity { .. } => ("unity", "Unity releases API".to_string()),
        SourceSpec::Hn { query } => ("hn", query.clone()),
        SourceSpec::Html { url, .. } => ("html", url.clone()),
    };
    let target_url = match spec {
        SourceSpec::Feed { url, .. } | SourceSpec::Html { url, .. } => Some(url.clone()),
        SourceSpec::Github { repo, .. } => Some(format!("https://github.com/{repo}")),
        _ => None,
    };
    (kind, target, target_url)
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
    let lookup = &st.registry.current().lookup;
    let mut title = "GEN — Game Engine News".to_string();
    if let Some(e) = &filter.engine {
        title.push_str(&format!(" — {}", lookup.engine_name(e)));
    }
    if let Some(k) = filter.kind {
        title.push_str(&format!(" ({})", k.as_str()));
    }
    let query = query_string(&filter_pairs(&filter));
    let feed = feed::build(&items, lookup, st.base_url(), &query, &title);
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
        .items_with_dups(
            params.filter(),
            limit,
            page.saturating_sub(1).saturating_mul(limit),
        )
        .await?;
    let snapshot = st.registry.current();
    let out: Vec<_> = items
        .iter()
        .map(|item| ApiItem {
            item,
            also_on: snapshot.lookup.also_on(item, dups.get(&item.id)),
        })
        .collect();
    Ok(Json(json!({"items": out, "page": page, "limit": limit})).into_response())
}
