//! Template-facing view models and small formatting helpers.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::model::Item;
pub use crate::registry::Lookup;

/// Maximum length of a summary snippet on list pages, in characters.
const SNIPPET_CHARS: usize = 280;

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
    /// The day it belongs to in the timeline, e.g. "Today" or "Wednesday".
    pub day: String,
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
            // Already filtered at ingestion; checked again in case of older rows.
            url: item.url.clone().filter(|u| crate::classify::is_http_url(u)),
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
            day: day_label(item.date(), now),
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
    /// Relative to now, compact, e.g. "3h".
    pub short: String,
}

impl DateView {
    pub fn new(dt: DateTime<Utc>, now: DateTime<Utc>) -> Self {
        DateView {
            iso: dt.to_rfc3339(),
            abs: dt.format("%Y-%m-%d %H:%M UTC").to_string(),
            rel: relative_time(dt, now),
            short: short_relative_time(dt, now),
        }
    }
}

/// Human relative time such as "just now", "5 minutes ago" or "2 years ago".
pub fn relative_time(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match elapsed(dt, now) {
        None => "just now".to_string(),
        Some((n, unit, _)) => {
            let plural = if n == 1 { "" } else { "s" };
            format!("{n} {unit}{plural} ago")
        }
    }
}

/// Compact relative time such as "now", "5m", "3h", "2d", "4mo" or "1y".
pub fn short_relative_time(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match elapsed(dt, now) {
        None => "now".to_string(),
        Some((n, _, short)) => format!("{n}{short}"),
    }
}

/// Time since `dt` in its largest whole unit (long and short name), or `None` under a minute.
fn elapsed(dt: DateTime<Utc>, now: DateTime<Utc>) -> Option<(i64, &'static str, &'static str)> {
    Some(match (now - dt).num_seconds() {
        s if s < 60 => return None,
        s if s < 3_600 => (s / 60, "minute", "m"),
        s if s < 86_400 => (s / 3_600, "hour", "h"),
        s if s < 30 * 86_400 => (s / 86_400, "day", "d"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month", "mo"),
        s => (s / (365 * 86_400), "year", "y"),
    })
}

/// The timeline day of `dt`: "Today", "Yesterday", a weekday within the last week, then a
/// date such as "15 October 2026". Days are UTC, like the stored dates.
pub fn day_label(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (day, today) = (dt.date_naive(), now.date_naive());
    match (today - day).num_days() {
        ..=0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => dt.format("%A").to_string(),
        _ => dt.format("%-d %B %Y").to_string(),
    }
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

pub use crate::classify::mentions_csharp;

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
        assert_eq!(short_relative_time(now, now), "now");
        assert_eq!(short_relative_time(now - Duration::hours(3), now), "3h");
        assert_eq!(short_relative_time(now - Duration::days(45), now), "1mo");
    }

    #[test]
    fn labels_timeline_days() {
        let now = DateTime::parse_from_rfc3339("2026-10-23T10:00:00Z")
            .unwrap()
            .to_utc();
        let ago = |h| day_label(now - Duration::hours(h), now);
        assert_eq!(ago(9), "Today");
        assert_eq!(ago(11), "Yesterday");
        assert_eq!(ago(2 * 24), "Wednesday");
        assert_eq!(ago(8 * 24), "15 October 2026");
        // A date slightly in the future still counts as today.
        assert_eq!(day_label(now + Duration::hours(1), now), "Today");
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
