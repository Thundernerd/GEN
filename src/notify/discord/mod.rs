//! Discord notifications, through a webhook or as the GEN bot, in the channel's
//! [`MessageStyle`].

mod card;
mod embed;
mod parts;

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use serde_json::{Value, json};

use self::parts::Parts;
use super::{NotifyContext, NotifyError, TextMessage};
use crate::config::{AppConfig, MessageStyle, SourcesConfig};
use crate::fetch::truncate;
use crate::model::Item;

/// Discord allows at most 10 embeds per message.
const MAX_EMBEDS: usize = 10;
/// Discord caps the combined text of all embeds in one message at 6000 chars.
const MAX_MESSAGE_CHARS: usize = 6000;
/// Discord caps a message's `content` at 2000 chars.
const MAX_CONTENT_CHARS: usize = 2000;
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

/// A message to post, and a plainer one (embeds only) for when Discord refuses
/// components from a webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub payload: Value,
    pub fallback: Option<Value>,
}

/// Whether the style shows pictures, so the notifier looks up `og:image` first.
pub fn shows_images(style: MessageStyle) -> bool {
    style != MessageStyle::Compact
}

pub async fn send(
    ctx: &NotifyContext<'_>,
    webhook_url: &str,
    style: MessageStyle,
    items: &[Item],
) -> Result<(), NotifyError> {
    // Webhooks we didn't create take (non-interactive) components only when asked.
    let sep = if webhook_url.contains('?') { '&' } else { '?' };
    let with_components = format!("{webhook_url}{sep}with_components=true");
    for message in build_messages(ctx.sources, ctx.base_url, style, items) {
        let has_components = message.payload.get("components").is_some();
        let url = if has_components {
            &with_components
        } else {
            webhook_url
        };
        match (
            post(ctx, url, None, &message.payload).await,
            &message.fallback,
        ) {
            (Ok(()), _) => {}
            (Err(PostError::Status(StatusCode::BAD_REQUEST, _)), Some(fallback)) => {
                tracing::warn!("the webhook refused components; sending a plain embed");
                post(ctx, webhook_url, None, fallback)
                    .await
                    .map_err(NotifyError::from)?;
            }
            (Err(err), _) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Posts the items to a channel as the bot.
pub async fn send_bot(
    ctx: &NotifyContext<'_>,
    channel_id: u64,
    style: MessageStyle,
    items: &[Item],
) -> Result<(), NotifyError> {
    let (url, auth) = bot_endpoint(ctx, channel_id)?;
    for message in build_messages(ctx.sources, ctx.base_url, style, items) {
        post(ctx, &url, Some(&auth), &message.payload).await?;
    }
    Ok(())
}

/// Sends a plain message, such as a health alert, through a webhook.
pub async fn send_text(
    ctx: &NotifyContext<'_>,
    webhook_url: &str,
    message: &TextMessage,
) -> Result<(), NotifyError> {
    Ok(post(ctx, webhook_url, None, &text_payload(message)).await?)
}

/// Sends a plain message, such as a health alert, as the bot.
pub async fn send_bot_text(
    ctx: &NotifyContext<'_>,
    channel_id: u64,
    message: &TextMessage,
) -> Result<(), NotifyError> {
    let (url, auth) = bot_endpoint(ctx, channel_id)?;
    Ok(post(ctx, &url, Some(&auth), &text_payload(message)).await?)
}

/// The URL to post a channel's messages to as the bot, and its authorization header.
fn bot_endpoint(ctx: &NotifyContext<'_>, channel_id: u64) -> Result<(String, String), NotifyError> {
    let bot = ctx.discord_bot.ok_or_else(|| {
        NotifyError::Other(
            "the Discord bot is not set up: add a [discord] section to gen.toml".into(),
        )
    })?;
    Ok((
        format!("{}/channels/{channel_id}/messages", bot.api_base),
        format!("Bot {}", bot.token),
    ))
}

/// A plain message: the title in bold, the text and the link, mentioning nobody.
pub fn text_payload(message: &TextMessage) -> Value {
    let content = format!(
        "**{}**\n{}\n{}",
        parts::escape(&message.title),
        parts::escape(&message.text),
        message.link
    );
    json!({
        "content": truncate(&content, MAX_CONTENT_CHARS - 1),
        "allowed_mentions": {"parse": []},
    })
}

/// The messages for `items` in `style`. Compact embeds share messages (up to 10 each);
/// the other styles post an item per message, with its link buttons.
pub fn build_messages(
    sources: &SourcesConfig,
    base_url: &str,
    style: MessageStyle,
    items: &[Item],
) -> Vec<Message> {
    let parts: Vec<Parts> = items
        .iter()
        .map(|i| Parts::from_item(sources, base_url, i))
        .collect();
    if style == MessageStyle::Compact {
        let embeds: Vec<_> = parts.iter().map(embed::compact).collect();
        return chunk_embeds(embeds)
            .into_iter()
            .map(|payload| Message {
                payload,
                fallback: None,
            })
            .collect();
    }
    parts
        .iter()
        .map(|p| {
            let (embed, _) = match style {
                MessageStyle::Image => embed::image(p).unwrap_or_else(|| embed::polished(p)),
                _ => embed::polished(p),
            };
            let plain = with_defaults(json!({ "embeds": [embed] }));
            match style {
                MessageStyle::Card => Message {
                    payload: with_defaults(card::message(p)),
                    fallback: Some(plain),
                },
                _ => match card::buttons(p) {
                    Some(row) => {
                        let mut payload = plain.clone();
                        payload["components"] = json!([row]);
                        Message {
                            payload,
                            fallback: Some(plain),
                        }
                    }
                    None => Message {
                        payload: plain,
                        fallback: None,
                    },
                },
            }
        })
        .collect()
}

/// Messages of at most 10 embeds that stay under Discord's size limit.
fn chunk_embeds(embeds: Vec<(Value, usize)>) -> Vec<Value> {
    let mut payloads = Vec::new();
    let mut chunk: Vec<Value> = Vec::new();
    let mut chars = 0;
    for (embed, len) in embeds {
        if !chunk.is_empty() && (chunk.len() == MAX_EMBEDS || chars + len > MAX_MESSAGE_CHARS) {
            payloads.push(with_defaults(
                json!({ "embeds": std::mem::take(&mut chunk) }),
            ));
            chars = 0;
        }
        chars += len;
        chunk.push(embed);
    }
    if !chunk.is_empty() {
        payloads.push(with_defaults(json!({ "embeds": chunk })));
    }
    payloads
}

/// Settings every message gets: feed text never pings anyone.
fn with_defaults(mut payload: Value) -> Value {
    payload["allowed_mentions"] = json!({ "parse": [] });
    payload
}

#[derive(Debug)]
enum PostError {
    Status(StatusCode, String),
    Other(NotifyError),
}

impl From<PostError> for NotifyError {
    fn from(err: PostError) -> Self {
        match err {
            PostError::Status(status, body) => NotifyError::Other(format!(
                "discord returned {status}: {}",
                truncate(body.trim(), 500)
            )),
            PostError::Other(err) => err,
        }
    }
}

impl From<reqwest::Error> for PostError {
    fn from(err: reqwest::Error) -> Self {
        PostError::Other(err.into())
    }
}

/// POSTs one payload, retrying once after a 429.
async fn post(
    ctx: &NotifyContext<'_>,
    url: &str,
    auth: Option<&str>,
    payload: &Value,
) -> Result<(), PostError> {
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
        return Err(PostError::Status(status, body));
    }
    Err(PostError::Other(NotifyError::Other(
        "discord still rate limited".into(),
    )))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use crate::notify::tests::{item, sources};

    const BASE: &str = "https://gen.example.org/";

    fn one(style: MessageStyle, i: &Item) -> Message {
        let mut messages = build_messages(&sources(), BASE, style, std::slice::from_ref(i));
        assert_eq!(messages.len(), 1);
        messages.remove(0)
    }

    #[test]
    fn polished_embed() {
        let mut i = item(
            &["godot", "x"],
            ItemKind::Release,
            "Godot 4.8",
            Some("Big\n• One"),
        );
        i.image_url = Some("https://example.org/hero.png".into());
        let m = one(MessageStyle::Polished, &i);
        let e = &m.payload["embeds"][0];
        assert_eq!(e["author"]["name"], "Godot, x · New release");
        assert_eq!(e["title"], "Godot 4.8");
        assert_eq!(e["url"], "https://godotengine.org/releases/4.8/");
        assert_eq!(e["description"], "Big\n• One");
        assert_eq!(e["color"], 0x2ECC71);
        assert_eq!(e["thumbnail"]["url"], "https://example.org/hero.png");
        assert_eq!(e["footer"]["text"], "GEN · GEN");
        assert_eq!(e["fields"][0]["name"], "Type");
        assert_eq!(e["timestamp"], i.published.unwrap().to_rfc3339());
        assert_eq!(m.payload["allowed_mentions"]["parse"], json!([]));
        let row = &m.payload["components"][0];
        assert_eq!(row["components"][0]["style"], 5);
        assert_eq!(row["components"][0]["label"], "Release notes");
        assert_eq!(
            row["components"][1]["url"],
            "https://gen.example.org/engine/godot"
        );
        let fallback = m.fallback.unwrap();
        assert!(fallback.get("components").is_none());
        assert_eq!(fallback["embeds"], m.payload["embeds"]);
    }

    #[test]
    fn image_style_falls_back_to_polished_without_an_image() {
        let mut i = item(&["godot"], ItemKind::News, "News", Some("Text"));
        let without = one(MessageStyle::Image, &i);
        assert_eq!(without, one(MessageStyle::Polished, &i));
        i.image_url = Some("https://example.org/hero.png".into());
        let with = one(MessageStyle::Image, &i);
        let e = &with.payload["embeds"][0];
        assert_eq!(e["image"]["url"], "https://example.org/hero.png");
        assert!(e.get("thumbnail").is_none() && e.get("fields").is_none());
    }

    #[test]
    fn compact_embeds_share_messages() {
        let items: Vec<_> = (0..23)
            .map(|n| item(&["godot"], ItemKind::News, &format!("n{n}"), Some("a\n• b")))
            .collect();
        let messages = build_messages(&sources(), BASE, MessageStyle::Compact, &items);
        let sizes: Vec<_> = messages
            .iter()
            .map(|m| m.payload["embeds"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![10, 10, 3]);
        let e = &messages[0].payload["embeds"][0];
        assert_eq!(e["description"], "a b");
        assert!(e.get("thumbnail").is_none() && e.get("fields").is_none());
        assert!(messages[0].payload.get("components").is_none());
        assert!(build_messages(&sources(), BASE, MessageStyle::Compact, &[]).is_empty());
    }

    #[test]
    fn compact_messages_stay_under_the_size_limit() {
        // Many unknown engine ids make a long author line.
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
        let embeds: Vec<_> = items
            .iter()
            .map(|i| embed::compact(&Parts::from_item(&sources(), BASE, i)))
            .collect();
        let total: usize = embeds.iter().map(|(_, len)| len).sum();
        assert!(
            total > MAX_MESSAGE_CHARS,
            "the test needs more than one message"
        );
        let messages = build_messages(&sources(), BASE, MessageStyle::Compact, &items);
        assert!(messages.len() > 1);
        for e in embeds.iter().map(|(e, _)| e) {
            assert!(e["author"]["name"].as_str().unwrap().chars().count() <= 256);
            assert!(e["title"].as_str().unwrap().chars().count() <= 256);
        }
        // Each message's embeds add up to at most 6000 characters.
        let sized = (0..10).map(|n| (json!({ "title": n }), 1000)).collect();
        let sizes: Vec<_> = chunk_embeds(sized)
            .iter()
            .map(|p| p["embeds"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [6, 4]);
    }

    #[test]
    fn card_message() {
        let mut i = item(
            &["godot"],
            ItemKind::Release,
            "Godot *4.8*",
            Some("Big\n• One"),
        );
        let m = one(MessageStyle::Card, &i);
        assert_eq!(m.payload["flags"], card::COMPONENTS_V2);
        assert!(m.payload.get("embeds").is_none() && m.payload.get("content").is_none());
        let container = &m.payload["components"][0];
        assert_eq!(container["type"], 17);
        assert_eq!(container["accent_color"], 0x2ECC71);
        let parts = container["components"].as_array().unwrap();
        assert_eq!(parts[0]["content"], "-# **GODOT · NEW RELEASE**");
        // No image or engine icon in these sources: plain text, no thumbnail section.
        assert_eq!(parts[1]["type"], 10);
        assert_eq!(
            parts[1]["content"],
            "## [Godot \\*4.8\\*](https://godotengine.org/releases/4.8/)\nBig\n• One"
        );
        assert_eq!(parts[2]["content"], "**Type** Stable");
        assert_eq!(parts[3]["type"], 14);
        assert!(
            parts[4]["content"]
                .as_str()
                .unwrap()
                .starts_with("-# GEN · <t:")
        );
        assert_eq!(parts[5]["type"], card::ACTION_ROW);
        assert!(m.fallback.unwrap()["embeds"][0]["title"].is_string());

        i.image_url = Some("https://example.org/hero.png".into());
        i.summary = Some("x".repeat(5000));
        let m = one(MessageStyle::Card, &i);
        let section = &m.payload["components"][0]["components"][1];
        assert_eq!(section["type"], 9);
        assert_eq!(
            section["accessory"]["media"]["url"],
            "https://example.org/hero.png"
        );
        assert!(card::text_len(&m.payload) <= card::MAX_TEXT);
    }

    #[tokio::test]
    async fn posts_as_the_bot() {
        let (addr, requests) = sink(&["200 OK"]).await;
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let bot = BotApi {
            token: "bot-token".into(),
            api_base: format!("http://{addr}/api/v10"),
        };
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: BASE,
            discord_bot: Some(&bot),
        };
        let i = item(&["godot"], ItemKind::Release, "Godot 4.8", None);
        send_bot(&ctx, 42, MessageStyle::Card, &[i]).await.unwrap();

        let request = requests.await.unwrap().remove(0);
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
        assert!(request.contains(r#""flags":32768"#), "{request}");
        assert!(request.contains("Godot 4.8"), "{request}");
    }

    #[tokio::test]
    async fn webhooks_fall_back_when_components_are_refused() {
        let (addr, requests) = sink(&["400 Bad Request", "204 No Content"]).await;
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: BASE,
            discord_bot: None,
        };
        let i = item(&["godot"], ItemKind::Release, "Godot 4.8", None);
        let hook = format!("http://{addr}/api/webhooks/1/x");
        send(&ctx, &hook, MessageStyle::Card, &[i]).await.unwrap();

        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("POST /api/webhooks/1/x?with_components=true "));
        assert!(requests[0].contains(r#""flags":32768"#));
        assert!(requests[1].starts_with("POST /api/webhooks/1/x "));
        assert!(requests[1].contains(r#""embeds":[{"#));
        assert!(!requests[1].contains(r#""components""#));
    }

    /// An HTTP server answering one request per status, returning what it was sent.
    async fn sink(
        statuses: &[&'static str],
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let statuses = statuses.to_vec();
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            for status in statuses {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 32 * 1024];
                let n = conn.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                );
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (addr, task)
    }

    #[tokio::test]
    async fn bot_channels_need_the_bot() {
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: BASE,
            discord_bot: None,
        };
        let i = item(&["godot"], ItemKind::Release, "Godot 4.8", None);
        let err = send_bot(&ctx, 42, MessageStyle::Card, &[i])
            .await
            .unwrap_err()
            .to_string();
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
