//! Human-readable time since a date, e.g. "3 hours ago" or "3h".

use chrono::{DateTime, Utc};

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
}
