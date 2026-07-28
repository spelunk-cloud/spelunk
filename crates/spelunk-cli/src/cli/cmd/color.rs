//! Centralised policy for whether stdout output includes ANSI SGR color
//! escapes.
//!
//! Every command that hand-writes `\x1b[...m` color codes should print
//! through [`cprintln`] instead of `println!`, so the on/off decision lives
//! in exactly one place ([`color_enabled`]) instead of being re-derived (or
//! forgotten) per command.

use std::io::IsTerminal as _;
use std::sync::OnceLock;

/// `--color` flag value.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorChoice {
    /// Color on when stdout is a terminal and `NO_COLOR` is unset (default).
    #[default]
    Auto,
    /// Always emit color, regardless of tty state or `NO_COLOR`.
    Always,
    /// Never emit color.
    Never,
}

static CHOICE: OnceLock<ColorChoice> = OnceLock::new();

/// Record the effective `--color` choice for this process. Called once from
/// `main`, before any command prints output.
pub(crate) fn set_color_choice(choice: ColorChoice) {
    let _ = CHOICE.set(choice);
}

/// Pure decision function, so the policy is unit-testable without a real tty
/// or mutating process env: given the `--color` flag, the raw `NO_COLOR`
/// value (if set), and whether stdout is a terminal, should output be
/// colored?
///
/// `Always`/`Never` are unconditional overrides. Otherwise (`Auto`),
/// `NO_COLOR` (<https://no-color.org>; present and non-empty) forces color
/// off regardless of tty state; failing that, color follows the tty.
pub(crate) fn resolve_color(
    choice: ColorChoice,
    no_color_env: Option<&str>,
    stdout_is_terminal: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            let no_color = no_color_env.is_some_and(|v| !v.is_empty());
            !no_color && stdout_is_terminal
        }
    }
}

/// Whether ANSI color should be emitted on stdout right now.
pub(crate) fn color_enabled() -> bool {
    resolve_color(
        CHOICE.get().copied().unwrap_or_default(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::io::stdout().is_terminal(),
    )
}

/// `println!`, but strips ANSI SGR escapes from the formatted line when
/// [`color_enabled`] is false.
macro_rules! cprintln {
    () => { println!() };
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        if $crate::cli::cmd::color::color_enabled() {
            println!("{line}");
        } else {
            println!("{}", spelunk_core::utils::strip_ansi(&line));
        }
    }};
}
pub(crate) use cprintln;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_off_when_stdout_is_not_a_terminal() {
        assert!(!resolve_color(ColorChoice::Auto, None, false));
    }

    #[test]
    fn auto_is_on_when_stdout_is_a_terminal_and_no_color_unset() {
        assert!(resolve_color(ColorChoice::Auto, None, true));
    }

    #[test]
    fn no_color_wins_even_on_a_terminal() {
        assert!(!resolve_color(ColorChoice::Auto, Some("1"), true));
    }

    #[test]
    fn empty_no_color_does_not_disable_color() {
        // no-color.org: the convention only fires when the var is "present
        // and not an empty string" — an exported-but-empty NO_COLOR must not
        // flip color off.
        assert!(resolve_color(ColorChoice::Auto, Some(""), true));
    }

    #[test]
    fn explicit_always_overrides_no_color_and_non_tty() {
        assert!(resolve_color(ColorChoice::Always, Some("1"), false));
    }

    #[test]
    fn explicit_never_overrides_a_terminal() {
        assert!(!resolve_color(ColorChoice::Never, None, true));
    }
}
