//! Fetching sources. `fetch_source` dispatches on the source kind; each kind lives in its
//! own module. All HTTP goes through `Http`, which handles the User-Agent, timeouts and
//! conditional requests (ETag / Last-Modified).

pub mod feed;
pub mod github;
pub mod hn;
pub mod html;
pub mod og;
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
    /// Non-success statuses other than 304 become errors (429, or GitHub's rate-limit 403,
    /// -> `RateLimited`).
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
        let header_u64 = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };
        // GitHub signals an exhausted rate limit with 403 + `x-ratelimit-remaining: 0`.
        let github_limited =
            status == StatusCode::FORBIDDEN && header_u64("x-ratelimit-remaining") == Some(0);
        if status == StatusCode::TOO_MANY_REQUESTS || github_limited {
            let retry_after = header_u64(header::RETRY_AFTER.as_str())
                .or_else(|| {
                    let reset = header_u64("x-ratelimit-reset")?;
                    let now = chrono::Utc::now().timestamp().max(0) as u64;
                    Some(reset.saturating_sub(now))
                })
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
        let etag = header_str(header::ETAG);
        let last_modified = header_str(header::LAST_MODIFIED);
        let body = read_limited(resp, url).await?;
        Ok(Conditional::Body(Response {
            body,
            etag,
            last_modified,
        }))
    }
}

/// Largest response body accepted from a source.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Reads the body, failing once it exceeds `MAX_BODY_BYTES`.
async fn read_limited(mut resp: reqwest::Response, url: &str) -> Result<bytes::Bytes, FetchError> {
    let too_large = || {
        FetchError::Parse(format!(
            "response from {url} exceeds {MAX_BODY_BYTES} bytes"
        ))
    };
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// Converts an HTML fragment to collapsed plain text.
pub fn html_to_text(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    let text: String = fragment.root_element().text().collect::<Vec<_>>().join(" ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Plain text from HTML that keeps its structure: paragraphs, headings and line breaks
/// start a new line and list items become "• " lines. Other whitespace collapses.
pub fn html_to_lines(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    let mut lines = Lines::default();
    for child in fragment.root_element().children() {
        walk_html(child, &mut lines);
    }
    lines.finish()
}

fn walk_html(node: ego_tree::NodeRef<'_, scraper::Node>, lines: &mut Lines) {
    match node.value() {
        scraper::Node::Text(text) => lines.text(text),
        scraper::Node::Element(element) => {
            let name = element.name();
            if matches!(name, "script" | "style" | "template") {
                return;
            }
            let block = matches!(
                name,
                "p" | "div"
                    | "br"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "ul"
                    | "ol"
                    | "li"
                    | "blockquote"
                    | "pre"
                    | "tr"
                    | "table"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "hr"
                    | "figure"
                    | "dl"
                    | "dt"
                    | "dd"
            );
            if name == "li" {
                lines.bullet();
            } else if block {
                lines.break_line();
            }
            for child in node.children() {
                walk_html(child, lines);
            }
            if block {
                lines.break_line();
            }
        }
        _ => {
            for child in node.children() {
                walk_html(child, lines);
            }
        }
    }
}

/// Plain text (or Markdown, as in GitHub release notes) tidied the same way: one line per
/// paragraph or list item, list markers as "• ", headings and emphasis markers dropped,
/// `[text](url)` links reduced to their text.
pub fn text_to_lines(text: &str) -> String {
    let link = regex::Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").expect("valid regex");
    let mut lines = Lines::default();
    for raw in text.lines() {
        let line = link.replace_all(raw.trim(), "$1");
        let line = line.trim_start_matches('#').trim();
        let line = line.replace("**", "").replace("__", "");
        if line.is_empty() {
            lines.break_line();
        } else if let Some(item) = ["- ", "* ", "+ ", "• "]
            .iter()
            .find_map(|m| line.strip_prefix(m))
        {
            lines.bullet();
            lines.text(item);
        } else {
            lines.break_line();
            lines.text(&line);
        }
    }
    lines.finish()
}

/// Builds text line by line, collapsing whitespace within a line.
#[derive(Default)]
struct Lines {
    done: Vec<String>,
    current: String,
    /// Whitespace came before the next word (possibly in an earlier text node).
    space: bool,
}

impl Lines {
    fn text(&mut self, text: &str) {
        self.space |= text.starts_with(char::is_whitespace);
        for (i, word) in text.split_whitespace().enumerate() {
            if (i > 0 || self.space) && !self.current.is_empty() && !self.current.ends_with(' ') {
                self.current.push(' ');
            }
            self.current.push_str(word);
            self.space = false;
        }
        self.space |= text.ends_with(char::is_whitespace);
    }

    fn break_line(&mut self) {
        self.space = false;
        let line = std::mem::take(&mut self.current);
        let line = line.trim();
        if !line.is_empty() && line != "•" {
            self.done.push(line.to_string());
        }
    }

    fn bullet(&mut self) {
        self.break_line();
        self.current.push_str("• ");
    }

    fn finish(mut self) -> String {
        self.break_line();
        self.done.join("\n")
    }
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
    fn keeps_lines_and_bullets() {
        assert_eq!(
            html_to_lines(
                "<p>Godot 4.8 is <b>out</b>.</p>\n<h2>Highlights</h2><ul><li>Jolt physics</li>\
                 <li>.NET 9</li></ul>Thanks!<br>The team<script>x()</script>"
            ),
            "Godot 4.8 is out.\nHighlights\n• Jolt physics\n• .NET 9\nThanks!\nThe team"
        );
        assert_eq!(html_to_lines("a<b>b</b> c"), "ab c");
        assert_eq!(html_to_lines("<p>  </p><li></li>"), "");
        assert_eq!(
            text_to_lines(
                "## What's new\r\n\r\n* **C#** hot   reload\n- [Docs](https://x.org) updated\n\nFull   changelog"
            ),
            "What's new\n• C# hot reload\n• Docs updated\nFull changelog"
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo world", 5), "héllo…");
        assert_eq!(truncate("short", 10), "short");
    }
}
