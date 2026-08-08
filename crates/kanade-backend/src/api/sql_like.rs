//! Shared SQLite `LIKE` pattern construction for operator-supplied text.
//!
//! Any filter that puts operator text into a `LIKE` pattern has to escape
//! the metacharacters first, or the text stops being text: `%` matches
//! everything and `_` matches any single character, so an unescaped
//! search silently widens to far more rows than asked for and comes back
//! looking like a match. For a fleet filter that is not cosmetic — a
//! metadata search for `_` would return the whole fleet and read as
//! "every machine has this attribute".
//!
//! The rule lives here because it kept being re-derived. `agents.rs`
//! already carried a note that three copies had drifted apart, and by the
//! time #1343 wanted the same escaping for `obs_events` there were still
//! two more open-coded closures in `inventory.rs`. A rule that is
//! security-relevant and duplicated per call site is a rule that will
//! diverge; sharing it the way `time_bounds` is shared (a small submodule
//! several `api` handlers import) makes divergence impossible rather than
//! merely discouraged.
//!
//! Every caller must declare `ESCAPE '\'` in its SQL. Note that in a Rust
//! string literal that has to be written `ESCAPE '\\'` — writing `'\'`
//! produces an EMPTY escape clause, which SQLite rejects only when the
//! `LIKE` is actually evaluated, so the mistake hides behind any
//! short-circuiting `IS NULL` gate and passes every unfiltered query.

/// Escape the LIKE metacharacters (`\` `%` `_`) so `s` matches literally
/// under a `LIKE … ESCAPE '\'` clause. Single source of truth for the
/// escape rule; the `*_like` wrappers add their own `%` framing on top.
///
/// The backslash must be replaced FIRST — escaping `%` and `_` introduces
/// new backslashes, and a later pass over them would double the ones this
/// function just added.
pub(crate) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `LIKE '%value%'` — a contains match.
pub(crate) fn contains_like(value: &str) -> String {
    format!("%{}%", escape_like(value))
}

/// `LIKE 'value%'` — a starts-with match.
pub(crate) fn starts_like(value: &str) -> String {
    format!("{}%", escape_like(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metacharacters_are_escaped() {
        assert_eq!(escape_like("a%b"), "a\\%b");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn the_backslash_pass_runs_before_the_others() {
        // If `%` were escaped first, its new backslash would be escaped
        // again by the backslash pass and the pattern would look for a
        // literal `\` followed by `%` instead of a literal `%`.
        assert_eq!(escape_like("%"), "\\%");
        assert_eq!(escape_like("\\%"), "\\\\\\%");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(escape_like("Sales"), "Sales");
        assert_eq!(contains_like("Sales"), "%Sales%");
        assert_eq!(starts_like("Sales"), "Sales%");
    }

    #[test]
    fn wrappers_frame_the_escaped_value_not_the_raw_one() {
        // The framing `%` must stay a wildcard while the value's own `%`
        // must not — that distinction is the whole point.
        assert_eq!(contains_like("100%"), "%100\\%%");
        assert_eq!(starts_like("100%"), "100\\%%");
    }

    #[test]
    fn empty_input_yields_a_pure_wildcard() {
        // Callers must therefore treat blank as "no filter" BEFORE
        // building a pattern: `%%` matches every non-null value, which is
        // a very different query from applying no condition at all.
        assert_eq!(contains_like(""), "%%");
    }
}
