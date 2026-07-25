//! Issue #1126 — shared validation for `from`/`to`/`since`/`until`
//! date bounds that are compared against a string-stored timestamp
//! column.
//!
//! Every fleet timestamp column (`obs_events.at`, `host_perf_samples.at`,
//! `agents.last_heartbeat`, `execution_results.finished_at` /
//! `recorded_at`, `inventory_history.observed_at`, …) is declared
//! `TIMESTAMP`. That declared type contains none of SQLite's affinity
//! keywords (`INT`, `CHAR`/`CLOB`/`TEXT`, `BLOB`, `REAL`/`FLOA`/`DOUB`),
//! so by the rule-5 fallback the column gets NUMERIC affinity — but that
//! is a no-op here: sqlx encodes `DateTime<Utc>` as an RFC3339 string,
//! which isn't a losslessly-convertible numeric literal, so SQLite keeps
//! both the stored value and the bound parameter in the TEXT storage
//! class and compares them with BINARY collation. So every
//! `col >= ? AND col < ?` gate that binds a parsed `DateTime<Utc>` is a
//! *byte-wise string* comparison in practice.
//!
//! That tracks chronological order only while both operands render as the
//! fixed-width 4-digit-year form. chrono accepts years up to ±262143 and
//! renders anything outside `0..=9999` sign-prefixed
//! (`+010000-01-01T…`); `'+'` (0x2B) and `'-'` (0x2D) sort below every
//! digit, so such a bound compares "less than" every stored row and
//! inverts the filter wholesale — the endpoint answers a "year 10000
//! onward" window with the entire table, 200 OK, no warning. Every real
//! stored timestamp has a 4-digit year, so rejecting out-of-range bounds
//! turns away only inputs no honest caller wants — and a wrong result is
//! worse than a 400.
//!
//! First fixed on `/api/obs_events` (#1076 → #1125); this module is the
//! fleet-wide extraction so every date-bounded list endpoint rejects the
//! same class of bound before it reaches its query.

use chrono::{DateTime, Datelike, Utc};

/// Is a single date bound safe to compare byte-wise against a
/// string-stored (RFC3339) timestamp column? True exactly when its year
/// is the fixed-width `0..=9999` form.
pub fn bound_in_range(dt: DateTime<Utc>) -> bool {
    (0..=9999).contains(&dt.year())
}

/// Convenience for a handler that carries several optional bounds: true
/// when every present bound is in range. `None`s are ignored, so an
/// absent bound never trips the guard.
pub fn bounds_in_range<I>(bounds: I) -> bool
where
    I: IntoIterator<Item = Option<DateTime<Utc>>>,
{
    bounds.into_iter().flatten().all(bound_in_range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn bound_in_range_accepts_four_digit_years_only() {
        let at = |y| Utc.with_ymd_and_hms(y, 1, 1, 0, 0, 0).unwrap();
        // In range: the whole fixed-width-year span, endpoints included.
        assert!(bound_in_range(at(2026)));
        assert!(bound_in_range(at(0))); // renders `0000-…`
        assert!(bound_in_range(at(9999)));
        // Out of range: chrono sign-prefixes these, breaking lexical order.
        assert!(!bound_in_range(at(10000)));
        assert!(!bound_in_range(at(-1)));
    }

    #[test]
    fn bounds_in_range_ignores_none_and_all_must_pass() {
        let at = |y| Some(Utc.with_ymd_and_hms(y, 1, 1, 0, 0, 0).unwrap());
        // All-None → vacuously in range.
        assert!(bounds_in_range([None, None]));
        // A lone in-range bound passes; unrelated Nones don't matter.
        assert!(bounds_in_range([at(2026), None]));
        assert!(bounds_in_range([at(0), at(9999)]));
        // A single out-of-range bound fails the whole set, in any slot.
        assert!(!bounds_in_range([at(2026), at(10000)]));
        assert!(!bounds_in_range([at(10000), at(2026)]));
    }
}
