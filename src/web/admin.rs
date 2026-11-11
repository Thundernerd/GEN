//! Admin actions that change the set of sources: adding (by hand or from the catalog's
//! suggestions) and removing sources and engines/outlets.
//! Changes go through the [`SourceRegistry`](crate::registry::SourceRegistry), so they take
//! effect right away and are stored in the database.

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    AppError, AppState, PageResult, SourcesParams, describe, non_empty, page_with_notice,
    safe_next, sources_page,
};
use crate::catalog;
use crate::config::{
    EngineConfig, GithubMode, MAX_INTERVAL_MINUTES, OutletConfig, ResolvedSource, SourceConfig,
    SourceSpec, TagPrefixes,
};
use crate::model::ItemKind;
use crate::pipeline::DEFAULT_INTERVAL_KEY;
use crate::registry::{EditError, NewOwner, SourceEdit};
use crate::scheduler::FetchReport;

impl AppState {
    /// Applies an edit under the store lock and drops the toggles of purged sources.
    async fn apply_edit(&self, edit: SourceEdit) -> Result<(), AppError> {
        let (store, registry) = (self.store.clone(), self.registry.clone());
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            registry.apply(&mut guard, edit)
        })
        .await?;
        match result {
            Ok(purged) => {
                purged.iter().for_each(|id| self.toggles.forget(id));
                Ok(())
            }
            Err(EditError::Invalid(msg)) => Err(AppError::Invalid(msg)),
            Err(EditError::Internal(err)) => Err(AppError::Internal(err)),
        }
    }
}

fn require_admin(st: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if st.is_admin(headers) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "Log in as admin to manage sources.".into(),
        ))
    }
}

/// The add-source form. Every field is optional text, so a bad submission re-renders the
/// form with a message instead of failing to deserialize. Only the fields of the chosen
/// `kind` are used. Unity releases and Hacker News searches can't be added here; the
/// Unity source comes from the catalog's suggestions.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct AddSourceForm {
    owner_id: Option<String>,
    kind: Option<String>,
    feed_url: Option<String>,
    cache_bust: Option<String>,
    repo: Option<String>,
    mode: Option<String>,
    tag_prefix: Option<String>,
    html_url: Option<String>,
    item: Option<String>,
    title: Option<String>,
    link: Option<String>,
    date: Option<String>,
    category: Option<String>,
    interval_minutes: Option<String>,
    keywords: Option<String>,
}

impl AddSourceForm {
    fn to_edit(&self) -> Result<SourceEdit, String> {
        let required =
            |v: &Option<String>, what: &str| non_empty(v).ok_or_else(|| format!("Enter {what}."));
        let owner_id =
            non_empty(&self.owner_id).ok_or("Choose the engine or outlet for this source.")?;
        let spec = match non_empty(&self.kind).as_deref() {
            Some("feed") => SourceSpec::Feed {
                url: required(&self.feed_url, "the feed URL")?,
                cache_bust: non_empty(&self.cache_bust).is_some(),
            },
            Some("github") => SourceSpec::Github {
                repo: required(&self.repo, "the GitHub repository")?,
                mode: match non_empty(&self.mode).as_deref() {
                    None | Some("releases") => GithubMode::Releases,
                    Some("tags") => GithubMode::Tags,
                    Some("commits") => GithubMode::Commits,
                    Some(other) => return Err(format!("Unknown GitHub mode “{other}”.")),
                },
                tag_prefixes: TagPrefixes::new(
                    self.tag_prefix
                        .as_deref()
                        .unwrap_or_default()
                        .split([',', '\n']),
                ),
            },
            Some("html") => SourceSpec::Html {
                url: required(&self.html_url, "the page URL")?,
                item: required(&self.item, "the item selector")?,
                title: required(&self.title, "the title selector")?,
                link: required(&self.link, "the link selector")?,
                date: non_empty(&self.date),
            },
            Some(other) => return Err(format!("Unknown kind of source “{other}”.")),
            None => return Err("Choose the kind of source.".into()),
        };
        let category = match non_empty(&self.category) {
            None => None,
            Some(c) => Some(ItemKind::parse(&c).ok_or_else(|| format!("Unknown category “{c}”."))?),
        };
        let interval_minutes = match non_empty(&self.interval_minutes) {
            None => None,
            Some(m) => Some(
                m.parse()
                    .map_err(|_| "The interval must be a whole number of minutes.".to_string())?,
            ),
        };
        Ok(SourceEdit::AddSource {
            owner_id,
            config: SourceConfig {
                category,
                enabled: true,
                interval_minutes,
                keywords: split_list(self.keywords.as_deref()),
                spec,
            },
        })
    }
}

/// Splits a comma- or newline-separated list, dropping empty entries.
fn split_list(text: Option<&str>) -> Vec<String> {
    text.unwrap_or_default()
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) async fn add_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddSourceForm>,
) -> Response {
    let result = add_source_inner(&st, &headers, form).await;
    st.respond(result)
}

async fn add_source_inner(st: &AppState, headers: &HeaderMap, form: AddSourceForm) -> PageResult {
    require_admin(st, headers)?;
    let result = match form.to_edit() {
        Ok(edit) => st.apply_edit(edit).await,
        Err(msg) => Err(AppError::Invalid(msg)),
    };
    match result {
        Ok(()) => Ok(Redirect::to("/sources").into_response()),
        // Show the form again, filled in, with the message.
        Err(AppError::Invalid(msg)) => {
            let feedback = json!({"panel": "add-source", "error": msg, "form": form});
            let page =
                sources_page(st, true, SourcesParams::default(), Some(feedback), None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(err) => Err(err),
    }
}

#[derive(Deserialize)]
pub(super) struct SourceForm {
    source_id: String,
    next: Option<String>,
}

pub(super) async fn remove_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SourceForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        st.apply_edit(SourceEdit::RemoveSource(form.source_id))
            .await?;
        Ok(Redirect::to(&safe_next(form.next.as_deref())).into_response())
    }
    .await;
    st.respond(result)
}

/// Adds a suggested source from the catalog to its (present) engine or outlet.
pub(super) async fn add_suggested_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SourceForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let source = catalog::source(&form.source_id).ok_or_else(|| {
            AppError::Invalid(format!("“{}” is not a suggested source.", form.source_id))
        })?;
        st.apply_edit(SourceEdit::AddSource {
            owner_id: source.owner.id().to_string(),
            config: SourceConfig {
                category: source.category,
                enabled: source.enabled,
                interval_minutes: source.interval_minutes,
                keywords: source.keywords,
                spec: source.spec,
            },
        })
        .await?;
        Ok(Redirect::to(&safe_next(form.next.as_deref())).into_response())
    }
    .await;
    st.respond(result)
}

/// The add-engine/outlet form; like [`AddSourceForm`], all optional text.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct AddOwnerForm {
    owner_kind: Option<String>,
    id: Option<String>,
    name: Option<String>,
    homepage: Option<String>,
    icon: Option<String>,
    tier: Option<String>,
    languages: Option<String>,
    language_note: Option<String>,
    aliases: Option<String>,
    exclude: Option<String>,
    require_engine_match: Option<String>,
}

impl AddOwnerForm {
    fn to_owner(&self) -> Result<NewOwner, String> {
        let id = non_empty(&self.id).ok_or("Enter an id.")?;
        let name = non_empty(&self.name).ok_or("Enter a name.")?;
        let homepage = non_empty(&self.homepage);
        match non_empty(&self.owner_kind).as_deref() {
            Some("engine") => Ok(NewOwner::Engine(EngineConfig {
                id,
                name,
                tier: non_empty(&self.tier)
                    .and_then(|t| t.parse::<u8>().ok())
                    .ok_or("Choose a tier.")?
                    .try_into()?,
                languages: split_list(self.languages.as_deref()),
                language_note: non_empty(&self.language_note),
                homepage,
                icon: non_empty(&self.icon),
                enabled: true,
                aliases: split_list(self.aliases.as_deref()),
                exclude: split_list(self.exclude.as_deref()),
                sources: Vec::new(),
            })),
            Some("outlet") => Ok(NewOwner::Outlet(OutletConfig {
                id,
                name,
                homepage,
                enabled: true,
                require_engine_match: non_empty(&self.require_engine_match).is_some(),
                sources: Vec::new(),
            })),
            _ => Err("Choose engine or news outlet.".into()),
        }
    }
}

pub(super) async fn add_owner(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddOwnerForm>,
) -> Response {
    let result = add_owner_inner(&st, &headers, form).await;
    st.respond(result)
}

async fn add_owner_inner(st: &AppState, headers: &HeaderMap, form: AddOwnerForm) -> PageResult {
    require_admin(st, headers)?;
    let result = match form.to_owner() {
        Ok(owner) => {
            let id = owner.id().to_string();
            st.apply_edit(SourceEdit::AddOwner(owner))
                .await
                .map(|()| id)
        }
        Err(msg) => Err(AppError::Invalid(msg)),
    };
    match result {
        // Straight on to adding its first source.
        Ok(id) => Ok(Redirect::to(&format!(
            "/sources?owner={}#add-source",
            url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
        ))
        .into_response()),
        Err(AppError::Invalid(msg)) => {
            let panel = match form.owner_kind.as_deref() {
                Some("outlet") => "add-outlet",
                _ => "add-engine",
            };
            let feedback = json!({"panel": panel, "error": msg, "form": form});
            let page =
                sources_page(st, true, SourcesParams::default(), Some(feedback), None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(err) => Err(err),
    }
}

#[derive(Deserialize)]
pub(super) struct OwnerForm {
    owner_id: String,
}

pub(super) async fn remove_owner(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<OwnerForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        st.apply_edit(SourceEdit::RemoveOwner(form.owner_id))
            .await?;
        Ok(Redirect::to("/sources").into_response())
    }
    .await;
    st.respond(result)
}

/// Adds a suggested engine or outlet from the catalog, with its sources.
pub(super) async fn add_suggested_owner(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<OwnerForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let owner = catalog::owner(&form.owner_id).ok_or_else(|| {
            AppError::Invalid(format!(
                "“{}” is not a suggested engine or outlet.",
                form.owner_id
            ))
        })?;
        st.apply_edit(SourceEdit::AddOwner(owner)).await?;
        Ok(Redirect::to("/sources").into_response())
    }
    .await;
    st.respond(result)
}

/// How long a "Fetch now" waits for its result before reporting that it's still running.
const FETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long "Fetch all now" waits for the results.
const FETCH_ALL_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Deserialize)]
pub(super) struct FetchForm {
    source_id: Option<String>,
    next: Option<String>,
}

/// A source's display name for messages, e.g. "Godot github godotengine/godot (releases)".
fn source_label(s: &ResolvedSource) -> String {
    let (kind, target, _) = describe(&s.spec);
    format!("{} {kind} {target}", s.owner_name)
}

/// Asks the source's poller to fetch now, waits for the result and shows it.
pub(super) async fn fetch_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<FetchForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let id = form.source_id.clone().unwrap_or_default();
        let source = st
            .registry
            .current()
            .config
            .manageable_sources()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| AppError::Invalid(format!("There is no source “{id}”.")))?;
        let label = source_label(&source);
        tracing::info!(source = %id, "fetch requested");
        let notice = if !st.toggles.is_enabled(&source) {
            json!({"ok": false, "message": format!("{label} is switched off; enable it first.")})
        } else {
            match st.fetches.request(&id) {
                None => json!({"ok": false, "message": format!("{label} isn't running yet; try again in a moment.")}),
                Some(rx) => match tokio::time::timeout(FETCH_WAIT, rx).await {
                    Ok(Ok(report)) => report_notice(&label, &report),
                    Ok(Err(_)) => json!({"ok": false, "message": format!("{label} was stopped before it could fetch.")}),
                    Err(_) => json!({"ok": true, "message": format!("{label} is still fetching; its status updates when it's done.")}),
                },
            }
        };
        page_with_notice(&st, &headers, form.next.as_deref(), notice).await
    }
    .await;
    st.respond(result)
}

fn report_notice(label: &str, report: &FetchReport) -> serde_json::Value {
    if let Some(why) = &report.skipped {
        return json!({"ok": true, "message": format!("Didn't fetch {label}: {why}.")});
    }
    let message = match (&report.error, report.new_items, report.not_modified) {
        (Some(err), _, _) => {
            return json!({"ok": false, "message": format!("Fetching {label} failed: {err}")});
        }
        (None, 0, true) => format!("Fetched {label}: unchanged since the last fetch."),
        (None, 0, false) => format!("Fetched {label}: no new items."),
        (None, 1, _) => format!("Fetched {label}: 1 new item."),
        (None, n, _) => format!("Fetched {label}: {n} new items."),
    };
    json!({"ok": true, "message": message})
}

/// Asks every enabled source to fetch now and reports how it went.
pub(super) async fn fetch_all(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let sources: Vec<ResolvedSource> = st
            .registry
            .current()
            .config
            .manageable_sources()
            .into_iter()
            .filter(|s| st.toggles.is_enabled(s))
            .collect();
        let pending: Vec<_> = sources
            .iter()
            .filter_map(|s| st.fetches.request(&s.id).map(|rx| (s, rx)))
            .collect();
        if pending.is_empty() {
            let notice =
                json!({"ok": false, "message": "Nothing to fetch: no enabled source is running."});
            return page_with_notice(&st, &headers, Some("/sources"), notice).await;
        }
        let requested = pending.len();
        tracing::info!(sources = requested, "fetch all requested");
        let batch = st.fetches.start_batch(requested);
        let mut pending: FuturesUnordered<_> = pending
            .into_iter()
            .map(|(source, rx)| async move { (source, rx.await.ok()) })
            .collect();
        let deadline = tokio::time::Instant::now() + FETCH_ALL_WAIT;
        let (mut new_items, mut failed, mut skipped) = (0, Vec::new(), 0);
        while let Ok(Some((source, report))) =
            tokio::time::timeout_at(deadline, pending.next()).await
        {
            batch.finished(report.as_ref());
            match report {
                Some(r) if r.skipped.is_some() => skipped += 1,
                Some(r) if r.error.is_none() => new_items += r.new_items,
                _ => failed.push(source.owner_name.clone()),
            }
        }
        let running = pending.len();
        drop(batch);
        tracing::info!(
            fetched = requested - running,
            new = new_items,
            failed = failed.len(),
            skipped,
            still_running = running,
            "fetch all finished"
        );
        let mut message = format!(
            "Fetched {} of {requested} sources: {new_items} new item{}",
            requested - running,
            if new_items == 1 { "" } else { "s" }
        );
        if !failed.is_empty() {
            failed.dedup();
            message.push_str(&format!(
                ", {} failed ({})",
                failed.len(),
                failed.join(", ")
            ));
        }
        if skipped > 0 {
            message.push_str(&format!(
                ", {skipped} skipped while a rate-limited host cools down"
            ));
        }
        if running > 0 {
            message.push_str(&format!(", {running} still fetching"));
        }
        message.push('.');
        let notice = json!({"ok": failed.is_empty(), "message": message});
        page_with_notice(&st, &headers, Some("/sources"), notice).await
    }
    .await;
    st.respond(result)
}

/// What the pollers are doing right now, polled by the Sources page to show progress.
pub(super) async fn fetch_activity(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let result =
        require_admin(&st, &headers).map(|()| axum::Json(st.fetches.snapshot()).into_response());
    st.respond(result)
}

#[derive(Deserialize)]
pub(super) struct IntervalForm {
    source_id: Option<String>,
    /// Empty means "use the default".
    minutes: Option<String>,
    next: Option<String>,
}

fn parse_minutes(text: Option<&str>) -> Result<Option<u64>, String> {
    match text.map(str::trim).filter(|m| !m.is_empty()) {
        None => Ok(None),
        Some(m) => match m.parse::<u64>() {
            Ok(n) if (1..=MAX_INTERVAL_MINUTES).contains(&n) => Ok(Some(n)),
            _ => Err(format!(
                "The interval must be a whole number of minutes from 1 to {MAX_INTERVAL_MINUTES}."
            )),
        },
    }
}

/// Sets how often one source is polled (or back to the default).
pub(super) async fn set_interval(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<IntervalForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let edit = parse_minutes(form.minutes.as_deref()).map(|minutes| SourceEdit::SetInterval {
            source_id: form.source_id.clone().unwrap_or_default(),
            minutes,
        });
        let result = match edit {
            Ok(edit) => st.apply_edit(edit).await,
            Err(msg) => Err(AppError::Invalid(msg)),
        };
        match result {
            Ok(()) => Ok(Redirect::to(&safe_next(form.next.as_deref())).into_response()),
            Err(AppError::Invalid(msg)) => {
                let page = page_with_notice(
                    &st,
                    &headers,
                    form.next.as_deref(),
                    json!({"ok": false, "message": msg}),
                )
                .await?;
                Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
            }
            Err(err) => Err(err),
        }
    }
    .await;
    st.respond(result)
}

#[derive(Deserialize)]
pub(super) struct DefaultIntervalForm {
    minutes: Option<String>,
}

/// Sets the polling interval of every source without its own.
pub(super) async fn set_default_interval(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DefaultIntervalForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let minutes = match parse_minutes(form.minutes.as_deref()) {
            Ok(Some(minutes)) => minutes,
            Ok(None) => st.config.polling.default_interval_minutes,
            Err(msg) => {
                let page = page_with_notice(
                    &st,
                    &headers,
                    Some("/sources"),
                    json!({"ok": false, "message": msg}),
                )
                .await?;
                return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
            }
        };
        st.with_store(move |s| s.set_meta(DEFAULT_INTERVAL_KEY, &minutes.to_string()))
            .await?;
        st.settings.set_default_interval(minutes);
        tracing::info!(minutes, "default polling interval changed");
        Ok(Redirect::to("/sources").into_response())
    }
    .await;
    st.respond(result)
}
