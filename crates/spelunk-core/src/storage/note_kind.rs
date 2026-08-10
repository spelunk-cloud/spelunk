//! Canonical set of memory-entry kinds and the strict parser that guards
//! `spelunk memory add --kind`.
//!
//! A memory entry's `kind` steers every retrieval path: `spelunk context`
//! selects handoffs/questions/decisions/requirements by kind, `memory failures`
//! selects `antipattern`, and `memory list --kind` filters on an exact match.
//! An entry stored under a kind outside this set (a typo like `decisions`, or a
//! bogus value) is therefore invisible to all of them. This module is the
//! single source of truth for which kinds are valid, so a kind added here
//! becomes storable — and, once a retrieval path selects on it, retrievable —
//! everywhere.

/// The nine canonical kinds a memory entry may have.
///
/// Retrieval paths (`spelunk context`, `memory failures`) deliberately select
/// on a *subset* of these — not every valid kind appears in the default context
/// view — but every kind they select on must be a member here, so validation
/// and retrieval cannot drift (guarded by tests).
pub const NOTE_KINDS: [&str; 9] = [
    "decision",
    "context",
    "requirement",
    "note",
    "question",
    "answer",
    "handoff",
    "intent",
    "antipattern",
];

/// Whether `kind` is one of the canonical [`NOTE_KINDS`].
pub fn is_valid_note_kind(kind: &str) -> bool {
    NOTE_KINDS.contains(&kind)
}

/// Strict parser for a user-supplied `--kind`.
///
/// Returns the kind unchanged when it is canonical, or an error that names the
/// offending value and lists every valid kind. Shaped as a clap value parser
/// (`Fn(&str) -> Result<String, String>`) so an invalid `--kind` is rejected at
/// argument-parse time — before any store is opened — with a non-zero exit.
pub fn parse_note_kind(kind: &str) -> Result<String, String> {
    if is_valid_note_kind(kind) {
        Ok(kind.to_string())
    } else {
        Err(format!(
            "unknown kind '{kind}' — valid kinds are: {}",
            NOTE_KINDS.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The canonical set is exactly the nine documented kinds — pins the
    // contract so neither an accidental addition nor a removal slips through.
    #[test]
    fn canonical_set_is_exactly_the_nine_documented_kinds() {
        let mut got = NOTE_KINDS.to_vec();
        got.sort_unstable();
        let mut want = vec![
            "answer",
            "antipattern",
            "context",
            "decision",
            "handoff",
            "intent",
            "note",
            "question",
            "requirement",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn every_canonical_kind_is_valid() {
        for kind in NOTE_KINDS {
            assert!(is_valid_note_kind(kind), "{kind} should be valid");
        }
    }

    #[test]
    fn unknown_and_typo_kinds_are_invalid() {
        for kind in ["", "bogus", "decisions", "desicion", "Decision", "notes"] {
            assert!(!is_valid_note_kind(kind), "{kind:?} should be invalid");
        }
    }

    #[test]
    fn parse_accepts_every_canonical_kind_unchanged() {
        for kind in NOTE_KINDS {
            assert_eq!(parse_note_kind(kind).as_deref(), Ok(kind));
        }
    }

    // The rejection message must name the offending value and list every valid
    // kind, so the CLI (and any other caller) can surface a self-correcting
    // error.
    #[test]
    fn parse_rejects_unknown_naming_value_and_listing_kinds() {
        let err = parse_note_kind("decisions").expect_err("must reject");
        assert!(
            err.contains("decisions"),
            "must name the offending value: {err}"
        );
        for kind in NOTE_KINDS {
            assert!(err.contains(kind), "must list valid kind {kind}: {err}");
        }
    }
}
