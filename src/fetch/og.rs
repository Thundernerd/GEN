//! A page's preview image (`og:image`), looked up once per item when notifying, for items
//! whose source gave no image.

use std::time::Duration;

use reqwest::header;

use super::Http;

/// Pages are read up to this many bytes: the meta tags are in the `<head>`.
const MAX_BYTES: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// The page's `og:image` (or `twitter:image`), as an absolute http(s) URL. Any failure
/// means no image.
pub async fn og_image(http: &Http, page_url: &str) -> Option<String> {
    let mut resp = http
        .client
        .get(page_url)
        .header(header::ACCEPT, "text/html")
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_BYTES {
            body.truncate(MAX_BYTES);
            break;
        }
    }
    find_image(&String::from_utf8_lossy(&body), page_url)
}

/// The preview image named in a page's meta tags, resolved against the page's URL.
pub fn find_image(html: &str, page_url: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let content = |selector: &str| {
        let selector = scraper::Selector::parse(selector).expect("valid selector");
        doc.select(&selector)
            .find_map(|m| m.value().attr("content").map(str::trim))
            .filter(|c| !c.is_empty())
            .map(str::to_string)
    };
    let found = content(r#"meta[property="og:image"]"#)
        .or_else(|| content(r#"meta[property="og:image:url"]"#))
        .or_else(|| content(r#"meta[name="twitter:image"]"#))?;
    absolute_url(page_url, &found)
}

/// `url` resolved against `base`, if it is http(s).
pub fn absolute_url(base: &str, url: &str) -> Option<String> {
    let resolved = match url::Url::parse(base) {
        Ok(base) => base.join(url).ok()?,
        Err(_) => url::Url::parse(url).ok()?,
    };
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_og_then_twitter_images() {
        let page = r#"<html><head>
            <meta name="twitter:image" content="https://cdn.example.org/t.png">
            <meta property="og:image" content="/img/hero.jpg">
            </head><body></body></html>"#;
        assert_eq!(
            find_image(page, "https://example.org/news/post").as_deref(),
            Some("https://example.org/img/hero.jpg")
        );
        let twitter = r#"<meta name="twitter:image" content="https://cdn.example.org/t.png">"#;
        assert_eq!(
            find_image(twitter, "https://example.org/").as_deref(),
            Some("https://cdn.example.org/t.png")
        );
        assert_eq!(find_image("<p>no meta</p>", "https://example.org/"), None);
        let script = r#"<meta property="og:image" content="javascript:alert(1)">"#;
        assert_eq!(find_image(script, "https://example.org/"), None);
    }

    #[tokio::test]
    async fn fetches_the_page_and_stops_at_the_size_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for page in [
                r#"<head><meta property="og:image" content="/a.png"></head>"#.to_string(),
                // The tag comes after the limit, so it isn't read.
                format!(
                    "<head>{}<meta property=\"og:image\" content=\"/late.png\"></head>",
                    " ".repeat(MAX_BYTES + 10)
                ),
            ] {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = conn.read(&mut buf).await.unwrap();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    page.len()
                );
                conn.write_all(head.as_bytes()).await.unwrap();
                let _ = conn.write_all(page.as_bytes()).await;
            }
        });
        let http = Http::new("gen-test", None).unwrap();
        let url = format!("http://{addr}/post");
        assert_eq!(
            og_image(&http, &url).await,
            Some(format!("http://{addr}/a.png"))
        );
        assert_eq!(og_image(&http, &url).await, None);
    }
}
