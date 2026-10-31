//! RSS / Atom / JSON Feed sources (via `feed-rs`).
//!
//! The body is parsed by sniffing its contents, never by trusting `Content-Type`: Unreal
//! serves Atom as `text/html` and Unity labels RSS as `application/atom+xml`.

use reqwest::header::{self, HeaderMap, HeaderValue};

use super::{Conditional, FetchError, FetchOutcome, Http, html_to_lines, html_to_text, truncate};
use crate::model::{RawItem, SourceState};

/// Max chars kept from an entry summary.
const SUMMARY_MAX: usize = 500;
/// Max chars of summary used as a title when the feed has none (Bluesky, Mastodon).
const TITLE_FROM_SUMMARY_MAX: usize = 120;

/// Polls a feed, optionally bypassing intermediate caches.
pub async fn fetch(
    http: &Http,
    url: &str,
    cache_bust: bool,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    let mut headers = HeaderMap::new();
    let url = if cache_bust {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        cache_busted(url, chrono::Utc::now().timestamp() / 3600)
    } else {
        url.to_string()
    };
    match http.get_conditional(&url, state, headers).await? {
        Conditional::NotModified => Ok(FetchOutcome::NotModified),
        Conditional::Body(resp) => Ok(FetchOutcome::Fetched {
            items: parse(&resp.body)?,
            etag: resp.etag,
            last_modified: resp.last_modified,
        }),
    }
}

/// Appends `_gen=<unix-hour>` so caches such as wp-super-cache serve a fresh copy
/// (CG Channel's plain `/feed/` is otherwise stale).
fn cache_busted(url: &str, hour: i64) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}_gen={hour}")
}

/// Parses an RSS / Atom / JSON Feed document into raw items.
pub fn parse(body: &[u8]) -> Result<Vec<RawItem>, FetchError> {
    let feed = feed_rs::parser::parse(body).map_err(|e| FetchError::Parse(e.to_string()))?;
    Ok(feed.entries.into_iter().filter_map(map_entry).collect())
}

/// Maps one entry; `None` when it has neither a title nor a link.
fn map_entry(entry: feed_rs::model::Entry) -> Option<RawItem> {
    let url = best_link(&entry.links);
    let html = entry
        .summary
        .map(|t| t.content)
        .or_else(|| entry.content.as_ref().and_then(|c| c.body.clone()));
    let image_url = media_image(&entry.media)
        .or_else(|| enclosure_image(&entry.links))
        .or_else(|| html.as_deref().and_then(first_img))
        .and_then(|img| super::og::absolute_url(url.as_deref().unwrap_or_default(), &img));
    let summary = html
        .map(|s| truncate(&html_to_lines(&s), SUMMARY_MAX))
        .filter(|s| !s.is_empty());
    let mut title = entry
        .title
        .map(|t| html_to_text(&t.content))
        .unwrap_or_default();
    if title.is_empty() {
        title = summary
            .as_deref()
            .map(|s| truncate(&s.replace('\n', " "), TITLE_FROM_SUMMARY_MAX))
            .unwrap_or_default();
    }
    if title.is_empty() && url.is_none() {
        return None;
    }
    let guid = if entry.id.trim().is_empty() {
        url.clone()?
    } else {
        entry.id
    };
    Some(RawItem {
        guid,
        title,
        url,
        summary,
        published: entry.published.or(entry.updated),
        prerelease: None,
        image_url,
    })
}

/// A thumbnail, or image content, from Media RSS (YouTube, many blogs) or an RSS
/// `<enclosure>`, which feed-rs reads as media content. (feed-rs keeps a thumbnail only
/// next to media content, as in `<media:group>`, not on its own.)
fn media_image(media: &[feed_rs::model::MediaObject]) -> Option<String> {
    let thumbnail = media
        .iter()
        .flat_map(|m| &m.thumbnails)
        .map(|t| t.image.uri.clone())
        .find(|u| !u.trim().is_empty());
    thumbnail.or_else(|| {
        media
            .iter()
            .flat_map(|m| &m.content)
            .filter(|c| match &c.content_type {
                Some(t) => t.to_string().starts_with("image/"),
                None => c.url.as_ref().is_some_and(|u| looks_like_image(u.path())),
            })
            .find_map(|c| c.url.as_ref().map(|u| u.to_string()))
    })
}

fn looks_like_image(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// An Atom `<link rel="enclosure">` to an image.
fn enclosure_image(links: &[feed_rs::model::Link]) -> Option<String> {
    links
        .iter()
        .find(|l| {
            l.rel.as_deref() == Some("enclosure")
                && l.media_type
                    .as_deref()
                    .is_some_and(|t| t.starts_with("image/"))
        })
        .map(|l| l.href.trim().to_string())
}

/// The first `<img src>` in an entry's HTML, skipping tracking pixels and emoji.
fn first_img(html: &str) -> Option<String> {
    let fragment = scraper::Html::parse_fragment(html);
    let selector = scraper::Selector::parse("img[src]").expect("valid selector");
    fragment
        .select(&selector)
        .filter(|img| {
            let small = |attr| {
                img.value()
                    .attr(attr)
                    .and_then(|v: &str| v.trim_end_matches("px").parse::<u32>().ok())
                    .is_some_and(|v| v < 64)
            };
            !small("width") && !small("height")
        })
        .filter_map(|img| img.value().attr("src"))
        .map(str::trim)
        .find(|src| !src.is_empty() && !src.starts_with("data:"))
        .map(str::to_string)
}

/// Prefers `rel="alternate"` (or no rel), else the first link.
fn best_link(links: &[feed_rs::model::Link]) -> Option<String> {
    links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
        .or_else(|| links.first())
        .map(|l| l.href.trim().to_string())
        .filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_images_in_media_enclosures_and_content() {
        let rss = br#"<?xml version="1.0"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/">
<channel><title>t</title><link>https://example.org/</link>
<item><title>Thumb</title><link>https://example.org/a</link><guid>a</guid>
  <media:group><media:content url="https://cdn.example.org/a.mp4" type="video/mp4"/>
  <media:thumbnail url="https://cdn.example.org/a.jpg"/></media:group></item>
<item><title>Untyped</title><link>https://example.org/e</link><guid>e</guid>
  <media:content url="https://cdn.example.org/e.JPG" medium="image"/></item>
<item><title>Enclosure</title><link>https://example.org/b</link><guid>b</guid>
  <enclosure url="https://cdn.example.org/b.png" type="image/png" length="1"/></item>
<item><title>Img</title><link>https://example.org/c</link><guid>c</guid>
  <description><![CDATA[<p><img src="/pixel.gif" width="1"> <img src="/img/c.webp"></p><p>Body</p><ul><li>One</li></ul>]]></description></item>
<item><title>Audio</title><link>https://example.org/d</link><guid>d</guid>
  <enclosure url="https://cdn.example.org/d.mp3" type="audio/mpeg" length="1"/></item>
</channel></rss>"#;
        let items = parse(rss).unwrap();
        let images: Vec<_> = items.iter().map(|i| i.image_url.as_deref()).collect();
        assert_eq!(
            images,
            [
                Some("https://cdn.example.org/a.jpg"),
                Some("https://cdn.example.org/e.JPG"),
                Some("https://cdn.example.org/b.png"),
                Some("https://example.org/img/c.webp"),
                None
            ]
        );
        assert_eq!(items[3].summary.as_deref(), Some("Body\n• One"));
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/feed/",
                $name
            ))
        };
    }

    #[test]
    fn cache_bust_respects_existing_query() {
        assert_eq!(cache_busted("https://a/feed/", 7), "https://a/feed/?_gen=7");
        assert_eq!(cache_busted("https://a/?x=1", 7), "https://a/?x=1&_gen=7");
    }

    #[test]
    fn godot_rss() {
        let items = parse(fixture!("godot.xml")).unwrap();
        assert_eq!(items.len(), 4);
        let first = &items[0];
        assert_eq!(first.title, "Dev snapshot: Godot 4.8 beta 1");
        assert_eq!(
            first.url.as_deref(),
            Some("https://godotengine.org/article/dev-snapshot-godot-4-8-beta-1/")
        );
        assert!(first.published.is_some());
        let summary = first.summary.as_deref().unwrap();
        assert!(!summary.contains('<'), "summary has html: {summary}");
        assert!(summary.chars().count() <= SUMMARY_MAX + 1);
        assert!(items.iter().all(|i| i.prerelease.is_none()));
    }

    #[test]
    fn unreal_atom_served_as_html_has_plain_titles() {
        let items = parse(fixture!("unreal.xml")).unwrap();
        assert!(!items.is_empty());
        assert_eq!(
            items[0].title,
            "Check out the top Fortnite islands from the UEFN Student Bootcamp"
        );
        for item in &items {
            assert!(!item.title.contains('<'), "html in title: {}", item.title);
            assert!(item.url.as_deref().unwrap().starts_with("https://"));
        }
        assert!(items[1].title.starts_with("SILENT HILL: Townfall delivers"));
        assert!(items[0].published.is_some(), "RFC 822 date in Atom");
    }

    #[test]
    fn wordpress_rss() {
        let items = parse(fixture!("flax.xml")).unwrap();
        let first = &items[0];
        assert_eq!(first.title, "Flax 1.13 released");
        assert_eq!(first.guid, "https://flaxengine.com/?p=2587");
        assert_eq!(
            first.url.as_deref(),
            Some("https://flaxengine.com/blog/flax-1-13-released/")
        );
        assert!(first.published.is_some());
        assert!(first.summary.as_deref().unwrap().contains("Flax 1.13"));
    }

    #[test]
    fn bluesky_rss_without_titles_uses_summary() {
        let items = parse(fixture!("bluesky.xml")).unwrap();
        assert!(!items.is_empty());
        let first = &items[0];
        assert!(
            first
                .title
                .starts_with("#GodotEngine will support spatial containers"),
            "{}",
            first.title
        );
        assert!(first.title.chars().count() <= TITLE_FROM_SUMMARY_MAX + 1);
        assert!(first.guid.starts_with("at://"));
        assert!(items.iter().all(|i| !i.title.is_empty()));
    }

    #[test]
    fn invalid_body_is_parse_error() {
        assert!(matches!(
            parse(b"<html><body>nope</body></html>"),
            Err(FetchError::Parse(_))
        ));
    }
}
