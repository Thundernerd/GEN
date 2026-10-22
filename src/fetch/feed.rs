//! RSS / Atom / JSON Feed sources (via `feed-rs`).
//!
//! The body is parsed by sniffing its contents, never by trusting `Content-Type`: Unreal
//! serves Atom as `text/html` and Unity labels RSS as `application/atom+xml`.

use reqwest::header::{self, HeaderMap, HeaderValue};

use super::{Conditional, FetchError, FetchOutcome, Http, html_to_text, truncate};
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
    let summary = entry
        .summary
        .map(|t| t.content)
        .or_else(|| entry.content.and_then(|c| c.body))
        .map(|s| truncate(&html_to_text(&s), SUMMARY_MAX))
        .filter(|s| !s.is_empty());
    let mut title = entry
        .title
        .map(|t| html_to_text(&t.content))
        .unwrap_or_default();
    if title.is_empty() {
        title = summary
            .as_deref()
            .map(|s| truncate(s, TITLE_FROM_SUMMARY_MAX))
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
    })
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
