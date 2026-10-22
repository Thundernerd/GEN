//! Scraped HTML pages, using CSS selectors from config.
//!
//! A fallback for sites without feeds. Only server-rendered markup is visible; pages that
//! build their content with JavaScript yield no items.

use chrono::{DateTime, NaiveDate, Utc};
use reqwest::header::HeaderMap;
use scraper::{ElementRef, Html, Selector};
use url::Url;

use super::{Conditional, FetchError, FetchOutcome, Http};
use crate::model::{RawItem, SourceState};

/// CSS selectors; `title`, `link` and `date` are relative to `item`.
pub struct Selectors<'a> {
    pub item: &'a str,
    pub title: &'a str,
    pub link: &'a str,
    pub date: Option<&'a str>,
}

/// Fetches `url` and extracts one item per element matching `selectors.item`.
pub async fn fetch(
    http: &Http,
    url: &str,
    selectors: &Selectors<'_>,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    match http.get_conditional(url, state, HeaderMap::new()).await? {
        Conditional::NotModified => Ok(FetchOutcome::NotModified),
        Conditional::Body(resp) => Ok(FetchOutcome::Fetched {
            items: parse(&String::from_utf8_lossy(&resp.body), url, selectors)?,
            etag: resp.etag,
            last_modified: resp.last_modified,
        }),
    }
}

/// Extracts items from `html`, resolving links against `page_url` (or the page's
/// `<base href>`). Items without a title are skipped.
pub fn parse(
    html: &str,
    page_url: &str,
    selectors: &Selectors<'_>,
) -> Result<Vec<RawItem>, FetchError> {
    let item_sel = selector(selectors.item, "item")?;
    let title_sel = selector(selectors.title, "title")?;
    let link_sel = selector(selectors.link, "link")?;
    let date_sel = selectors.date.map(|d| selector(d, "date")).transpose()?;
    let base_sel = selector("base[href]", "base")?;

    let doc = Html::parse_document(html);
    let page = Url::parse(page_url)
        .map_err(|e| FetchError::Parse(format!("invalid page url `{page_url}`: {e}")))?;
    let base = doc
        .select(&base_sel)
        .next()
        .and_then(|b| b.value().attr("href"))
        .and_then(|href| page.join(href).ok())
        .unwrap_or(page);

    let items = doc
        .select(&item_sel)
        .filter_map(|item| {
            let title = first(item, &title_sel).map(|e| collapse(&e.text().collect::<String>()))?;
            if title.is_empty() {
                return None;
            }
            let url = first(item, &link_sel)
                .and_then(|e| e.value().attr("href"))
                .and_then(|href| base.join(href.trim()).ok())
                .map(String::from);
            let published = date_sel.as_ref().and_then(|sel| {
                let el = first(item, sel)?;
                match el.value().attr("datetime") {
                    Some(dt) => parse_date(dt),
                    None => parse_date(&collapse(&el.text().collect::<String>())),
                }
            });
            Some(RawItem {
                guid: url.clone().unwrap_or_else(|| title.clone()),
                title,
                url,
                summary: None,
                published,
                prerelease: None,
            })
        })
        .collect();
    Ok(items)
}

/// Checks that `css` is a valid selector. The error is a message for the user.
pub fn check_selector(css: &str, what: &str) -> Result<(), String> {
    selector(css, what).map(|_| ()).map_err(|e| match e {
        FetchError::Parse(msg) => msg,
        other => other.to_string(),
    })
}

fn selector(css: &str, what: &str) -> Result<Selector, FetchError> {
    Selector::parse(css)
        .map_err(|e| FetchError::Parse(format!("invalid {what} selector `{css}`: {e}")))
}

/// The item itself if it matches, else its first matching descendant.
fn first<'a>(item: ElementRef<'a>, sel: &Selector) -> Option<ElementRef<'a>> {
    if sel.matches(&item) {
        Some(item)
    } else {
        item.select(sel).next()
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses common date formats; date-only values become midnight UTC.
fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Utc));
    }
    ["%Y-%m-%d", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y"]
        .iter()
        .find_map(|fmt| NaiveDate::parse_from_str(s, fmt).ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTORS: Selectors<'static> = Selectors {
        item: "article.post",
        title: ".post-title",
        link: "a",
        date: Some("time, .date"),
    };

    #[test]
    fn parses_fixture() {
        let html = include_str!("../../tests/fixtures/html/news.html");
        let items = parse(html, "https://example.com/blog/index.html", &SELECTORS).unwrap();
        assert_eq!(items.len(), 4);

        assert_eq!(items[0].title, "Engine 2.0 released");
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://example.com/news/engine-2-0")
        );
        assert_eq!(items[0].guid, "https://example.com/news/engine-2-0");
        assert_eq!(
            items[0].published.unwrap().to_rfc3339(),
            "2026-10-20T14:30:00+00:00"
        );

        assert_eq!(
            items[1].url.as_deref(),
            Some("https://example.com/blog/beta-notes.html")
        );
        assert_eq!(
            items[1].published.unwrap().to_rfc3339(),
            "2026-09-05T00:00:00+00:00"
        );

        assert_eq!(
            items[2].url.as_deref(),
            Some("https://example.org/external")
        );
        assert_eq!(
            items[2].published.unwrap().to_rfc3339(),
            "2026-08-12T00:00:00+00:00"
        );

        assert_eq!(items[3].guid, "No link here");
        assert_eq!(items[3].url, None);
        assert_eq!(items[3].published, None);
    }

    #[test]
    fn honours_base_href() {
        let html = r#"<html><head><base href="/docs/"></head><body>
            <a class="x" href="page">Page</a></body></html>"#;
        let sel = Selectors {
            item: "a.x",
            title: "a",
            link: "a",
            date: None,
        };
        let items = parse(html, "https://example.com/a/b", &sel).unwrap();
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://example.com/docs/page")
        );
    }

    #[test]
    fn invalid_selector_is_parse_error() {
        let sel = Selectors {
            item: "div[",
            ..SELECTORS
        };
        let err = parse("", "https://example.com/", &sel).unwrap_err();
        assert!(matches!(&err, FetchError::Parse(m) if m.contains("item selector")));
    }

    #[test]
    fn parses_date_formats() {
        for s in [
            "2026-01-02",
            "January 2, 2026",
            "Jan 2, 2026",
            "2 January 2026",
            "Fri, 02 Jan 2026 00:00:00 +0000",
            "2026-01-02T00:00:00Z",
        ] {
            assert_eq!(
                parse_date(s).map(|d| d.to_rfc3339()).as_deref(),
                Some("2026-01-02T00:00:00+00:00"),
                "{s}"
            );
        }
        assert_eq!(parse_date("soon"), None);
    }
}
