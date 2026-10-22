//! Admin actions that change the set of sources: adding, removing and restoring them.
//! Changes go through the [`SourceRegistry`](crate::registry::SourceRegistry), so they take
//! effect right away and are stored in the database (sources.toml is never written).

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{AppError, AppState, PageResult, SourcesParams, non_empty, safe_next, sources_page};
use crate::config::{GithubMode, SourceConfig, SourceSpec, default_unity_limit};
use crate::model::ItemKind;
use crate::registry::{EditError, SourceEdit};

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
/// `kind` are used.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct AddSourceForm {
    owner_id: Option<String>,
    kind: Option<String>,
    feed_url: Option<String>,
    cache_bust: Option<String>,
    repo: Option<String>,
    mode: Option<String>,
    tag_prefix: Option<String>,
    limit: Option<String>,
    query: Option<String>,
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
                tag_prefix: non_empty(&self.tag_prefix),
            },
            Some("unity") => SourceSpec::Unity {
                limit: match non_empty(&self.limit) {
                    None => default_unity_limit(),
                    Some(l) => l
                        .parse()
                        .map_err(|_| "The Unity limit must be a number.".to_string())?,
                },
            },
            Some("hn") => SourceSpec::Hn {
                query: required(&self.query, "a Hacker News search query")?,
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
            let page = sources_page(st, true, SourcesParams::default(), Some(feedback)).await?;
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

pub(super) async fn restore_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SourceForm>,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        st.apply_edit(SourceEdit::RestoreSource(form.source_id))
            .await?;
        Ok(Redirect::to(&safe_next(form.next.as_deref())).into_response())
    }
    .await;
    st.respond(result)
}
