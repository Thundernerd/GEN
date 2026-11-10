//! Source health: whether a source is fetching fine, failing, or silent.

use chrono::{DateTime, Duration, Utc};

use crate::model::SourceState;
use crate::web::view::relative_time;

/// A source's health, as shown on the Sources page and by `gen check-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Switched off.
    Disabled,
    /// Never fetched yet.
    Pending,
    /// Fetching fine, with recent items.
    Ok,
    /// The last fetch failed, but it worked recently; it is retried on the next poll.
    Retrying,
    /// Fetching fine, but it has had no new items for a long time.
    Stale,
    /// Fails repeatedly, or hasn't fetched successfully for days.
    Failing,
}

/// Consecutive failed fetches after which a source counts as failing.
pub const FAILING_AFTER: u32 = 3;
/// A source that fails and hasn't fetched successfully for this long counts as failing.
pub const FAILING_WITHOUT_OK_DAYS: i64 = 3;
/// A working source without new items for this long (about six months) counts as stale.
pub const STALE_AFTER_DAYS: i64 = 182;

/// The health of a source from its fetch state and the date of its newest item.
pub fn classify(
    enabled: bool,
    state: &SourceState,
    latest_item: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Health {
    let older_than =
        |dt: Option<DateTime<Utc>>, days: i64| dt.is_none_or(|dt| now - dt > Duration::days(days));
    if !enabled {
        Health::Disabled
    } else if state.consecutive_failures > 0 {
        if state.consecutive_failures >= FAILING_AFTER
            || older_than(state.last_ok, FAILING_WITHOUT_OK_DAYS)
        {
            Health::Failing
        } else {
            Health::Retrying
        }
    } else if state.last_ok.is_none() {
        Health::Pending
    } else if older_than(latest_item, STALE_AFTER_DAYS) {
        Health::Stale
    } else {
        Health::Ok
    }
}

impl Health {
    /// Whether the source needs a look: it fails, or it has gone silent.
    pub fn is_unhealthy(self) -> bool {
        matches!(self, Health::Failing | Health::Stale)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Health::Disabled => "disabled",
            Health::Pending => "pending",
            Health::Ok => "ok",
            Health::Retrying => "retrying",
            Health::Stale => "stale",
            Health::Failing => "failing",
        }
    }
}

/// Why an unhealthy source is unhealthy, in a few words, e.g. for `gen check-config`.
pub fn reason(
    health: Health,
    state: &SourceState,
    latest_item: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> String {
    let ago = |dt| relative_time(dt, now);
    match health {
        Health::Failing | Health::Retrying => {
            let n = state.consecutive_failures;
            let last_ok = state.last_ok.map_or_else(
                || "never OK".to_string(),
                |dt| format!("last OK {}", ago(dt)),
            );
            let error = state.last_error.as_deref().unwrap_or("unknown error");
            let plural = if n == 1 { "" } else { "s" };
            format!("{n} failure{plural} in a row, {last_ok}: {error}")
        }
        Health::Stale => latest_item.map_or_else(
            || "no items yet".to_string(),
            |dt| format!("newest item {}", ago(dt)),
        ),
        Health::Disabled | Health::Pending | Health::Ok => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn state(last_ok_days_ago: Option<i64>, failures: u32) -> SourceState {
        SourceState {
            last_ok: last_ok_days_ago.map(|d| now() - Duration::days(d)),
            last_error: (failures > 0).then(|| "HTTP 404".into()),
            consecutive_failures: failures,
            ..Default::default()
        }
    }

    fn days_ago(d: i64) -> Option<DateTime<Utc>> {
        Some(now() - Duration::days(d))
    }

    #[test]
    fn disabled_sources_are_disabled_whatever_their_state() {
        assert_eq!(
            classify(false, &state(None, 9), None, now()),
            Health::Disabled
        );
    }

    #[test]
    fn never_fetched_sources_are_pending() {
        assert_eq!(
            classify(true, &state(None, 0), None, now()),
            Health::Pending
        );
    }

    #[test]
    fn working_sources_with_recent_items_are_ok() {
        assert_eq!(
            classify(true, &state(Some(0), 0), days_ago(30), now()),
            Health::Ok
        );
    }

    #[test]
    fn a_single_failure_after_a_recent_success_is_retrying() {
        assert_eq!(
            classify(true, &state(Some(1), 1), days_ago(2), now()),
            Health::Retrying
        );
        assert_eq!(
            classify(true, &state(Some(1), 2), days_ago(2), now()),
            Health::Retrying
        );
    }

    #[test]
    fn three_failures_in_a_row_are_failing() {
        assert_eq!(
            classify(true, &state(Some(0), 3), days_ago(2), now()),
            Health::Failing
        );
    }

    #[test]
    fn failing_without_success_for_over_three_days_is_failing() {
        assert_eq!(
            classify(true, &state(Some(4), 1), days_ago(4), now()),
            Health::Failing
        );
        // Never fetched successfully, e.g. a URL that was 404 from the start.
        assert_eq!(
            classify(true, &state(None, 1), None, now()),
            Health::Failing
        );
    }

    #[test]
    fn a_slow_polling_interval_alone_is_not_failing() {
        // Polled weekly: last OK a week ago, no failures since.
        assert_eq!(
            classify(true, &state(Some(7), 0), days_ago(7), now()),
            Health::Ok
        );
    }

    #[test]
    fn working_sources_without_items_for_six_months_are_stale() {
        assert_eq!(
            classify(true, &state(Some(0), 0), days_ago(200), now()),
            Health::Stale
        );
        assert_eq!(
            classify(true, &state(Some(0), 0), days_ago(170), now()),
            Health::Ok
        );
        // Fetches fine but has never produced an item, e.g. a broken scraper selector.
        assert_eq!(
            classify(true, &state(Some(0), 0), None, now()),
            Health::Stale
        );
    }

    #[test]
    fn only_failing_and_stale_are_unhealthy() {
        let unhealthy: Vec<_> = [
            Health::Disabled,
            Health::Pending,
            Health::Ok,
            Health::Retrying,
            Health::Stale,
            Health::Failing,
        ]
        .into_iter()
        .filter(|h| h.is_unhealthy())
        .collect();
        assert_eq!(unhealthy, [Health::Stale, Health::Failing]);
    }

    #[test]
    fn explains_why_a_source_is_unhealthy() {
        assert_eq!(
            reason(Health::Failing, &state(Some(5), 4), days_ago(5), now()),
            "4 failures in a row, last OK 5 days ago: HTTP 404"
        );
        assert_eq!(
            reason(Health::Failing, &state(None, 1), None, now()),
            "1 failure in a row, never OK: HTTP 404"
        );
        assert_eq!(
            reason(Health::Stale, &state(Some(0), 0), days_ago(200), now()),
            "newest item 6 months ago"
        );
        assert_eq!(
            reason(Health::Stale, &state(Some(0), 0), None, now()),
            "no items yet"
        );
    }
}
