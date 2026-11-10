//! GitHub releases, tags or commits for a repository.
//!
//! Releases use the REST API when a token is configured (it carries the prerelease flag
//! and hides drafts), otherwise the public `releases.atom` feed. Tags and commits always
//! use the Atom feeds. Conditional requests (ETag / 304) go through `Http::get_conditional`;
//! GitHub does not count 304s against the API rate limit.
//!
//! Titles that are just a version (e.g. Godot's `4.7.2-stable`, S&Box's `26.10.13`) are
//! prefixed with the repository name so they make sense out of context
//! (`godot 4.7.2-stable`); descriptive names such as `Stride 4.4.1` are kept as is.

use chrono::{DateTime, Utc};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::Deserialize;

use super::{Conditional, FetchError, FetchOutcome, Http, feed, text_to_lines, truncate};
use crate::config::{GithubMode, ResolvedSource, SourceSpec};
use crate::model::{RawItem, SourceState};

/// Max chars kept from a release body.
const SUMMARY_MAX: usize = 500;

/// Polls a repository's releases, tags or commits.
pub async fn fetch(
    http: &Http,
    repo: &str,
    mode: GithubMode,
    tag_prefix: Option<&str>,
    state: &SourceState,
) -> Result<FetchOutcome, FetchError> {
    let (url, headers) = match (mode, http.github_token.as_deref()) {
        (GithubMode::Releases, Some(token)) => (
            format!("https://api.github.com/repos/{repo}/releases?per_page=30"),
            api_headers(token)?,
        ),
        (GithubMode::Releases, None) => (
            format!("https://github.com/{repo}/releases.atom"),
            HeaderMap::new(),
        ),
        (GithubMode::Tags, _) => (
            format!("https://github.com/{repo}/tags.atom"),
            HeaderMap::new(),
        ),
        (GithubMode::Commits, _) => (
            format!("https://github.com/{repo}/commits.atom"),
            HeaderMap::new(),
        ),
    };
    let resp = match http.get_conditional(&url, state, headers).await? {
        Conditional::NotModified => return Ok(FetchOutcome::NotModified),
        Conditional::Body(resp) => resp,
    };
    let items = match (mode, http.github_token.is_some()) {
        (GithubMode::Releases, true) => parse_releases_json(&resp.body, repo, tag_prefix)?,
        (GithubMode::Releases | GithubMode::Tags, _) => {
            parse_tag_atom(&resp.body, repo, mode, tag_prefix)?
        }
        (GithubMode::Commits, _) => parse_commits_atom(&resp.body)?,
    };
    Ok(FetchOutcome::Fetched {
        items,
        etag: resp.etag,
        last_modified: resp.last_modified,
    })
}

/// A warning for when GitHub releases sources are polled without a token, or `None`.
/// Without one, releases come from the public `releases.atom` feed, which has no
/// prerelease flag (only the title can mark a prerelease) and is polled anonymously; a
/// token changes nothing for tags and commits. `default_interval_minutes` applies to sources
/// without their own interval.
pub fn token_warning(
    sources: &[ResolvedSource],
    default_interval_minutes: u64,
    has_token: bool,
) -> Option<String> {
    if has_token {
        return None;
    }
    let mut releases = 0;
    let mut per_hour = 0.0;
    for s in sources {
        let SourceSpec::Github { mode, .. } = &s.spec else {
            continue;
        };
        if *mode == GithubMode::Releases {
            releases += 1;
        }
        per_hour += 60.0
            / s.interval_minutes
                .unwrap_or(default_interval_minutes)
                .max(1) as f64;
    }
    if releases == 0 {
        return None;
    }
    Some(format!(
        "{releases} GitHub releases {} polled without a GitHub token: they use the public \
         releases.atom feed, which has no prerelease flag, and all GitHub sources \
         together make about {} requests/hour to github.com anonymously. Set the GITHUB_TOKEN \
         env var or [polling] github_token (a fine-grained token with no permissions is \
         enough) to use the GitHub API instead.",
        if releases == 1 {
            "source is"
        } else {
            "sources are"
        },
        per_hour.round() as u64
    ))
}

/// Headers for an authenticated REST API call.
fn api_headers(token: &str) -> Result<HeaderMap, FetchError> {
    let mut headers = HeaderMap::new();
    let auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| FetchError::Parse("github token is not a valid header value".into()))?;
    headers.insert(header::AUTHORIZATION, auth);
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "x-github-api-version",
        HeaderValue::from_static("2022-11-28"),
    );
    Ok(headers)
}

/// One release from `GET /repos/{repo}/releases`.
#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    name: Option<String>,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
}

/// Parses the REST releases list, skipping drafts and tags outside `tag_prefix`.
fn parse_releases_json(
    body: &[u8],
    repo: &str,
    tag_prefix: Option<&str>,
) -> Result<Vec<RawItem>, FetchError> {
    let releases: Vec<ApiRelease> =
        serde_json::from_slice(body).map_err(|e| FetchError::Parse(e.to_string()))?;
    Ok(releases
        .into_iter()
        .filter(|r| !r.draft && has_prefix(&r.tag_name, tag_prefix))
        .map(|r| {
            let name = r.name.as_deref().map(str::trim).unwrap_or_default();
            let name = if name.is_empty() { &r.tag_name } else { name };
            RawItem {
                title: release_title(repo, name),
                url: Some(r.html_url),
                summary: r
                    .body
                    .map(|b| truncate(&text_to_lines(&b), SUMMARY_MAX))
                    .filter(|s| !s.is_empty()),
                published: r.published_at,
                prerelease: Some(r.prerelease),
                image_url: None,
                guid: r.tag_name,
            }
        })
        .collect())
}

/// Parses `releases.atom` or `tags.atom`: guid = tag from the entry link.
fn parse_tag_atom(
    body: &[u8],
    repo: &str,
    mode: GithubMode,
    tag_prefix: Option<&str>,
) -> Result<Vec<RawItem>, FetchError> {
    Ok(feed::parse(body)?
        .into_iter()
        .filter_map(|mut item| {
            let tag = item.url.as_deref().and_then(tag_from_url)?;
            if !has_prefix(&tag, tag_prefix) {
                return None;
            }
            let name = if mode == GithubMode::Tags || item.title.is_empty() {
                &tag
            } else {
                &item.title
            };
            item.title = release_title(repo, name);
            item.guid = tag;
            Some(item)
        })
        .collect())
}

/// Parses `commits.atom`: title = first line of the commit message.
fn parse_commits_atom(body: &[u8]) -> Result<Vec<RawItem>, FetchError> {
    Ok(feed::parse(body)?
        .into_iter()
        .map(|mut item| {
            if let Some(line) = item.title.lines().map(str::trim).find(|l| !l.is_empty()) {
                item.title = line.to_string();
            }
            item
        })
        .collect())
}

/// Extracts the URL-decoded tag from `.../releases/tag/<tag>`.
fn tag_from_url(url: &str) -> Option<String> {
    let (_, encoded) = url.split_once("/releases/tag/")?;
    let encoded = encoded.split(['?', '#']).next()?;
    let tag = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?
        .into_owned();
    (!tag.is_empty()).then_some(tag)
}

fn has_prefix(tag: &str, prefix: Option<&str>) -> bool {
    prefix.is_none_or(|p| tag.starts_with(p))
}

/// Prefixes bare version names (`4.7.2-stable`, `v1.2`) with the repository name.
fn release_title(repo: &str, name: &str) -> String {
    let mut chars = name.chars();
    let bare_version = match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('v' | 'V') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    };
    if bare_version {
        let short = repo.rsplit('/').next().unwrap_or(repo);
        format!("{short} {name}")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Owner;

    fn source(repo: &str, mode: GithubMode, interval: Option<u64>) -> ResolvedSource {
        ResolvedSource {
            id: format!("e:github:{repo}"),
            owner: Owner::Engine("e".into()),
            owner_name: "E".into(),
            enabled: true,
            category: None,
            interval_minutes: interval,
            keywords: Vec::new(),
            require_engine_match: false,
            spec: SourceSpec::Github {
                repo: repo.into(),
                mode,
                tag_prefix: None,
            },
        }
    }

    /// Without a token, releases come from the Atom feed (no prerelease flag), so
    /// `check-config` and `serve` say so, with the github.com polling load.
    #[test]
    fn warns_about_releases_without_a_token() {
        let hn = ResolvedSource {
            spec: SourceSpec::Hn { query: "x".into() },
            ..source("a/hn", GithubMode::Releases, None)
        };
        let sources = vec![
            source("a/one", GithubMode::Releases, None),
            source("a/two", GithubMode::Releases, Some(30)),
            source("a/three", GithubMode::Tags, Some(15)),
            hn,
        ];
        // 60-minute default: 1 + 2 + 4 requests an hour.
        let warning = token_warning(&sources, 60, false).expect("warns");
        assert!(warning.contains("2 GitHub releases sources"), "{warning}");
        assert!(warning.contains("about 7 requests/hour"), "{warning}");
        assert!(warning.contains("GITHUB_TOKEN"), "{warning}");

        assert_eq!(token_warning(&sources, 60, true), None, "token set");
        let tags_only = vec![source("a/three", GithubMode::Tags, None)];
        assert_eq!(
            token_warning(&tags_only, 60, false),
            None,
            "a token doesn't change tags or commits"
        );
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/github/",
                $name
            ))
        };
    }

    #[test]
    fn decodes_tag_from_link() {
        assert_eq!(
            tag_from_url("https://github.com/stride3d/stride/releases/tag/releases%2F4.4.1")
                .as_deref(),
            Some("releases/4.4.1")
        );
        assert_eq!(tag_from_url("https://github.com/a/b/commit/abc"), None);
    }

    #[test]
    fn titles_prefix_bare_versions_only() {
        assert_eq!(
            release_title("godotengine/godot", "4.7.2-stable"),
            "godot 4.7.2-stable"
        );
        assert_eq!(release_title("x/flax", "v1.13"), "flax v1.13");
        assert_eq!(
            release_title("stride3d/stride", "Stride 4.4.1"),
            "Stride 4.4.1"
        );
    }

    #[test]
    fn stride_releases_atom_with_prefix() {
        let body = fixture!("stride_releases.atom");
        let all = parse_tag_atom(body, "stride3d/stride", GithubMode::Releases, None).unwrap();
        assert_eq!(all.len(), 10);
        let items = parse_tag_atom(
            body,
            "stride3d/stride",
            GithubMode::Releases,
            Some("releases/"),
        )
        .unwrap();
        let guids: Vec<_> = items.iter().map(|i| i.guid.as_str()).collect();
        assert_eq!(guids, ["releases/4.4.1", "releases/4.4.0"]);
        assert_eq!(items[0].title, "Stride 4.4.1");
        assert!(items[0].published.is_some());
        assert!(items[0].prerelease.is_none());
        assert!(!items[0].summary.as_deref().unwrap_or("").contains('<'));
    }

    #[test]
    fn godot_releases_json() {
        let items = parse_releases_json(fixture!("godot_releases.json"), "godotengine/godot", None)
            .unwrap();
        assert_eq!(items.len(), 5);
        let first = &items[0];
        assert_eq!(first.guid, "3.6.3-stable");
        assert_eq!(first.title, "godot 3.6.3-stable");
        assert_eq!(first.prerelease, Some(false));
        assert!(first.published.is_some());
        assert!(
            first
                .url
                .as_deref()
                .unwrap()
                .ends_with("/releases/tag/3.6.3-stable")
        );
        let summary = first.summary.as_deref().unwrap();
        assert!(summary.chars().count() <= SUMMARY_MAX + 1);
        assert!(!summary.contains("\n\n") && !summary.contains('\r'));
    }

    #[test]
    fn releases_json_skips_drafts_and_keeps_prerelease_flag() {
        let body = br#"[
            {"tag_name": "4.8-beta1", "name": "", "html_url": "https://h/4.8-beta1",
             "body": "Beta\n\n* fixes", "draft": false, "prerelease": true,
             "published_at": "2026-10-15T12:00:00Z"},
            {"tag_name": "4.8-draft", "name": "Draft", "html_url": "https://h/d",
             "body": null, "draft": true, "prerelease": false, "published_at": null},
            {"tag_name": "launcher/1.0", "name": "Launcher 1.0", "html_url": "https://h/l",
             "body": "", "draft": false, "prerelease": false, "published_at": null}
        ]"#;
        let items = parse_releases_json(body, "godotengine/godot", None).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].guid, "4.8-beta1");
        assert_eq!(items[0].title, "godot 4.8-beta1");
        assert_eq!(items[0].prerelease, Some(true));
        assert_eq!(items[0].summary.as_deref(), Some("Beta\n• fixes"));
        assert_eq!(items[1].summary, None);

        let filtered = parse_releases_json(body, "o/r", Some("launcher/")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].guid, "launcher/1.0");
    }

    #[test]
    fn sbox_tags_atom() {
        let items = parse_tag_atom(
            fixture!("sbox_tags.atom"),
            "Facepunch/sbox-public",
            GithubMode::Tags,
            None,
        )
        .unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0].guid, "26.10.13");
        assert_eq!(items[0].title, "sbox-public 26.10.13");
        assert!(items[0].published.is_some());
    }

    #[test]
    fn godot_commits_atom() {
        let items = parse_commits_atom(fixture!("godot_commits.atom")).unwrap();
        assert!(!items.is_empty());
        assert_eq!(
            items[0].title,
            "Merge pull request #124750 from clayjohn/GLES3-tex-slot"
        );
        assert!(items[0].url.as_deref().unwrap().contains("/commit/"));
        assert!(items[0].guid.contains("5d1cd20f"));
    }
}
