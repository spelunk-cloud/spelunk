// Crate-wide invariants on how the daemon handles the credential it sends
// upstream. Each is a "never do X" property of the whole crate rather than of
// any one call, so each is checked over the whole crate: a runtime test can
// only prove that the paths it happens to exercise stay clear.
//
// The detached daemon must never open the OS keychain. It usually runs with no
// user session, so a keychain read there is an authorization prompt nobody can
// see or answer, and the process would hang rather than fail.
//
// The constraint is enforced by construction: the spawning CLI resolves the
// credential and hands it over out of band, and this crate reaches for a secret
// store nowhere at all.

use std::path::Path;

// Identifiers, not prose: the modules explain in comments why they never touch
// a keychain, and those sentences must stay writable.
const FORBIDDEN: &[&str] = &[
    "secret_store",
    "SecretStore",
    "default_secret_store",
    "KeyringStore",
    "keyring::",
    "KEY_LLM_KEY",
    "KEY_SERVER_KEY",
];

fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("reading the server source tree") {
        let path = entry.expect("reading a source dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_server_crate_never_reaches_for_a_secret_store() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.len() > 1,
        "the source scan found almost nothing, so it would pass vacuously: {files:?}"
    );

    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("reading a server source file");
        for (lineno, line) in text.lines().enumerate() {
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    offenders.push(format!("{}:{}: {needle}", file.display(), lineno + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "spelunk-server must resolve no credential from a secret store; the CLI passes \
         the LLM key in via SPELUNK_LLM_KEY or --llm-key-file instead. Found: {offenders:#?}"
    );
}

// The other half of the same constraint: no key flag or key value may be
// derivable from a store, so the only inbound channels are the documented ones.
#[test]
fn the_llm_credential_has_exactly_three_inbound_channels() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);

    let mut sources = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("reading a server source file");
        if text.contains("args.llm_key.as_deref()") {
            sources.push("--llm-key");
        }
        if text.contains("args.llm_key_file.as_deref()") {
            sources.push("--llm-key-file");
        }
        if text.contains("std::env::var(\"SPELUNK_LLM_KEY\")") {
            sources.push("SPELUNK_LLM_KEY");
        }
    }
    sources.sort_unstable();

    assert_eq!(
        sources,
        vec!["--llm-key", "--llm-key-file", "SPELUNK_LLM_KEY"],
        "the credential's inbound channels changed; re-check the precedence tests and \
         the constraint that no keychain is involved"
    );
}

// `Args` derives `Debug` and holds `--llm-key` (and the server's own `--key`)
// as plain `String`s, so any `{:?}` of the parsed args would print both
// verbatim into the daemon log. Nothing renders them today, and this is what
// keeps it that way: the CLI side hand-wrote a redacting `Debug` on `LlmSpawn`
// for the same reason.
#[test]
fn the_parsed_args_are_never_rendered_through_debug() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);

    let renderings = ["{args:?}", "{:?}\", args", "?args", "args = ?"];
    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("reading a server source file");
        for (lineno, line) in text.lines().enumerate() {
            for needle in renderings {
                if line.contains(needle) {
                    offenders.push(format!("{}:{}: {needle}", file.display(), lineno + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the parsed args carry credentials in the clear, so they must not be Debug-rendered; \
         redact them first. Found: {offenders:#?}"
    );
}
