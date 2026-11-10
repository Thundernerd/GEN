//! Source health: whether a source is fetching fine, failing, or silent.

use chrono::{DateTime, Duration, Utc};

use std::collections::HashMap;

use crate::config::MAX_INTERVAL_MINUTES;
use crate::model::SourceState;
use crate::store::Store;
use crate::time_ago::relative_time;

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
/// A source that hasn't fetched successfully for this long counts as failing, unless it
/// is polled so rarely that two polls take longer; see `overdue_after`.
pub const FAILING_WITHOUT_OK_DAYS: i64 = 3;
/// A working source without new items for this long (about six months) counts as stale.
pub const STALE_AFTER_DAYS: i64 = 182;

/// How long without a successful fetch makes a source polled every `interval_minutes`
/// count as failing: 3 days, or two polls for sources polled less often.
fn overdue_after(interval_minutes: u64) -> Duration {
    // Intervals are at most a week; the clamp only keeps the arithmetic in range.
    let minutes = interval_minutes.min(MAX_INTERVAL_MINUTES) as i64;
    let two_polls = Duration::minutes(2 * minutes);
    two_polls.max(Duration::days(FAILING_WITHOUT_OK_DAYS))
}

/// Every source's fetch state and the date of its newest item.
pub struct FetchHistory {
    states: HashMap<String, SourceState>,
    latest_items: HashMap<String, DateTime<Utc>>,
}

impl FetchHistory {
    pub fn load(store: &Store) -> anyhow::Result<Self> {
        Ok(FetchHistory {
            states: store.source_states()?,
            latest_items: store.latest_item_dates()?,
        })
    }

    /// One source's history; a source never fetched has an empty one.
    pub fn source(&self, source_id: &str) -> SourceHistory {
        SourceHistory {
            state: self.states.get(source_id).cloned().unwrap_or_default(),
            latest_item: self.latest_items.get(source_id).copied(),
        }
    }
}

/// A source's fetch state and the date of its newest item, duplicates included.
#[derive(Debug, Clone, Default)]
pub struct SourceHistory {
    pub state: SourceState,
    pub latest_item: Option<DateTime<Utc>>,
}

impl SourceHistory {
    /// The source's health, given whether it is switched on and how often it is polled.
    pub fn health(&self, enabled: bool, interval_minutes: u64, now: DateTime<Utc>) -> Health {
        let state = &self.state;
        let older_than =
            |dt: Option<DateTime<Utc>>, limit: Duration| dt.is_none_or(|dt| now - dt > limit);
        let overdue = older_than(state.last_ok, overdue_after(interval_minutes));
        if !enabled {
            Health::Disabled
        } else if state.consecutive_failures > 0 {
            if state.consecutive_failures >= FAILING_AFTER || overdue {
                Health::Failing
            } else {
                Health::Retrying
            }
        } else if state.last_ok.is_none() {
            Health::Pending
        } else if overdue {
            Health::Failing
        } else if older_than(self.latest_item, Duration::days(STALE_AFTER_DAYS)) {
            Health::Stale
        } else {
            Health::Ok
        }
    }

    /// Why the source has this health, in a few words, e.g. for `gen check-config`.
    /// Empty for healthy sources.
    pub fn reason(&self, health: Health, now: DateTime<Utc>) -> String {
        let ago = |dt| relative_time(dt, now);
        match health {
            Health::Failing if self.state.consecutive_failures == 0 => {
                let last_ok = self.state.last_ok.map_or_else(|| "never".into(), ago);
                format!("last OK {last_ok}, not fetched since")
            }
            Health::Failing | Health::Retrying => {
                let n = self.state.consecutive_failures;
                let last_ok = self.state.last_ok.map_or_else(
                    || "never OK".to_string(),
                    |dt| format!("last OK {}", ago(dt)),
                );
                let error = self.state.last_error.as_deref().unwrap_or("unknown error");
                let plural = if n == 1 { "" } else { "s" };
                format!("{n} failure{plural} in a row, {last_ok}: {error}")
            }
            Health::Stale => self.latest_item.map_or_else(
                || "no items yet".to_string(),
                |dt| format!("newest item {}", ago(dt)),
            ),
            Health::Disabled | Health::Pending | Health::Ok => String::new(),
        }
    }
}

/// Whether a poll that moved a source from `before` to `after` should alert the channels
/// with health alerts: a source that worked (or was retrying) now fails. A source that
/// stays failing, or fails on its first fetch, doesn't alert.
pub fn starts_failing(before: Health, after: Health) -> bool {
    after == Health::Failing && matches!(before, Health::Ok | Health::Retrying | Health::Stale)
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

    fn history(state: SourceState, latest_item: Option<DateTime<Utc>>) -> SourceHistory {
        SourceHistory { state, latest_item }
    }

    fn classify(
        enabled: bool,
        state: &SourceState,
        latest_item: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Health {
        classify_every(60, enabled, state, latest_item, now)
    }

    fn classify_every(
        interval_minutes: u64,
        enabled: bool,
        state: &SourceState,
        latest_item: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Health {
        history(state.clone(), latest_item).health(enabled, interval_minutes, now)
    }

    const WEEKLY: u64 = 7 * 24 * 60;

    fn reason(
        health: Health,
        state: &SourceState,
        latest_item: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> String {
        history(state.clone(), latest_item).reason(health, now)
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
            classify_every(WEEKLY, true, &state(Some(7), 0), days_ago(7), now()),
            Health::Ok
        );
        // One failed weekly poll is retried before it counts as failing.
        assert_eq!(
            classify_every(WEEKLY, true, &state(Some(7), 1), days_ago(7), now()),
            Health::Retrying
        );
        // But not two polls' worth of silence.
        assert_eq!(
            classify_every(WEEKLY, true, &state(Some(15), 1), days_ago(15), now()),
            Health::Failing
        );
    }

    #[test]
    fn sources_not_fetched_for_over_three_days_are_failing() {
        // Hourly source whose last successful fetch was 4 days ago and that hasn't been
        // fetched since, e.g. polling stopped.
        assert_eq!(
            classify(true, &state(Some(4), 0), days_ago(4), now()),
            Health::Failing
        );
        assert_eq!(
            reason(Health::Failing, &state(Some(4), 0), days_ago(4), now()),
            "last OK 4 days ago, not fetched since"
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
    fn alerts_only_when_a_working_source_starts_failing() {
        use Health::*;
        let cases = [
            (Ok, Failing, true),
            (Retrying, Failing, true),
            (Stale, Failing, true),
            (Failing, Failing, false),
            // The first fetch of a new source.
            (Pending, Failing, false),
            (Disabled, Failing, false),
            (Ok, Retrying, false),
            (Failing, Ok, false),
        ];
        for (before, after, expected) in cases {
            assert_eq!(
                starts_failing(before, after),
                expected,
                "{before:?} -> {after:?}"
            );
        }
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
