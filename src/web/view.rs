//! Template-facing view models and small formatting helpers.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::{Owner, SourcesConfig};
use crate::model::Item;

/// Maximum length of a summary snippet on list pages, in characters.
const SNIPPET_CHARS: usize = 280;

/// Name lookups derived once from `sources.toml`.
pub struct Lookup {
    /// Source id -> owner display name, for every source (including disabled ones).
    pub source_names: HashMap<String, String>,
    /// Ids of sources that belong to general outlets rather than an engine.
    pub outlet_sources: HashSet<String>,
    /// Engine id -> display name.
    pub engine_names: HashMap<String, String>,
}

impl Lookup {
    pub fn new(sources: &SourcesConfig) -> Self {
        let all = sources.all_sources();
        Lookup {
            source_names: all
                .iter()
                .map(|s| (s.id.clone(), s.owner_name.clone()))
                .collect(),
            outlet_sources: all
                .iter()
                .filter(|s| matches!(s.owner, Owner::Outlet(_)))
                .map(|s| s.id.clone())
                .collect(),
            engine_names: sources
                .engines
                .iter()
                .map(|e| (e.id.clone(), e.name.clone()))
                .collect(),
        }
    }

    /// Display name of a source's owner, falling back to the raw id for unknown sources.
    pub fn source_name(&self, source_id: &str) -> String {
        self.source_names
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| source_id.to_string())
    }

    pub fn engine_name(&self, engine_id: &str) -> String {
        self.engine_names
            .get(engine_id)
            .cloned()
            .unwrap_or_else(|| engine_id.to_string())
    }

    /// Distinct owner names of the given duplicate sources, minus the item's own owner.
    pub fn also_on(&self, item: &Item, dup_sources: Option<&Vec<String>>) -> Vec<String> {
        let own = self.source_name(&item.source_id);
        let mut names: Vec<String> = Vec::new();
        for id in dup_sources.into_iter().flatten() {
            let name = self.source_name(id);
            if name != own && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

#[derive(Debug, Serialize)]
pub struct EngineChip {
    pub id: String,
    pub name: String,
}

/// An item as rendered in a list.
#[derive(Debug, Serialize)]
pub struct ItemView {
    pub id: i64,
    pub kind: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub source: String,
    pub engines: Vec<EngineChip>,
    pub date: DateView,
    pub summary: Option<String>,
    pub also_on: Vec<String>,
}

impl ItemView {
    pub fn new(
        item: &Item,
        lookup: &Lookup,
        dups: &HashMap<i64, Vec<String>>,
        now: DateTime<Utc>,
    ) -> Self {
        ItemView {
            id: item.id,
            kind: item.kind.as_str(),
            title: item.title.clone(),
            url: item.url.clone(),
            source: lookup.source_name(&item.source_id),
            engines: item
                .engines
                .iter()
                .map(|id| EngineChip {
                    id: id.clone(),
                    name: lookup.engine_name(id),
                })
                .collect(),
            date: DateView::new(item.date(), now),
            summary: item
                .summary
                .as_deref()
                .map(|s| snippet(s, SNIPPET_CHARS))
                .filter(|s| !s.is_empty()),
            also_on: lookup.also_on(item, dups.get(&item.id)),
        }
    }
}

/// A timestamp in the three forms templates need.
#[derive(Debug, Serialize)]
pub struct DateView {
    /// RFC 3339, for `<time datetime>`.
    pub iso: String,
    /// Absolute, for the `title` tooltip.
    pub abs: String,
    /// Relative to now, e.g. "3 hours ago".
    pub rel: String,
}

impl DateView {
    pub fn new(dt: DateTime<Utc>, now: DateTime<Utc>) -> Self {
        DateView {
            iso: dt.to_rfc3339(),
            abs: dt.format("%Y-%m-%d %H:%M UTC").to_string(),
            rel: relative_time(dt, now),
        }
    }
}

/// Human relative time such as "just now", "5 minutes ago" or "2 years ago".
pub fn relative_time(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - dt).num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }
    let (n, unit) = match secs {
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s if s < 30 * 86_400 => (s / 86_400, "day"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
        s => (s / (365 * 86_400), "year"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// Collapses whitespace and truncates to at most `max` characters at a word boundary.
pub fn snippet(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(max).collect();
    let trimmed = match cut.rfind(' ') {
        Some(i) if i > max / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!(
        "{}…",
        trimmed.trim_end_matches(|c: char| c.is_ascii_punctuation())
    )
}

/// Whether text mentions C# or .NET.
// TODO: replace with classify::mentions_csharp
pub fn mentions_csharp(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["c#", ".net", "dotnet", "nuget"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Builds a `?a=b&c=d` query string from the pairs that have a value ("" when none do).
pub fn query_string(pairs: &[(&str, Option<String>)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        if let Some(v) = value {
            ser.append_pair(key, v);
        }
    }
    let qs = ser.finish();
    if qs.is_empty() { qs } else { format!("?{qs}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn formats_relative_times() {
        let now = Utc::now();
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(
            relative_time(now - Duration::minutes(1), now),
            "1 minute ago"
        );
        assert_eq!(relative_time(now - Duration::hours(3), now), "3 hours ago");
        assert_eq!(relative_time(now - Duration::days(400), now), "1 year ago");
    }

    #[test]
    fn truncates_snippets_at_words() {
        assert_eq!(snippet("  a   b\n c ", 10), "a b c");
        assert_eq!(snippet("hello wonderful world", 18), "hello wonderful…");
    }

    #[test]
    fn detects_csharp_mentions() {
        assert!(mentions_csharp("Now with C# scripting"));
        assert!(mentions_csharp("Ships on NuGet"));
        assert!(!mentions_csharp("Rust and C++ only"));
    }

    #[test]
    fn builds_query_strings() {
        assert_eq!(query_string(&[("a", None)]), "");
        assert_eq!(
            query_string(&[("engine", Some("godot".into())), ("kind", None)]),
            "?engine=godot"
        );
    }
}
