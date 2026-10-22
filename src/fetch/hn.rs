//! Hacker News stories via the Algolia search API.

use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use serde::Deserialize;

use super::{Conditional, FetchError, FetchOutcome, Http};
use crate::model::{RawItem, SourceState};

const API_URL: &str = "https://hn.algolia.com/api/v1/search_by_date";

/// Stories with fewer points are dropped as likely spam or noise. Fresh stories that
/// later reach this score are picked up on a following poll while they are still among
/// the most recent results.
pub const MIN_POINTS: u32 = 3;

#[derive(Deserialize)]
struct Search {
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "objectID")]
    object_id: String,
    title: Option<String>,
    url: Option<String>,
    points: Option<u32>,
    num_comments: Option<u32>,
    created_at: Option<DateTime<Utc>>,
}

/// Fetches the newest stories matching `query` (Algolia syntax; quotes force a phrase).
pub async fn fetch(
    http: &Http,
    query: &str,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    let url = url::Url::parse_with_params(
        API_URL,
        [("query", query), ("tags", "story"), ("hitsPerPage", "30")],
    )
    .map_err(|e| FetchError::Parse(format!("hn url: {e}")))?;
    match http
        .get_conditional(url.as_str(), state, HeaderMap::new())
        .await?
    {
        Conditional::NotModified => Ok(FetchOutcome::NotModified),
        Conditional::Body(resp) => Ok(FetchOutcome::Fetched {
            items: parse(&resp.body)?,
            etag: resp.etag,
            last_modified: resp.last_modified,
        }),
    }
}

/// Parses an Algolia search response, dropping untitled stories and those below
/// [`MIN_POINTS`].
pub fn parse(body: &[u8]) -> Result<Vec<RawItem>, FetchError> {
    let search: Search = serde_json::from_slice(body)
        .map_err(|e| FetchError::Parse(format!("hn search json: {e}")))?;
    Ok(search
        .hits
        .into_iter()
        .filter(|h| h.points.unwrap_or(0) >= MIN_POINTS)
        .filter_map(to_item)
        .collect())
}

fn to_item(hit: Hit) -> Option<RawItem> {
    let title = hit.title.filter(|t| !t.trim().is_empty())?;
    let discussion = format!("https://news.ycombinator.com/item?id={}", hit.object_id);
    let url = hit.url.filter(|u| !u.is_empty());
    let mut summary = format!(
        "{} points · {} comments on Hacker News",
        hit.points.unwrap_or(0),
        hit.num_comments.unwrap_or(0)
    );
    if url.is_some() {
        summary.push_str(&format!(": {discussion}"));
    }
    Some(RawItem {
        guid: hit.object_id,
        title,
        url: Some(url.unwrap_or(discussion)),
        summary: Some(summary),
        published: hit.created_at,
        prerelease: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture_and_drops_low_scores() {
        let items = parse(include_bytes!("../../tests/fixtures/hn/search.json")).unwrap();
        let ids: Vec<_> = items.iter().map(|i| i.guid.as_str()).collect();
        assert_eq!(ids, ["50187380", "50127320", "50123777"]);

        let first = &items[0];
        assert_eq!(first.title, "Show HN: Overdesigned Simple Solitaire");
        assert_eq!(first.url.as_deref(), Some("https://defaultsolitaire.app/"));
        assert_eq!(
            first.summary.as_deref(),
            Some(
                "5 points · 9 comments on Hacker News: https://news.ycombinator.com/item?id=50187380"
            )
        );
        assert!(first.published.is_some());
        assert_eq!(first.prerelease, None);
    }

    #[test]
    fn ask_hn_links_to_discussion() {
        let body = br#"{"hits":[{"objectID":"42","title":"Ask HN: Engines?","url":null,
            "points":10,"num_comments":3,"created_at":"2026-10-01T12:00:00Z"}]}"#;
        let items = parse(body).unwrap();
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://news.ycombinator.com/item?id=42")
        );
        assert_eq!(
            items[0].summary.as_deref(),
            Some("10 points · 3 comments on Hacker News")
        );
    }
}
