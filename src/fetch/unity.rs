//! Unity editor releases from the public releases JSON API.
//!
//! The API returns every release with its full download/module tree, which is large
//! (~90 KB per release). Filtering on `platform=LINUX` trims each entry to one platform's
//! downloads without dropping releases, and the response carries an ETag, so repeat polls
//! are usually a cheap 304.

use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use serde::Deserialize;

use super::{Conditional, FetchError, FetchOutcome, Http};
use crate::model::{RawItem, SourceState};

const API_URL: &str = "https://services.api.unity.com/unity/editor/release/v1/releases";

#[derive(Deserialize)]
struct Page {
    results: Vec<Release>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    version: String,
    release_date: Option<DateTime<Utc>>,
    /// `LTS`, `SUPPORTED`, `TECH`, `BETA` or `ALPHA`.
    stream: Option<String>,
}

/// Fetches the newest `limit` editor releases.
pub async fn fetch(
    http: &Http,
    limit: u32,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    let url = format!("{API_URL}?limit={limit}&offset=0&order=RELEASE_DATE_DESC&platform=LINUX");
    match http.get_conditional(&url, state, HeaderMap::new()).await? {
        Conditional::NotModified => Ok(FetchOutcome::NotModified),
        Conditional::Body(resp) => Ok(FetchOutcome::Fetched {
            items: parse(&resp.body)?,
            etag: resp.etag,
            last_modified: resp.last_modified,
        }),
    }
}

/// Parses a releases API response into items, newest first.
pub fn parse(body: &[u8]) -> Result<Vec<RawItem>, FetchError> {
    let page: Page = serde_json::from_slice(body)
        .map_err(|e| FetchError::Parse(format!("unity releases json: {e}")))?;
    Ok(page.results.into_iter().map(to_item).collect())
}

fn to_item(r: Release) -> RawItem {
    let stream = r.stream.as_deref().unwrap_or("").to_ascii_uppercase();
    let suffix = version_suffix(&r.version);
    let prerelease = matches!(stream.as_str(), "ALPHA" | "BETA") || matches!(suffix, 'a' | 'b');
    let (label, page) = match (stream.as_str(), suffix) {
        ("ALPHA", _) | (_, 'a') => ("Alpha", "alpha"),
        ("BETA", _) | (_, 'b') => ("Beta", "beta"),
        ("LTS", _) => ("LTS", "whats-new"),
        ("TECH", _) => ("Tech stream", "whats-new"),
        ("SUPPORTED", _) => ("Supported", "whats-new"),
        _ => ("Stable", "whats-new"),
    };
    RawItem {
        title: format!("Unity {} ({label})", r.version),
        url: Some(format!(
            "https://unity.com/releases/editor/{page}/{}",
            r.version
        )),
        summary: Some(format!("Unity {label} release")),
        published: r.release_date,
        prerelease: Some(prerelease),
        image_url: None,
        guid: r.version,
    }
}

/// The letter after the numeric `major.minor.patch` part, e.g. `a` in `7000.0.0a12`
/// (`a` alpha, `b` beta, `f` final, `p` patch). `'\0'` when there is none.
fn version_suffix(version: &str) -> char {
    version
        .chars()
        .find(|c| c.is_ascii_alphabetic())
        .map_or('\0', |c| c.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixture() {
        let items = parse(include_bytes!("../../tests/fixtures/unity/releases.json")).unwrap();
        assert_eq!(items.len(), 6);

        let alpha = &items[0];
        assert_eq!(alpha.guid, "7000.0.0a12");
        assert_eq!(alpha.title, "Unity 7000.0.0a12 (Alpha)");
        assert_eq!(
            alpha.url.as_deref(),
            Some("https://unity.com/releases/editor/alpha/7000.0.0a12")
        );
        assert_eq!(alpha.prerelease, Some(true));
        assert_eq!(
            alpha.published.unwrap().to_rfc3339(),
            "2026-10-22T09:56:27.460+00:00"
        );

        let supported = &items[1];
        assert_eq!(supported.guid, "6000.6.7f1");
        assert_eq!(supported.prerelease, Some(false));
        assert_eq!(
            supported.url.as_deref(),
            Some("https://unity.com/releases/editor/whats-new/6000.6.7f1")
        );

        assert_eq!(items[2].prerelease, Some(true));
        assert_eq!(items[2].summary.as_deref(), Some("Unity Beta release"));

        let lts = &items[3];
        assert_eq!(lts.title, "Unity 6000.3.27f1 (LTS)");
        assert_eq!(lts.summary.as_deref(), Some("Unity LTS release"));
        assert_eq!(lts.prerelease, Some(false));
    }

    #[test]
    fn version_letter_marks_prerelease_without_stream() {
        let item = to_item(Release {
            version: "6000.3.0b4".into(),
            release_date: None,
            stream: None,
        });
        assert_eq!(item.prerelease, Some(true));
        assert_eq!(item.title, "Unity 6000.3.0b4 (Beta)");
    }

    #[test]
    fn rejects_bad_json() {
        assert!(matches!(parse(b"{}"), Err(FetchError::Parse(_))));
    }
}
