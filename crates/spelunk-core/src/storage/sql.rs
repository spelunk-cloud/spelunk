//! Storage-internal SQL helpers.

/// Maximum number of bound parameters to place in a single statement.
///
/// SQLite caps bound parameters per statement at `SQLITE_LIMIT_VARIABLE_NUMBER`
/// (default 999 on older builds, 32766 on SQLite >= 3.32). Callers chunk their
/// input lists at this size and run one statement per chunk so that a large
/// input slice never exceeds the limit at prepare/bind time. For a statement
/// that binds the same slice twice, halve this budget.
pub(crate) const SQLITE_MAX_BIND: usize = 30_000;

/// Build a comma-separated list of `n` anonymous bind placeholders: `?,?,?`.
///
/// Returns an empty string for `n == 0` (callers must early-return on empty
/// input rather than emit an empty `IN ()` clause).
///
/// Anonymous `?` placeholders are used (rather than numbered `?N`) so that a
/// query needing the same value set in two clauses can simply bind the value
/// slice twice — no `?N` bookkeeping.
pub(crate) fn placeholders(n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut s = "?,".repeat(n);
    s.pop(); // drop trailing comma
    s
}

#[cfg(test)]
mod tests {
    use super::placeholders;

    #[test]
    fn zero_is_empty() {
        assert_eq!(placeholders(0), "");
    }

    #[test]
    fn one_placeholder() {
        assert_eq!(placeholders(1), "?");
    }

    #[test]
    fn three_placeholders() {
        assert_eq!(placeholders(3), "?,?,?");
    }
}
