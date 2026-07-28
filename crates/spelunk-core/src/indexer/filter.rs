//! Index-time file filter: skips generated, vendored, minified, and
//! machine-data files that are committed to the repo (so `.gitignore` never
//! catches them) yet carry near-zero retrieval value while costing real
//! embed/parse wall-clock.
//!
//! This is a **separate layer** from the unconditional sensitive-file exclusion
//! (`.env`, `*.pem`, private keys) applied by the walker's `OverrideBuilder` in
//! `collect_files`. That layer is not user-overridable; nothing here can
//! re-include a sensitive file, because sensitive files are dropped by the walk
//! before this filter ever sees them.
//!
//! Matching uses gitignore syntax via `ignore::gitignore`: built-in defaults are
//! added first, user lines second, and matching is last-match-wins, so a user
//! `!pattern` line re-includes a path the defaults would drop.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Built-in exclude globs (gitignore syntax). Applied first; user lines layer on
/// top with last-match-wins semantics, so `!glob` in user config re-includes.
///
/// Covers package lockfiles, minified assets, common vendored/generated
/// directories, protobuf/codegen outputs, and bulk machine-data (schemas,
/// translation/locale JSON).
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // package lockfiles (machine-written, huge, no recall value)
    "package-lock.json",
    "npm-shrinkwrap.json",
    "packages.lock.json",
    // minified assets
    "*.min.js",
    "*.min.css",
    // vendored / generated directories
    "vendor/",
    "node_modules/",
    "third_party/",
    "dist/",
    "generated/",
    "__generated__/",
    // generated file markers by name
    "*.generated.*",
    "*.gen.go",
    "*.gen.ts",
    "zz_generated*.go",
    // protobuf / grpc codegen
    "*.pb.go",
    "*.pb.cc",
    "*.pb.h",
    "*_pb2.py",
    "*_pb2_grpc.py",
    "*_pb.js",
    "*_pb.d.ts",
    // bulk machine-data
    "schema.json",
    "*.schema.json",
    "**/translations/**/*.json",
    "**/locales/**/*.json",
    "**/locale/**/*.json",
    "**/i18n/**/*.json",
];

/// Sentinel `from` paths recorded on each glob so a match can report whether it
/// came from the built-in defaults or from user config.
const SRC_DEFAULT: &str = "<default>";
const SRC_USER: &str = "<user>";

/// Max bytes read from the head of a file when sniffing for a generated marker.
const MARKER_HEAD_BYTES: usize = 4 * 1024;
/// Number of leading lines a generated marker must appear within.
const MARKER_MAX_LINES: usize = 5;

/// Which glob matched, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchInfo {
    /// The glob as written (e.g. `node_modules/` or `!keep.min.js`).
    pub pattern: String,
    /// True if the glob is a built-in default, false if it came from user config.
    pub from_default: bool,
}

/// Outcome of testing one path against the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No exclude matched (or only the sensitive layer, handled elsewhere): keep.
    Keep,
    /// An exclude glob matched: drop the path. Carries the matched glob.
    Exclude(MatchInfo),
    /// A user `!` line re-included the path: keep it AND exempt it from
    /// generated-marker detection (the user asked for it explicitly).
    ForceInclude(MatchInfo),
}

/// Compiled index filter: the layered gitignore matcher plus the
/// generated-marker toggle.
#[derive(Debug, Clone)]
pub struct IndexFilter {
    gi: Gitignore,
    detect_generated: bool,
}

impl IndexFilter {
    /// Build a filter from user excludes and the two toggles.
    ///
    /// `use_default_excludes` prepends [`DEFAULT_EXCLUDES`]; user lines are added
    /// after so they win on ties. `detect_generated` gates the `@generated` /
    /// `Code generated ... DO NOT EDIT.` header sniff.
    pub fn build(
        user_excludes: &[String],
        use_default_excludes: bool,
        detect_generated: bool,
    ) -> anyhow::Result<Self> {
        // Root "" so paths are matched exactly as passed (already project-relative).
        let mut b = GitignoreBuilder::new("");
        if use_default_excludes {
            let from = Some(PathBuf::from(SRC_DEFAULT));
            for line in DEFAULT_EXCLUDES {
                b.add_line(from.clone(), line)?;
            }
        }
        let from = Some(PathBuf::from(SRC_USER));
        for line in user_excludes {
            b.add_line(from.clone(), line)?;
        }
        let gi = b.build()?;
        Ok(Self {
            gi,
            detect_generated,
        })
    }

    /// Whether generated-marker sniffing is enabled.
    pub fn detect_generated(&self) -> bool {
        self.detect_generated
    }

    /// Classify a project-relative path against the path itself only (no
    /// ancestor lookup, no file read).
    ///
    /// This is the hot-loop entry: during the walk, excluded ancestor
    /// directories are already pruned by [`IndexFilter::prune_dir`], so a plain
    /// per-path match is both correct and cheap. Use [`IndexFilter::classify`]
    /// when the caller has no walk hierarchy (e.g. `spelunk chunks <path>`).
    pub fn decide(&self, rel_path: &Path, is_dir: bool) -> Decision {
        Self::from_match(self.gi.matched(rel_path, is_dir))
    }

    /// Classify a project-relative path, also matching against any excluded
    /// parent directory (e.g. a file under `node_modules/`). More expensive than
    /// [`IndexFilter::decide`]; use it when there is no walk to prune ancestors,
    /// such as explaining why `spelunk chunks <path>` found nothing.
    pub fn classify(&self, rel_path: &Path, is_dir: bool) -> Decision {
        Self::from_match(self.gi.matched_path_or_any_parents(rel_path, is_dir))
    }

    fn from_match(m: ignore::Match<&ignore::gitignore::Glob>) -> Decision {
        match m {
            ignore::Match::None => Decision::Keep,
            ignore::Match::Ignore(glob) => Decision::Exclude(Self::info(glob)),
            ignore::Match::Whitelist(glob) => Decision::ForceInclude(Self::info(glob)),
        }
    }

    /// True if this directory should be pruned from the walk (an exclude glob
    /// matched it). A user `!` re-include of the directory keeps it. Note that
    /// gitignore semantics do not let a `!file` line re-include through an
    /// already-excluded parent directory, matching git itself.
    pub fn prune_dir(&self, rel_path: &Path) -> bool {
        matches!(self.decide(rel_path, true), Decision::Exclude(_))
    }

    fn info(glob: &ignore::gitignore::Glob) -> MatchInfo {
        let from_default = glob
            .from()
            .map(|p| p == Path::new(SRC_DEFAULT))
            .unwrap_or(false);
        MatchInfo {
            pattern: glob.original().to_string(),
            from_default,
        }
    }
}

/// Regex for the Go-style `// Code generated by <tool>. DO NOT EDIT.` header.
/// Compiled once. Applied per line (anchored), not across the whole buffer.
fn marker_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^// Code generated by .* DO NOT EDIT\.$").expect("valid marker regex")
    })
}

/// Return the self-declared generated marker in a file's head, if any.
///
/// Reads at most [`MARKER_HEAD_BYTES`] and inspects the first
/// [`MARKER_MAX_LINES`] lines for a literal `@generated` token or the Go
/// `Code generated ... DO NOT EDIT.` header. Self-declaration only: no
/// line-length, entropy, or statistical heuristics. Returns the marker text for
/// debug logging, or `None`.
pub fn generated_marker(path: &Path) -> Option<&'static str> {
    let head = read_head(path)?;
    marker_in_head(&head)
}

/// Marker scan over an already-read head string (unit-testable core).
fn marker_in_head(head: &str) -> Option<&'static str> {
    for line in head.lines().take(MARKER_MAX_LINES) {
        if line.contains("@generated") {
            return Some("@generated");
        }
        if marker_regex().is_match(line.trim_end()) {
            return Some("Code generated ... DO NOT EDIT.");
        }
    }
    None
}

/// Read up to [`MARKER_HEAD_BYTES`] from the file, lossily decoded so a binary
/// or non-UTF-8 head never errors the scan.
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; MARKER_HEAD_BYTES];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_filter() -> IndexFilter {
        IndexFilter::build(&[], true, true).unwrap()
    }

    fn is_excluded(f: &IndexFilter, rel: &str, is_dir: bool) -> bool {
        matches!(f.decide(Path::new(rel), is_dir), Decision::Exclude(_))
    }

    #[test]
    fn each_default_class_is_excluded() {
        let f = default_filter();
        // lockfiles
        assert!(is_excluded(&f, "package-lock.json", false));
        assert!(is_excluded(&f, "npm-shrinkwrap.json", false));
        assert!(is_excluded(&f, "packages.lock.json", false));
        // minified
        assert!(is_excluded(&f, "app.min.js", false));
        assert!(is_excluded(&f, "site.min.css", false));
        // vendored / generated directories (as dirs)
        assert!(is_excluded(&f, "vendor", true));
        assert!(is_excluded(&f, "node_modules", true));
        assert!(is_excluded(&f, "third_party", true));
        assert!(is_excluded(&f, "dist", true));
        assert!(is_excluded(&f, "generated", true));
        assert!(is_excluded(&f, "__generated__", true));
        // A file nested under a vendored dir: `decide` matches the path itself
        // only (ancestors are pruned during the walk), so the parent-aware
        // `classify` is what recognises it out of walk context.
        assert!(matches!(
            f.classify(Path::new("node_modules/react/index.js"), false),
            Decision::Exclude(_)
        ));
        // generated file markers
        assert!(is_excluded(&f, "api.generated.ts", false));
        assert!(is_excluded(&f, "types.gen.go", false));
        assert!(is_excluded(&f, "client.gen.ts", false));
        assert!(is_excluded(&f, "zz_generated_deepcopy.go", false));
        // protobuf / grpc
        assert!(is_excluded(&f, "user.pb.go", false));
        assert!(is_excluded(&f, "user.pb.cc", false));
        assert!(is_excluded(&f, "user.pb.h", false));
        assert!(is_excluded(&f, "user_pb2.py", false));
        assert!(is_excluded(&f, "user_pb2_grpc.py", false));
        assert!(is_excluded(&f, "user_pb.js", false));
        assert!(is_excluded(&f, "user_pb.d.ts", false));
        // machine-data
        assert!(is_excluded(&f, "schema.json", false));
        assert!(is_excluded(&f, "openapi.schema.json", false));
        assert!(is_excluded(&f, "src/translations/en/messages.json", false));
        assert!(is_excluded(&f, "src/locales/en.json", false));
        assert!(is_excluded(&f, "app/locale/fr.json", false));
        assert!(is_excluded(&f, "src/i18n/en.json", false));
    }

    #[test]
    fn survivors_are_kept() {
        let f = default_filter();
        for rel in [
            "src/lib.rs",
            "package.json",
            "tsconfig.json",
            "README.md",
            "tests/foo_test.rs",
            // .ts under i18n/ survives: the i18n default only excludes *.json
            "src/i18n/index.ts",
        ] {
            assert_eq!(
                f.decide(Path::new(rel), false),
                Decision::Keep,
                "{rel} must be kept"
            );
        }
    }

    #[test]
    fn user_bang_line_reincludes() {
        // A user `!` re-include wins over a default (last-match-wins) and yields
        // ForceInclude (exempt from marker detection).
        let f = IndexFilter::build(&["!vendored.min.js".to_string()], true, true).unwrap();
        match f.decide(Path::new("vendored.min.js"), false) {
            Decision::ForceInclude(mi) => {
                assert!(!mi.from_default, "the re-include came from user config");
            }
            other => panic!("expected ForceInclude, got {other:?}"),
        }
        // A different .min.js is still excluded.
        assert!(is_excluded(&f, "other.min.js", false));
    }

    #[test]
    fn use_default_excludes_false_disables_builtins() {
        let f = IndexFilter::build(&[], false, true).unwrap();
        assert_eq!(
            f.decide(Path::new("package-lock.json"), false),
            Decision::Keep
        );
        assert_eq!(f.decide(Path::new("node_modules"), true), Decision::Keep);
    }

    #[test]
    fn user_excludes_apply_without_defaults() {
        // Defaults off, but a user glob still excludes.
        let f = IndexFilter::build(&["*.bin".to_string()], false, true).unwrap();
        match f.decide(Path::new("blob.bin"), false) {
            Decision::Exclude(mi) => assert!(!mi.from_default),
            other => panic!("expected Exclude, got {other:?}"),
        }
    }

    #[test]
    fn match_info_reports_default_source_and_pattern() {
        let f = default_filter();
        match f.decide(Path::new("node_modules"), true) {
            Decision::Exclude(mi) => {
                assert!(mi.from_default);
                assert_eq!(mi.pattern, "node_modules/");
            }
            other => panic!("expected Exclude, got {other:?}"),
        }
    }

    #[test]
    fn generated_marker_fires_within_first_lines_only() {
        // @generated on line 1.
        assert_eq!(
            marker_in_head("// @generated\nfn a() {}\n"),
            Some("@generated")
        );
        // Go header on line 1.
        assert_eq!(
            marker_in_head("// Code generated by protoc. DO NOT EDIT.\npackage x\n"),
            Some("Code generated ... DO NOT EDIT.")
        );
        // @generated inside the window (line 5) still fires.
        let within = "1\n2\n3\n4\n// @generated\n";
        assert_eq!(marker_in_head(within), Some("@generated"));
        // Past the 5-line window: no match.
        let beyond = "1\n2\n3\n4\n5\n// @generated\n";
        assert_eq!(marker_in_head(beyond), None);
        // Ordinary source: no match.
        assert_eq!(marker_in_head("fn main() {}\n"), None);
        // Near-miss Go header (missing trailing period) must not match.
        assert_eq!(
            marker_in_head("// Code generated by protoc. DO NOT EDIT\n"),
            None
        );
    }

    #[test]
    fn generated_marker_reads_file_head() {
        let dir = tempfile::tempdir().unwrap();
        let generated = dir.path().join("g.rs");
        std::fs::write(&generated, "// @generated by tool\nfn a() {}\n").unwrap();
        assert_eq!(generated_marker(&generated), Some("@generated"));

        let plain = dir.path().join("p.rs");
        std::fs::write(&plain, "fn a() {}\n").unwrap();
        assert_eq!(generated_marker(&plain), None);
    }

    /// Line-window boundary is exactly `MARKER_MAX_LINES` (5): a marker on line 5
    /// fires, the same marker one line later (line 6) does not. Pins the `take(5)`
    /// bound so a later off-by-one can't silently widen or narrow it.
    #[test]
    fn generated_marker_line5_fires_line6_does_not() {
        // Marker on line 5 (four preceding lines): fires.
        assert_eq!(
            marker_in_head("1\n2\n3\n4\n// @generated\n"),
            Some("@generated")
        );
        // Same marker on line 6 (five preceding lines): must NOT fire.
        assert_eq!(marker_in_head("1\n2\n3\n4\n5\n// @generated\n"), None);
        // Go header on line 5 fires; on line 6 it does not.
        assert_eq!(
            marker_in_head("1\n2\n3\n4\n// Code generated by x. DO NOT EDIT.\n"),
            Some("Code generated ... DO NOT EDIT.")
        );
        assert_eq!(
            marker_in_head("1\n2\n3\n4\n5\n// Code generated by x. DO NOT EDIT.\n"),
            None
        );
    }

    /// CRLF line endings must not defeat detection: `str::lines()` strips the
    /// `\r`, and the Go header additionally `trim_end()`s, so both markers still
    /// fire under Windows-style newlines.
    #[test]
    fn generated_marker_handles_crlf() {
        assert_eq!(
            marker_in_head("// @generated\r\nfn a() {}\r\n"),
            Some("@generated")
        );
        assert_eq!(
            marker_in_head("// Code generated by protoc. DO NOT EDIT.\r\npackage x\r\n"),
            Some("Code generated ... DO NOT EDIT.")
        );
    }

    /// A UTF-8 BOM prefix on line 1: the `@generated` substring scan is
    /// position-independent so it still fires. The Go-header scan is anchored at
    /// `^//`, so a BOM (or any) prefix before the `//` defeats it; documented
    /// here as a known boundary, not asserted as desirable.
    #[test]
    fn generated_marker_bom_prefix() {
        let bom = "\u{feff}";
        // `@generated` still detected through a leading BOM.
        assert_eq!(
            marker_in_head(&format!("{bom}// @generated\nfn a() {{}}\n")),
            Some("@generated")
        );
        // Anchored Go header does NOT fire when a BOM precedes the `//`.
        assert_eq!(
            marker_in_head(&format!("{bom}// Code generated by x. DO NOT EDIT.\n")),
            None
        );
    }

    /// The head sniff reads at most `MARKER_HEAD_BYTES` (4 KiB). A marker pushed
    /// past that window by a long leading line is never seen, so the file is not
    /// flagged. Exercised through the real file-reading `generated_marker`, since
    /// the 4 KiB cap lives in `read_head`, not `marker_in_head`.
    #[test]
    fn generated_marker_past_4kib_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        // A single line longer than the head window, then the marker on line 2.
        // The marker is within the first 5 lines but past the 4 KiB byte cap.
        let padding = "x".repeat(MARKER_HEAD_BYTES + 10);
        let f = dir.path().join("big_head.rs");
        std::fs::write(&f, format!("// {padding}\n// @generated\nfn a() {{}}\n")).unwrap();
        assert_eq!(
            generated_marker(&f),
            None,
            "a marker beyond the 4 KiB head window must not be detected"
        );

        // Control: the same marker within the window IS detected.
        let g = dir.path().join("small_head.rs");
        std::fs::write(&g, "// short\n// @generated\nfn a() {}\n").unwrap();
        assert_eq!(generated_marker(&g), Some("@generated"));
    }

    // ── Dir-prune re-include semantics: matches git ──────────────────────────

    /// A user `!file` line CANNOT un-prune an excluded parent directory, but a
    /// user `!dir/` line CAN un-prune the directory itself. `prune_dir` is what
    /// the walk consults per directory, so this is the exact lever that decides
    /// whether the walk descends.
    #[test]
    fn dir_prune_reinclude_semantics() {
        // A `!` on a file *inside* node_modules does not lift the directory prune:
        // the walk still never descends, so the file is unreachable (git parity).
        let file_reinclude =
            IndexFilter::build(&["!node_modules/keep.js".to_string()], true, true).unwrap();
        assert!(
            file_reinclude.prune_dir(Path::new("node_modules")),
            "a !file line must NOT un-prune the excluded parent directory"
        );

        // A `!dir/` line on the directory itself DOES lift the prune.
        let dir_reinclude = IndexFilter::build(&["!vendor/".to_string()], true, true).unwrap();
        assert!(
            !dir_reinclude.prune_dir(Path::new("vendor")),
            "a !dir/ line re-includes the directory, so the walk descends into it"
        );
        // Sibling excluded dirs are unaffected by the vendor re-include.
        assert!(dir_reinclude.prune_dir(Path::new("node_modules")));
    }

    // ── Sensitive-layer invariant: defense-in-depth mutation-kill ────────────

    /// MUTATION-KILL A: the sensitive patterns (`.env`, `*.pem`, private keys)
    /// must NEVER appear in `DEFAULT_EXCLUDES`. They live only in the walker's
    /// non-overridable `OverrideBuilder`; routing them through this
    /// user-tunable gitignore layer would make them re-includable via
    /// `[index].exclude = ["!.env"]`. If a future refactor "consolidates" the
    /// sensitive set into the defaults, this fails.
    #[test]
    fn sensitive_patterns_absent_from_default_excludes() {
        let joined = DEFAULT_EXCLUDES.join("\n").to_lowercase();
        for needle in [
            ".env",
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".p8",
            ".cer",
            ".crt",
            ".der",
            "id_rsa",
            "id_ecdsa",
            "id_ed25519",
            "id_dsa",
            ".keystore",
            ".jks",
            ".netrc",
            ".npmrc",
        ] {
            assert!(
                !joined.contains(needle),
                "sensitive pattern `{needle}` must not be in the user-overridable DEFAULT_EXCLUDES"
            );
        }
    }

    /// MUTATION-KILL B: the index filter itself has NO opinion on sensitive
    /// paths - it returns `Keep`, because sensitive protection lives entirely in
    /// the separate override layer. A regression that made the filter the
    /// sensitive gate (Exclude) would fire here, and a `!` re-include of a
    /// sensitive path resolves through the filter to a plain keep, never a
    /// filter-level protection.
    #[test]
    fn index_filter_has_no_opinion_on_sensitive_paths() {
        let f = default_filter();
        for rel in [".env", "config.pem", "server.key", "id_rsa", "keys.p12"] {
            assert_eq!(
                f.decide(Path::new(rel), false),
                Decision::Keep,
                "the index filter must not be the layer that excludes {rel}"
            );
        }
    }
}
