//! Fetching sources. `fetch_source` dispatches on the source kind; each kind lives in its
//! own module. All HTTP goes through `Http`, which handles the User-Agent, timeouts and
//! conditional requests (ETag / Last-Modified).

pub mod feed;
pub mod github;
pub mod hn;
pub mod html;
pub mod unity;

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap, HeaderValue};

use crate::config::{ResolvedSource, SourceSpec};
use crate::model::{RawItem, SourceState};

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {status} from {url}")]
    Status { status: StatusCode, url: String },
    #[error("rate limited by {url}")]
    RateLimited {
        url: String,
        retry_after: Option<Duration>,
    },
    #[error("parse error: {0}")]
    Parse(String),
    #[error("{0} sources are not implemented yet")]
    Unimplemented(&'static str),
}

/// Result of polling a source.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Server answered 304; nothing changed since the stored validators.
    NotModified,
    Fetched {
        items: Vec<RawItem>,
        /// New validators to store, if the response had any.
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// Fetches one source. `state` supplies validators for conditional requests.
pub async fn fetch_source(
    http: &Http,
    source: &ResolvedSource,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    match &source.spec {
        SourceSpec::Feed { url, cache_bust } => feed::fetch(http, url, *cache_bust, state).await,
        SourceSpec::Github {
            repo,
            mode,
            tag_prefix,
        } => github::fetch(http, repo, *mode, tag_prefix.as_deref(), state).await,
        SourceSpec::Unity { limit } => unity::fetch(http, *limit, state).await,
        SourceSpec::Hn { query } => hn::fetch(http, query, state).await,
        SourceSpec::Html {
            url,
            item,
            title,
            link,
            date,
        } => {
            let selectors = html::Selectors {
                item,
                title,
                link,
                date: date.as_deref(),
            };
            html::fetch(http, url, &selectors, state).await
        }
    }
}

/// Shared HTTP client.
#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
    pub github_token: Option<String>,
}

/// A response body, or a 304.
pub enum Conditional {
    NotModified,
    Body(Response),
}

pub struct Response {
    pub body: bytes::Bytes,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Http {
    pub fn new(user_agent: &str, github_token: Option<String>) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Http {
            client,
            github_token,
        })
    }

    /// GET with `If-None-Match` / `If-Modified-Since` from `state`.
    /// Non-success statuses other than 304 become errors (429 -> `RateLimited`).
    pub async fn get_conditional(
        &self,
        url: &str,
        state: &SourceState,
        extra_headers: HeaderMap,
    ) -> Result<Conditional, FetchError> {
        let mut req = self.client.get(url).headers(extra_headers);
        if let Some(etag) = state
            .etag
            .as_deref()
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = state
            .last_modified
            .as_deref()
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            req = req.header(header::IF_MODIFIED_SINCE, lm);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(Conditional::NotModified);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(FetchError::RateLimited {
                url: url.to_string(),
                retry_after,
            });
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                status,
                url: url.to_string(),
            });
        }
        let header_str = |name| {
            resp.headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header_str(header::CONTENT_TYPE);
        let etag = header_str(header::ETAG);
        let last_modified = header_str(header::LAST_MODIFIED);
        let body = resp.bytes().await?;
        Ok(Conditional::Body(Response {
            body,
            content_type,
            etag,
            last_modified,
        }))
    }
}

/// Converts an HTML fragment to collapsed plain text.
pub fn html_to_text(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    let text: String = fragment.root_element().text().collect::<Vec<_>>().join(" ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates to at most `max` chars on a char boundary, appending "…" when cut.
pub fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", s[..idx].trim_end()),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        assert_eq!(
            html_to_text("<p>Hello <b>world</b></p>\n<p>again</p>"),
            "Hello world again"
        );
        assert_eq!(html_to_text("&lt;p&gt;escaped"), "<p>escaped");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo world", 5), "héllo…");
        assert_eq!(truncate("short", 10), "short");
    }
}
