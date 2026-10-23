//! The Notifications page: adding, editing, switching off, testing and removing notification
//! channels (admin only). Channels are stored in the database; the notifier reads them on
//! every run, and is woken after a change.
//!
//! Secrets (webhook URLs, ntfy topics and tokens, SMTP passwords) are never sent back to the
//! browser. A `${VAR}` reference is not a secret and is shown as written.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use serde_json::{Value, json};

use super::view::DateView;
use super::{AppError, AppState, PageResult};
use crate::config::{ChannelConfig, ChannelTarget, NotifyRules};
use crate::model::ItemKind;
use crate::notify::{self, Channel, NotifyContext};

fn require_admin(st: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if st.is_admin(headers) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "Log in as admin to manage notifications.".into(),
        ))
    }
}

pub(super) async fn page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !st.is_admin(&headers) {
        return Redirect::to("/login?next=/notifications").into_response();
    }
    let result = render(&st, None, None).await;
    st.respond(result)
}

/// Renders the page. `feedback` reopens a failed dialog (`dialog`, `error`, posted `form`);
/// `notice` reports the result of a test send (`ok`, `message`).
async fn render(st: &AppState, feedback: Option<Value>, notice: Option<Value>) -> PageResult {
    let channels = st.with_store(notify::load_channels).await?;
    let now = Utc::now();
    let engines = engine_choices(st, &channels);
    let failed = feedback.as_ref().and_then(|f| f["dialog"].as_str());
    let views: Vec<_> = channels
        .iter()
        .map(|c| {
            let dialog = format!("edit-{}", c.config.name);
            let form = match &feedback {
                Some(f) if failed == Some(dialog.as_str()) => f["form"].clone(),
                _ => form_values(&c.config),
            };
            json!({
                "name": c.config.name,
                "kind": c.config.target.kind(),
                "kind_label": kind_label(c.config.target.kind()),
                "enabled": c.config.enabled,
                "target": target_summary(&c.config.target),
                "rules": rules_summary(st, &c.config.rules),
                "last_ok": c.last_ok.map(|d| DateView::new(d, now)),
                "last_error": c.last_error,
                "last_error_at": c.last_error_at.map(|d| DateView::new(d, now)),
                "dialog": dialog,
                "form": form,
                "secrets": secret_hints(&c.config.target),
            })
        })
        .collect();
    let add_form = match &feedback {
        Some(f) if failed == Some("add-channel") => f["form"].clone(),
        _ => json!({"kind": "discord", "enabled": true, "ntfy_server": "https://ntfy.sh",
                    "smtp_port": 587, "kinds": ItemKind::ALL.map(ItemKind::as_str)}),
    };
    let page = st.render(
        "notifications.html",
        json!({
            "nav": "notifications",
            "admin": true,
            "channels": views,
            "add_form": add_form,
            "engines": engines,
            "kinds": ItemKind::ALL.map(ItemKind::as_str),
            "feedback": feedback,
            "notice": notice,
        }),
    )?;
    let status = if feedback.is_some() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    Ok((status, page).into_response())
}

/// Enabled engines, plus engines named in a channel's rules that no longer exist.
fn engine_choices(st: &AppState, channels: &[Channel]) -> Vec<Value> {
    let snapshot = st.registry.current();
    let mut out: Vec<Value> = snapshot
        .config
        .enabled_engines()
        .map(|e| json!({"id": e.id, "name": e.name}))
        .collect();
    for id in channels.iter().flat_map(|c| &c.config.rules.engines) {
        if !out.iter().any(|e| e["id"] == id.as_str()) {
            out.push(json!({"id": id, "name": format!("{id} (gone)")}));
        }
    }
    out
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "discord" => "Discord",
        "ntfy" => "ntfy",
        _ => "Email",
    }
}

/// Whether a stored value is a `${VAR}` reference (safe to show) rather than a secret.
fn is_reference(value: &str) -> bool {
    value.starts_with("${") && value.ends_with('}')
}

/// Where a channel sends to, without secrets.
fn target_summary(target: &ChannelTarget) -> String {
    match target {
        ChannelTarget::Discord { webhook_url } if is_reference(webhook_url) => {
            format!("webhook {webhook_url}")
        }
        ChannelTarget::Discord { .. } => "Discord webhook".to_string(),
        ChannelTarget::Ntfy { server, topic, .. } => {
            let host = url::Url::parse(server)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| server.clone());
            if is_reference(topic) {
                format!("{host} · topic {topic}")
            } else {
                format!("{host} · private topic")
            }
        }
        ChannelTarget::Email {
            smtp_host,
            to,
            digest,
            ..
        } => format!(
            "{} via {smtp_host}{}",
            to.join(", "),
            if *digest { " · digest" } else { "" }
        ),
    }
}

/// Which items a channel gets, in a few words.
fn rules_summary(st: &AppState, rules: &NotifyRules) -> String {
    let mut parts = Vec::new();
    let kinds = effective_kinds(rules);
    if kinds.len() < ItemKind::ALL.len() {
        parts.push(
            kinds
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if rules.engines.is_empty() {
        parts.push("all engines".to_string());
    } else {
        let lookup = &st.registry.current().lookup;
        let names: Vec<_> = rules
            .engines
            .iter()
            .map(|e| lookup.engine_name(e))
            .collect();
        parts.push(names.join(", "));
    }
    if rules.include_untagged {
        parts.push("untagged outlet news".to_string());
    }
    if !rules.keywords.is_empty() {
        parts.push(format!("keywords: {}", rules.keywords.join(", ")));
    }
    parts.join(" · ")
}

/// The kinds a channel is sent: `kinds` (or all when empty) minus `exclude_kinds`.
fn effective_kinds(rules: &NotifyRules) -> Vec<ItemKind> {
    ItemKind::ALL
        .into_iter()
        .filter(|k| rules.kinds.is_empty() || rules.kinds.contains(k))
        .filter(|k| !rules.exclude_kinds.contains(k))
        .collect()
}

/// What the edit form shows for each secret: a `${VAR}` reference, "set", or nothing.
fn secret_hints(target: &ChannelTarget) -> Value {
    let hint = |v: Option<&String>| match v {
        Some(v) if is_reference(v) => json!({"set": true, "reference": v}),
        Some(v) if !v.is_empty() => json!({"set": true}),
        _ => json!({"set": false}),
    };
    match target {
        ChannelTarget::Discord { webhook_url } => json!({"webhook_url": hint(Some(webhook_url))}),
        ChannelTarget::Ntfy { topic, token, .. } => {
            json!({"ntfy_topic": hint(Some(topic)), "ntfy_token": hint(token.as_ref())})
        }
        ChannelTarget::Email { password, .. } => json!({"smtp_password": hint(password.as_ref())}),
    }
}

/// The form values for a stored channel, secrets left out.
fn form_values(c: &ChannelConfig) -> Value {
    let mut form = json!({
        "name": c.name,
        "kind": c.target.kind(),
        "enabled": c.enabled,
        "engines": c.rules.engines,
        "kinds": effective_kinds(&c.rules).iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "keywords": c.rules.keywords.join(", "),
        "include_untagged": c.rules.include_untagged,
        "ntfy_server": "https://ntfy.sh",
        "smtp_port": 587,
    });
    match &c.target {
        ChannelTarget::Discord { .. } => {}
        ChannelTarget::Ntfy { server, .. } => form["ntfy_server"] = json!(server),
        ChannelTarget::Email {
            smtp_host,
            smtp_port,
            username,
            from,
            to,
            digest,
            ..
        } => {
            form["smtp_host"] = json!(smtp_host);
            form["smtp_port"] = json!(smtp_port);
            form["smtp_username"] = json!(username);
            form["from"] = json!(from);
            form["to"] = json!(to.join(", "));
            form["digest"] = json!(digest);
        }
    }
    form
}

/// A posted form. Parsed by hand because checkbox groups repeat their key.
struct Posted(Vec<(String, String)>);

impl Posted {
    fn parse(body: &Bytes) -> Self {
        Posted(url::form_urlencoded::parse(body).into_owned().collect())
    }

    /// The first value of `key`, trimmed, if not empty.
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    fn all(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect()
    }

    fn checked(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The values to show again after a failed submission (secrets left out).
    fn redisplay(&self) -> Value {
        let mut form = json!({
            "engines": self.all("engines"),
            "kinds": self.all("kinds"),
            "enabled": self.checked("enabled"),
            "include_untagged": self.checked("include_untagged"),
            "digest": self.checked("digest"),
        });
        for key in [
            "name",
            "kind",
            "keywords",
            "ntfy_server",
            "smtp_host",
            "smtp_port",
            "smtp_username",
            "from",
            "to",
        ] {
            form[key] = json!(self.get(key));
        }
        form
    }

    /// Builds a channel from the form. Secret fields left empty keep their value from
    /// `existing` (when editing a channel of the same kind).
    fn to_channel(&self, existing: Option<&ChannelConfig>) -> Result<ChannelConfig, String> {
        let name = match existing {
            Some(c) => c.name.clone(),
            None => self.get("name").ok_or("Enter a name.")?,
        };
        let kept =
            |pick: fn(&ChannelTarget) -> Option<String>| existing.and_then(|c| pick(&c.target));
        let secret = |key: &str, pick: fn(&ChannelTarget) -> Option<String>| {
            self.get(key).or_else(|| kept(pick))
        };
        let target = match self.get("kind").as_deref() {
            Some("discord") => ChannelTarget::Discord {
                webhook_url: secret("webhook_url", |t| match t {
                    ChannelTarget::Discord { webhook_url } => Some(webhook_url.clone()),
                    _ => None,
                })
                .ok_or("Enter the Discord webhook URL.")?,
            },
            Some("ntfy") => ChannelTarget::Ntfy {
                server: self
                    .get("ntfy_server")
                    .unwrap_or_else(|| "https://ntfy.sh".into()),
                topic: secret("ntfy_topic", |t| match t {
                    ChannelTarget::Ntfy { topic, .. } => Some(topic.clone()),
                    _ => None,
                })
                .ok_or("Enter the ntfy topic.")?,
                token: secret("ntfy_token", |t| match t {
                    ChannelTarget::Ntfy { token, .. } => token.clone(),
                    _ => None,
                }),
            },
            Some("email") => ChannelTarget::Email {
                smtp_host: self.get("smtp_host").ok_or("Enter the SMTP host.")?,
                smtp_port: match self.get("smtp_port") {
                    None => 587,
                    Some(p) => p.parse().map_err(|_| "The SMTP port must be a number.")?,
                },
                username: self.get("smtp_username"),
                password: secret("smtp_password", |t| match t {
                    ChannelTarget::Email { password, .. } => password.clone(),
                    _ => None,
                }),
                from: self.get("from").ok_or("Enter the From address.")?,
                to: split_list(self.get("to")),
                digest: self.checked("digest"),
            },
            _ => return Err("Choose Discord, ntfy or email.".into()),
        };
        let checked: Vec<ItemKind> = self
            .all("kinds")
            .iter()
            .filter_map(|k| ItemKind::parse(k))
            .collect();
        if checked.is_empty() {
            return Err("Choose at least one kind of item.".into());
        }
        let rules = NotifyRules {
            engines: self.all("engines"),
            // Every kind checked means no restriction, so new kinds are included too.
            kinds: if checked.len() == ItemKind::ALL.len() {
                Vec::new()
            } else {
                checked
            },
            exclude_kinds: Vec::new(),
            keywords: split_list(self.get("keywords")),
            include_untagged: self.checked("include_untagged"),
        };
        let channel = ChannelConfig {
            name,
            enabled: self.checked("enabled"),
            rules,
            target,
        };
        channel.check()?;
        Ok(channel)
    }
}

fn split_list(text: Option<String>) -> Vec<String> {
    text.unwrap_or_default()
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

async fn find(st: &AppState, name: &str) -> Result<ChannelConfig, AppError> {
    let channels = st.with_store(notify::load_channels).await?;
    channels
        .into_iter()
        .map(|c| c.config)
        .find(|c| c.name == name)
        .ok_or_else(|| AppError::Invalid(format!("There is no channel “{name}”.")))
}

/// Stores a changed channel and wakes the notifier.
async fn save(st: &AppState, channel: &ChannelConfig, new: bool) -> Result<(), AppError> {
    let json = serde_json::to_string(channel).map_err(anyhow::Error::from)?;
    let name = channel.name.clone();
    let stored = st
        .with_store(move |s| {
            if new {
                s.add_channel(&name, &json)
            } else {
                s.update_channel(&name, &json)
            }
        })
        .await?;
    if !stored {
        let msg = if new {
            format!("“{}” is already used.", channel.name)
        } else {
            format!("There is no channel “{}”.", channel.name)
        };
        return Err(AppError::Invalid(msg));
    }
    st.notify_wake.notify_one();
    tracing::info!(channel = %channel.name, "notify channel saved");
    Ok(())
}

pub(super) async fn add(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let posted = Posted::parse(&body);
        let result = match posted.to_channel(None) {
            Ok(channel) => save(&st, &channel, true).await,
            Err(msg) => Err(AppError::Invalid(msg)),
        };
        finish(&st, result, "add-channel", &posted).await
    }
    .await;
    st.respond(result)
}

pub(super) async fn edit(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let posted = Posted::parse(&body);
        let name = posted.get("original_name").unwrap_or_default();
        let existing = find(&st, &name).await?;
        let result = match posted.to_channel(Some(&existing)) {
            Ok(channel) => save(&st, &channel, false).await,
            Err(msg) => Err(AppError::Invalid(msg)),
        };
        finish(&st, result, &format!("edit-{name}"), &posted).await
    }
    .await;
    st.respond(result)
}

/// Redirects after a successful add or edit, or shows the dialog again with the message.
async fn finish(
    st: &AppState,
    result: Result<(), AppError>,
    dialog: &str,
    posted: &Posted,
) -> PageResult {
    match result {
        Ok(()) => Ok(Redirect::to("/notifications").into_response()),
        Err(AppError::Invalid(msg)) => {
            let feedback = json!({"dialog": dialog, "error": msg, "form": posted.redisplay()});
            render(st, Some(feedback), None).await
        }
        Err(err) => Err(err),
    }
}

pub(super) async fn toggle(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let posted = Posted::parse(&body);
        let mut channel = find(&st, &posted.get("name").unwrap_or_default()).await?;
        channel.enabled = posted.get("enabled").as_deref() == Some("1");
        save(&st, &channel, false).await?;
        Ok(Redirect::to("/notifications").into_response())
    }
    .await;
    st.respond(result)
}

pub(super) async fn remove(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let name = find(&st, &Posted::parse(&body).get("name").unwrap_or_default())
            .await?
            .name;
        let removed = name.clone();
        st.with_store_mut(move |s| s.remove_channel(&removed))
            .await?;
        tracing::info!(channel = %name, "notify channel removed");
        Ok(Redirect::to("/notifications").into_response())
    }
    .await;
    st.respond(result)
}

/// Sends a sample item to one channel, ignoring its rules, and shows how it went.
pub(super) async fn test(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        require_admin(&st, &headers)?;
        let channel = find(&st, &Posted::parse(&body).get("name").unwrap_or_default()).await?;
        let snapshot = st.registry.current();
        let ctx = NotifyContext {
            http: &st.http,
            sources: &snapshot.config,
            base_url: &st.config.server.base_url,
        };
        let sent = match channel.resolved() {
            Ok(resolved) => notify::send(&ctx, &resolved, &[notify::sample_item()])
                .await
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("{e:#}")),
        };
        let name = channel.name.clone();
        let outcome = sent.clone();
        st.with_store(move |s| match &outcome {
            Ok(()) => s.record_channel_ok(&name),
            Err(e) => s.record_channel_error(&name, e),
        })
        .await?;
        let notice = match sent {
            Ok(()) => json!({"ok": true, "message": format!("Test notification sent to {}.", channel.name)}),
            Err(e) => json!({"ok": false, "message": format!("Sending to {} failed: {e}", channel.name)}),
        };
        render(&st, None, Some(notice)).await
    }
    .await;
    st.respond(result)
}
