//! Apprise API notifications: a self-hosted relay that forwards to the services it's
//! configured for. Either a configuration saved on the server (`POST /notify/{key}`,
//! optionally narrowed by a tag) or Apprise URLs sent along (`POST /notify`).

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::{NotifyContext, NotifyError, kind_emoji, prefixed_title};
use crate::config::SourcesConfig;
use crate::fetch::truncate;
use crate::model::{Item, ItemKind};

/// Max body length in chars, so it fits most services Apprise forwards to.
const MAX_BODY: usize = 1000;

/// Where and how to reach the Apprise API.
pub struct Target<'a> {
    pub server: &'a str,
    pub key: Option<&'a str>,
    pub urls: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
}

pub async fn send(
    ctx: &NotifyContext<'_>,
    target: &Target<'_>,
    items: &[Item],
) -> Result<(), NotifyError> {
    let url = notify_url(target.server, target.key)?;
    for item in items {
        let mut payload = build_payload(ctx.sources, item);
        match (target.key, target.urls) {
            (Some(_), _) => {
                if let Some(tag) = target.tag.filter(|t| !t.is_empty()) {
                    payload["tag"] = json!(tag);
                }
            }
            (None, Some(urls)) => payload["urls"] = json!(urls),
            (None, None) => return Err(NotifyError::Other("no Apprise key or URLs".into())),
        }
        let mut req = ctx.http.client.post(url.clone()).json(&payload);
        if target.username.is_some() || target.password.is_some() {
            req = req.basic_auth(target.username.unwrap_or_default(), target.password);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status {
                StatusCode::UNAUTHORIZED => " (check the username and password)",
                StatusCode::NOT_FOUND => " (no configuration saved under this key?)",
                StatusCode::FAILED_DEPENDENCY => " (a service failed, or the tag matched nothing)",
                StatusCode::TOO_MANY_REQUESTS => " (rate limited)",
                _ => "",
            };
            return Err(NotifyError::Other(format!(
                "Apprise returned {status}{hint}: {}",
                truncate(body.trim(), 500)
            )));
        }
    }
    Ok(())
}

/// `{server}/notify/{key}` for a saved configuration, `{server}/notify` otherwise.
pub fn notify_url(server: &str, key: Option<&str>) -> Result<url::Url, NotifyError> {
    let mut url = url::Url::parse(server)
        .map_err(|e| NotifyError::Other(format!("invalid Apprise server URL: {e}")))?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| NotifyError::Other("invalid Apprise server URL".into()))?;
        path.pop_if_empty().push("notify");
        if let Some(key) = key {
            path.push(key);
        }
    }
    Ok(url)
}

/// The JSON Apprise expects: a title, a Markdown body with the summary and a link, and a
/// message type that services use for colour or icon.
pub fn build_payload(sources: &SourcesConfig, item: &Item) -> Value {
    let title = format!(
        "{} {}",
        kind_emoji(item.kind),
        prefixed_title(sources, item)
    );
    let summary = item
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(s, MAX_BODY));
    let link = item.url.as_deref().map(|u| format!("[Open]({u})"));
    let body = match (summary, link) {
        (Some(s), Some(l)) => format!("{s}\n\n{l}"),
        (Some(s), None) => s,
        (None, Some(l)) => l,
        // Apprise requires a body.
        (None, None) => item.title.clone(),
    };
    let kind = match item.kind {
        ItemKind::Release => "success",
        _ => "info",
    };
    json!({"title": title, "body": body, "type": kind, "format": "markdown"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::{item, sources};

    #[test]
    fn builds_markdown_payloads() {
        let p = build_payload(
            &sources(),
            &item(&["godot"], ItemKind::Release, "Godot 4.8", Some("Notes")),
        );
        assert_eq!(p["title"], "🚀 [Godot] Godot 4.8");
        assert_eq!(
            p["body"],
            "Notes\n\n[Open](https://godotengine.org/releases/4.8/)"
        );
        assert_eq!(p["type"], "success");
        assert_eq!(p["format"], "markdown");

        let mut bare = item(&[], ItemKind::News, "Thread", None);
        bare.url = None;
        let p = build_payload(&sources(), &bare);
        assert_eq!(p["body"], "Thread", "never empty");
        assert_eq!(p["type"], "info");
    }

    /// Serves one request with `status` and returns the raw request it received.
    async fn sink(status: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = conn.read(&mut buf).await.unwrap();
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-length: 9\r\nconnection: close\r\n\r\nno tagged"
            );
            conn.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (format!("http://{addr}"), task)
    }

    #[tokio::test]
    async fn posts_to_a_saved_configuration() {
        let (server, request) = sink("200 OK").await;
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: "https://gen.example.org/",
        };
        let target = Target {
            server: &server,
            key: Some("gen"),
            urls: None,
            tag: Some("releases"),
            username: Some("gen"),
            password: Some("pw"),
        };
        let release = item(&["godot"], ItemKind::Release, "Godot 4.8", Some("Notes"));
        send(&ctx, &target, &[release]).await.unwrap();
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /notify/gen "), "{request}");
        // "gen:pw" in base64.
        assert!(
            request
                .to_lowercase()
                .contains("authorization: basic z2vuonb3"),
            "{request}"
        );
        assert!(request.contains(r#""tag":"releases""#), "{request}");
        assert!(!request.contains(r#""urls""#), "{request}");
    }

    #[tokio::test]
    async fn reports_failed_deliveries() {
        let (server, _request) = sink("424 Failed Dependency").await;
        let http = crate::fetch::Http::new("gen-test", None).unwrap();
        let ctx = NotifyContext {
            http: &http,
            sources: &sources(),
            base_url: "https://gen.example.org/",
        };
        let target = Target {
            server: &server,
            key: None,
            urls: Some("json://localhost"),
            tag: None,
            username: None,
            password: None,
        };
        let err = send(&ctx, &target, &[item(&[], ItemKind::News, "t", None)])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("424") && err.contains("tag matched nothing"),
            "{err}"
        );
    }

    #[test]
    fn builds_notify_urls() {
        let url = |s, k| notify_url(s, k).unwrap().to_string();
        assert_eq!(
            url("https://apprise.lan", Some("gen")),
            "https://apprise.lan/notify/gen"
        );
        assert_eq!(
            url("https://apprise.lan/", None),
            "https://apprise.lan/notify"
        );
        assert_eq!(
            url("https://x.lan/apprise/", Some("a b")),
            "https://x.lan/apprise/notify/a%20b"
        );
        assert!(notify_url("not a url", None).is_err());
    }
}
