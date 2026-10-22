//! ntfy push notifications.

use reqwest::StatusCode;

use super::{NotifyContext, NotifyError, kind_shortcode, prefixed_title};
use crate::config::SourcesConfig;
use crate::fetch::truncate;
use crate::model::{Item, ItemKind};

/// Max body length in chars.
const MAX_BODY: usize = 1000;

pub async fn send(
    ctx: &NotifyContext<'_>,
    server: &str,
    topic: &str,
    token: Option<&str>,
    items: &[Item],
) -> Result<(), NotifyError> {
    let url = topic_url(server, topic);
    for item in items {
        let msg = build_message(ctx.sources, item);
        let mut req = ctx.http.client.post(&url).body(msg.body);
        for (name, value) in msg.headers {
            req = req.header(name, value);
        }
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = if status == StatusCode::TOO_MANY_REQUESTS {
                " (rate limited)"
            } else {
                ""
            };
            return Err(NotifyError::Other(format!(
                "ntfy returned {status}{hint}: {}",
                truncate(body.trim(), 500)
            )));
        }
    }
    Ok(())
}

/// `{server}/{topic}` without doubled slashes.
pub fn topic_url(server: &str, topic: &str) -> String {
    format!(
        "{}/{}",
        server.trim_end_matches('/'),
        topic.trim_start_matches('/')
    )
}

/// One ntfy publish request: body plus ASCII-safe headers.
#[derive(Debug)]
pub struct Message {
    pub body: String,
    pub headers: Vec<(&'static str, String)>,
}

pub fn build_message(sources: &SourcesConfig, item: &Item) -> Message {
    let body = item
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&item.title);
    // ntfy turns bodies over 4 KB into attachments; keep it well below that.
    let body = truncate(body, MAX_BODY);
    let mut tags = vec![kind_shortcode(item.kind).to_string()];
    tags.extend(item.engines.iter().cloned());
    let priority = if item.kind == ItemKind::Release {
        "4"
    } else {
        "3"
    };
    let mut headers = vec![
        ("Title", encode_header(&prefixed_title(sources, item))),
        ("Tags", encode_header(&tags.join(","))),
        ("Priority", priority.to_string()),
    ];
    if let Some(url) = &item.url {
        // A parsed URL is pure ASCII (punycode host, percent-encoded path).
        let click = url::Url::parse(url).map_or_else(|_| url.clone(), String::from);
        headers.push(("Click", encode_header(&click)));
    }
    Message { body, headers }
}

/// Returns `value` unchanged when it is a valid plain header value, otherwise RFC 2047
/// `=?UTF-8?B?...?=` encoding (which ntfy decodes for Title, Tags, Click, ...).
pub fn encode_header(value: &str) -> String {
    if value.bytes().all(|b| (0x20..0x7f).contains(&b)) && !value.contains("=?") {
        value.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", base64(value.as_bytes()))
    }
}

/// Standard base64 with padding.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::{item, sources};

    fn header<'a>(m: &'a Message, name: &str) -> Option<&'a str> {
        m.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn encodes_non_ascii_headers() {
        assert_eq!(encode_header("[Godot] 4.8"), "[Godot] 4.8");
        assert_eq!(encode_header("héllo"), "=?UTF-8?B?aMOpbGxv?=");
        assert!(encode_header("a\nb").starts_with("=?UTF-8?B?"));
        assert!(encode_header("=?x?=").starts_with("=?UTF-8?B?"));
        for (_, v) in build_message(
            &sources(),
            &item(&["godot"], ItemKind::News, "Ünïcode 🚀", None),
        )
        .headers
        {
            assert!(reqwest::header::HeaderValue::from_str(&v).is_ok(), "{v}");
        }
    }

    #[test]
    fn release_message() {
        let m = build_message(
            &sources(),
            &item(&["godot"], ItemKind::Release, "Godot 4.8", Some("Notes")),
        );
        assert_eq!(m.body, "Notes");
        assert_eq!(header(&m, "Title"), Some("[Godot] Godot 4.8"));
        assert_eq!(header(&m, "Tags"), Some("rocket,godot"));
        assert_eq!(header(&m, "Priority"), Some("4"));
        assert_eq!(
            header(&m, "Click"),
            Some("https://godotengine.org/releases/4.8/")
        );
    }

    #[test]
    fn untagged_message_without_summary_or_url() {
        let mut i = item(&[], ItemKind::Community, "Thread", Some("  "));
        i.url = None;
        let m = build_message(&sources(), &i);
        assert_eq!(m.body, "Thread");
        assert_eq!(header(&m, "Title"), Some("Thread"));
        assert_eq!(header(&m, "Tags"), Some("speech_balloon"));
        assert_eq!(header(&m, "Priority"), Some("3"));
        assert_eq!(header(&m, "Click"), None);
    }

    #[test]
    fn click_is_ascii() {
        let mut i = item(&[], ItemKind::News, "t", None);
        i.url = Some("https://exämple.org/ä".into());
        let m = build_message(&sources(), &i);
        assert_eq!(
            header(&m, "Click"),
            Some("https://xn--exmple-cua.org/%C3%A4")
        );
    }

    #[test]
    fn joins_topic_url() {
        assert_eq!(topic_url("https://ntfy.sh/", "gen"), "https://ntfy.sh/gen");
        assert_eq!(topic_url("https://ntfy.sh", "gen"), "https://ntfy.sh/gen");
    }
}
