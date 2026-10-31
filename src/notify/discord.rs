//! Discord notifications, through a webhook or as the GEN bot.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use serde_json::{Value, json};

use super::{NotifyContext, NotifyError, engine_names, kind_color, kind_emoji, kind_label};
use crate::config::{AppConfig, SourcesConfig};
use crate::fetch::truncate;
use crate::model::Item;

/// Discord allows at most 10 embeds per message.
const MAX_EMBEDS: usize = 10;
/// Discord caps the combined text of all embeds in one message at 6000 chars.
const MAX_MESSAGE_CHARS: usize = 6000;
const MAX_TITLE: usize = 256;
const MAX_DESCRIPTION: usize = 300;
/// Longest we wait on a 429 before the single retry.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Discord's REST API, as used by the bot.
pub const API_BASE: &str = "https://discord.com/api/v10";

/// What the bot needs to post messages: its token and the API to post to.
#[derive(Debug, Clone)]
pub struct BotApi {
    pub token: String,
    pub api_base: String,
}

impl BotApi {
    /// The bot's API access, when gen.toml has a `[discord]` section with a token.
    pub fn from_config(app: &AppConfig) -> Option<BotApi> {
        let discord = app.discord.as_ref().filter(|d| !d.token.is_empty())?;
        Some(BotApi {
            token: discord.token.clone(),
            api_base: API_BASE.to_string(),
        })
    }
}

pub async fn send(
    ctx: &NotifyContext<'_>,
    webhook_url: &str,
    items: &[Item],
) -> Result<(), NotifyError> {
    for payload in build_payloads(ctx.sources, items) {
        post(ctx, webhook_url, None, &payload).await?;
    }
    Ok(())
}

/// Posts the items to a channel as the bot.
pub async fn send_bot(
    ctx: &NotifyContext<'_>,
    channel_id: u64,
    items: &[Item],
) -> Result<(), NotifyError> {
    let bot = ctx.discord_bot.ok_or_else(|| {
        NotifyError::Other(
            "the Discord bot is not set up: add a [discord] section to gen.toml".into(),
        )
    })?;
    let url = format!("{}/channels/{channel_id}/messages", bot.api_base);
    let auth = format!("Bot {}", bot.token);
    for payload in build_payloads(ctx.sources, items) {
        post(ctx, &url, Some(&auth), &payload).await?;
    }
    Ok(())
}

/// POSTs one payload, retrying once after a 429.
async fn post(
    ctx: &NotifyContext<'_>,
    url: &str,
    auth: Option<&str>,
    payload: &Value,
) -> Result<(), NotifyError> {
    for attempt in 0..2 {
        let mut req = ctx.http.client.post(url).json(payload);
        if let Some(auth) = auth {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let header_wait = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());
        let body = resp.text().await.unwrap_or_default();
        if status == StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
            let wait = retry_after(&body, header_wait);
            tracing::warn!("discord rate limited, retrying in {wait:?}");
            tokio::time::sleep(wait).await;
            continue;
        }
        return Err(NotifyError::Other(format!(
            "discord returned {status}: {}",
            truncate(body.trim(), 500)
        )));
    }
    Err(NotifyError::Other("discord still rate limited".into()))
}

/// Wait time from a 429: `retry_after` (seconds) in the JSON body, else the header, capped.
fn retry_after(body: &str, header_secs: Option<f64>) -> Duration {
    let body_secs = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("retry_after").and_then(Value::as_f64));
    body_secs
        .or(header_secs)
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map_or(Duration::from_secs(1), Duration::from_secs_f64)
        .min(MAX_RETRY_WAIT)
}

/// Webhook payloads, each holding at most 10 embeds and staying under Discord's size limit.
pub fn build_payloads(sources: &SourcesConfig, items: &[Item]) -> Vec<Value> {
    let mut payloads = Vec::new();
    let mut chunk: Vec<Value> = Vec::new();
    let mut chars = 0;
    for item in items {
        let (embed, len) = build_embed(sources, item);
        if !chunk.is_empty() && (chunk.len() == MAX_EMBEDS || chars + len > MAX_MESSAGE_CHARS) {
            payloads.push(json!({ "embeds": std::mem::take(&mut chunk) }));
            chars = 0;
        }
        chars += len;
        chunk.push(embed);
    }
    if !chunk.is_empty() {
        payloads.push(json!({ "embeds": chunk }));
    }
    payloads
}

/// One embed plus the number of characters Discord counts toward the message limit.
fn build_embed(sources: &SourcesConfig, item: &Item) -> (Value, usize) {
    let title = truncate(
        &format!("{} {}", kind_emoji(item.kind), item.title),
        MAX_TITLE - 1,
    );
    let names = engine_names(sources, item);
    let footer = if names.is_empty() {
        kind_label(item.kind).to_string()
    } else {
        format!("{} · {}", names.join(", "), kind_label(item.kind))
    };
    let mut embed = json!({
        "title": title,
        "color": kind_color(item.kind),
        "footer": { "text": footer },
        "timestamp": item.date().to_rfc3339(),
    });
    let mut len = title.chars().count() + footer.chars().count();
    if let Some(url) = &item.url {
        embed["url"] = json!(url);
    }
    if let Some(summary) = item.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        let description = truncate(summary.trim(), MAX_DESCRIPTION);
        len += description.chars().count();
        embed["description"] = json!(description);
    }
    (embed, len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use crate::notify::tests::{item, sources};

    #[test]
    fn embed_fields() {
        let i = item(&["godot", "x"], ItemKind::Release, "Godot 4.8", Some("Big"));
        let payloads = build_payloads(&sources(), std::slice::from_ref(&i));
        assert_eq!(payloads.len(), 1);
        let e = &payloads[0]["embeds"][0];
        assert_eq!(e["title"], "🚀 Godot 4.8");
        assert_eq!(e["url"], "https://godotengine.org/releases/4.8/");
        assert_eq!(e["description"], "Big");
        assert_eq!(e["color"], 0x2ECC71);
        assert_eq!(e["footer"]["text"], "Godot, x · Release");
        assert_eq!(e["timestamp"], i.published.unwrap().to_rfc3339());
    }

    #[test]
    fn falls_back_to_first_seen_and_truncates() {
        let mut i = item(
            &[],
            ItemKind::Video,
            &"t".repeat(400),
            Some(&"s".repeat(400)),
        );
        i.published = None;
        i.url = None;
        let e = build_embed(&sources(), &i).0;
        assert_eq!(e["timestamp"], i.first_seen.to_rfc3339());
        assert_eq!(e["title"].as_str().unwrap().chars().count(), 256);
        assert!(e["description"].as_str().unwrap().chars().count() <= 301);
        assert!(e.get("url").is_none());
        assert_eq!(e["footer"]["text"], "Video");
        assert_eq!(e["color"], 0xE74C3C);
    }

    #[test]
    fn chunks_by_ten() {
        let items: Vec<_> = (0..23)
            .map(|n| item(&["godot"], ItemKind::News, &format!("n{n}"), None))
            .collect();
        let sizes: Vec<_> = build_payloads(&sources(), &items)
            .iter()
            .map(|p| p["embeds"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![10, 10, 3]);
        assert!(build_payloads(&sources(), &[]).is_empty());
    }

    #[test]
    fn chunks_by_size() {
        // Many unknown engine ids make a long footer, pushing 10 embeds past 6000 chars.
        let engines: Vec<String> = (0..40).map(|n| format!("engine{n:02}")).collect();
        let engines: Vec<&str> = engines.iter().map(String::as_str).collect();
        let items: Vec<_> = (0..10)
            .map(|_| {
                item(
                    &engines,
                    ItemKind::News,
                    &"t".repeat(300),
                    Some(&"s".repeat(400)),
                )
            })
            .collect();
        let payloads = build_payloads(&sources(), &items);
        assert!(payloads.len() > 1);
        for p in &payloads {
            let total: usize = p["embeds"].as_array().unwrap().iter().map(build_len).sum();
            assert!(total <= MAX_MESSAGE_CHARS);
        }
    }

    fn build_len(e: &Value) -> usize {
        ["title", "description"]
            .iter()
            .filter_map(|k| e[*k].as_str())
            .chain(e["footer"]["text"].as_str())
            .map(|s| s.chars().count())
            .sum()
    }

    #[tokio::test]
    async fn posts_as_the_bot() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sink = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = conn.read(&mut buf).await.unwrap();
            conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let bot = BotApi {
            token: "bot-token".into(),
            api_base: format!("http://{addr}/api/v10"),
        };
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: "https://gen.example.org/",
            discord_bot: Some(&bot),
        };
        let i = item(&["godot"], ItemKind::Release, "Godot 4.8", None);
        send_bot(&ctx, 42, &[i]).await.unwrap();

        let request = sink.await.unwrap();
        assert!(
            request.starts_with("POST /api/v10/channels/42/messages "),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bot bot-token"),
            "{request}"
        );
        assert!(request.contains(r#""embeds":[{"#), "{request}");
        assert!(request.contains("Godot 4.8"), "{request}");
    }

    #[tokio::test]
    async fn bot_channels_need_the_bot() {
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: "https://gen.example.org/",
            discord_bot: None,
        };
        let i = item(&["godot"], ItemKind::Release, "Godot 4.8", None);
        let err = send_bot(&ctx, 42, &[i]).await.unwrap_err().to_string();
        assert!(err.contains("[discord]"), "{err}");
    }

    #[test]
    fn parses_retry_after() {
        assert_eq!(
            retry_after(r#"{"retry_after": 1.5, "global": false}"#, Some(9.0)),
            Duration::from_millis(1500)
        );
        assert_eq!(retry_after("nope", Some(2.0)), Duration::from_secs(2));
        assert_eq!(retry_after("{}", Some(120.0)), MAX_RETRY_WAIT);
        assert_eq!(retry_after("", None), Duration::from_secs(1));
    }
}
