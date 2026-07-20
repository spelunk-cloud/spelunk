use anyhow::{Context, Result};

use super::MemoryAddArgs;
use crate::{
    capability,
    config::Config,
    indexer::secrets::contains_secret,
    server_client::ServerInferenceClient,
    storage::{
        GitNotesBackend, MemoryBackend, NoteInput, NoteRecord, RewriteRefStatus,
        append_state_update, append_to_git_notes, now_millis, now_secs, open_memory_backend,
    },
};

pub(super) async fn memory_add(
    args: MemoryAddArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
    pre_init_notes: bool,
) -> Result<()> {
    // Honor the auto-discovered server tier (ADR-004): loopback auto-discovery
    // sets the capability tier without populating `cfg.server_url`, so without
    // this bridge `try_embed_via_server` cannot reach the local embedder and the
    // note is stored without a vector (invisible to semantic `memory search`).
    // Build an effective config that routes inference to the discovered server
    // while leaving `server_url` unset, so `open_memory_backend` still writes the
    // note to the project's local `memory.db` (the single canonical store).
    // On the git-notes paths `mem_path` is a placeholder (explicit `--backend
    // git-notes` or the ADR-068 D3 pre-init carrier); the project is the git
    // repo at CWD, so derive the inference project id from there instead.
    let cwd;
    let placeholder_path = pre_init_notes || backend_override == Some("git-notes");
    let project_root: &std::path::Path = if placeholder_path {
        cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        &cwd
    } else {
        mem_path.parent().unwrap_or(mem_path)
    };
    let tier = capability::get_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    let cfg = &eff_cfg;
    let (title, body) = if let Some(url) = &args.from_url {
        let (fetched_title, fetched_body) = fetch_url_content(url)
            .await
            .with_context(|| format!("fetching {url}"))?;
        let title = args.title.clone().unwrap_or(fetched_title);
        let body = args.body.clone().unwrap_or(fetched_body);
        (title, body)
    } else {
        let title = args
            .title
            .clone()
            .context("--title is required when --from-url is not provided")?;
        let body = match args.body.clone() {
            Some(b) => b,
            None => {
                let t = title.clone();
                tokio::task::spawn_blocking(move || super::open_editor_for_body(&t))
                    .await
                    .context("editor task panicked")?
                    .context("opening editor for body")?
            }
        };
        (title, body)
    };

    let tags: Vec<String> = args
        .tags
        .as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect())
        .unwrap_or_default();

    let files: Vec<String> = args
        .files
        .as_deref()
        .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
        .unwrap_or_default();

    // ── Secret-scan gate (binding requirement #8) ────────────────────────────
    // Checked before ANY persistence (SQLite or git-notes) so no credential
    // can reach either store.  Error message deliberately does not echo the
    // matched text.
    if contains_secret(&title) || contains_secret(&body) {
        anyhow::bail!(
            "memory add: refusing to store entry — title or body matches a secret pattern. \
             Remove the credential and try again. (No data was written to SQLite or git notes.)"
        );
    }

    // Pre-init carrier entries carry no vector (git notes hold none), and
    // semantic `memory search` stays gated until the project is indexed and the
    // carrier hydrates the index (ADR-068 D3/D4).
    let embedding = if pre_init_notes {
        None
    } else {
        let embed_text = format!("title: {title} | text: {body}");
        try_embed_via_server(cfg, &embed_text).await
    };

    let valid_at = args
        .valid_at
        .and_then(|s| super::parse_as_of(Some(&s)).ok().flatten());

    // ── Supersede pre-flight (ADR-068 E4) ────────────────────────────────────
    // If `--supersedes OLD` is given, OLD must still be active — checked
    // *before* any write (SQLite or git-notes), on both storage paths,
    // mirroring `memory supersede`'s existing reject-on-stale-OLD contract.
    // Without this, the SQL layer's `WHERE status = 'active'` guard on the
    // archive-OLD UPDATE silently no-ops on a stale OLD, leaving an orphaned
    // new note plus a conflicting git-notes carrier record — the bug this
    // amendment closes. The read is reused below by the write-through
    // carrier instead of reading OLD a second time.
    let mut backend_for_add: Option<Box<dyn MemoryBackend + Send>> = None;
    let mut old_note_for_carrier = None;
    if let Some(old_id) = args.supersedes {
        let old = if pre_init_notes {
            GitNotesBackend::new().get(old_id).await?
        } else {
            let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
            let old = backend.get(old_id).await?;
            backend_for_add = Some(backend);
            old
        };
        match old {
            Some(note) if note.status == "active" => {
                old_note_for_carrier = Some(note);
            }
            _ => {
                anyhow::bail!("No active memory entry with id {old_id} (old).");
            }
        }
    }

    // Primary store (ADR-004): the local SQLite `memory.db`, an explicit team
    // server, or (with `--backend git-notes`) git notes itself. Pre-init there
    // is no primary; the write-through carrier below is the sole writer, so mint
    // an id the same way the backends do (`now_millis`).
    let (id, created) = if pre_init_notes {
        (now_millis(), true)
    } else {
        let backend = match backend_for_add.take() {
            Some(backend) => backend,
            None => open_memory_backend(cfg, mem_path, backend_override).await?,
        };
        backend
            .add(NoteInput {
                kind: args.kind.clone(),
                title: title.clone(),
                body: body.clone(),
                tags: tags.clone(),
                linked_files: files.clone(),
                embedding,
                source_ref: None,
                valid_at,
                supersedes: args.supersedes,
            })
            .await?
    };

    // ── Git-notes write-through carrier ──────────────────────────────────────
    // The single write path to `refs/notes/spelunk` both pre- and post-`init`,
    // so every note carries an identical record shape. Suppressed only when git
    // notes is already the primary store (explicit `--backend git-notes`), to
    // avoid a double write. Post-`init` it is best-effort (SQLite already holds
    // the entry); pre-`init` it is the sole store, so a failed carry is fatal.
    let write_through =
        pre_init_notes || (cfg.store_in_git_notes && backend_override != Some("git-notes"));
    let mut notes_rewrite_note: Option<&str> = None;
    if write_through {
        let new_entity_id = crate::storage::entity_id::entity_id(&args.kind, &title, &body);
        let record = NoteRecord {
            schema_version: 1,
            id,
            kind: args.kind.clone(),
            title: title.clone(),
            body: body.clone(),
            tags: tags.clone(),
            linked_files: files.clone(),
            created_at: now_secs(),
            status: "active".to_string(),
            source_ref: None,
            valid_at,
            invalid_at: None,
            superseded_by: None,
            // Never-synced local row: no cross-machine id yet.
            remote_id: None,
            entity_id: Some(new_entity_id.clone()),
            superseded_by_entity_id: None,
        };
        // Use process CWD (None) — the CLI is always run from the project root.
        // Secret scan already ran above; no second check needed here.
        match append_to_git_notes(None, &record).await {
            Ok(outcome) => {
                // Visible without RUST_LOG: an unserialized write can lose a
                // concurrent entry, and this is the only channel that reaches
                // the user (ADR-069 D8: proceed unlocked, loudly).
                if let Some(degradation) = outcome.lock_degradation {
                    eprintln!("Warning: {degradation}");
                }
                match outcome.rewrite_ref {
                    // Announce only the call that set it, so a repo says this once.
                    RewriteRefStatus::Configured => {
                        notes_rewrite_note = Some(
                            "Configured git notes.rewriteRef in this repo, so memory now survives \
                             `git commit --amend` and `git rebase`.",
                        );
                    }
                    RewriteRefStatus::Failed => {
                        notes_rewrite_note = Some(
                            "Warning: could not set git notes.rewriteRef, so memory may not survive \
                             `git commit --amend` or `git rebase`. Set it with: \
                             git config --add notes.rewriteRef refs/notes/spelunk",
                        );
                    }
                    RewriteRefStatus::AlreadyCovered => {}
                }
            }
            Err(e) if pre_init_notes => {
                return Err(e.context(
                    "recording memory entry to git notes (no local project store to fall back on)",
                ));
            }
            // Visible without RUST_LOG: a swallowed carry failure is how an
            // entry silently stops traveling with the repo (ADR-069 D8).
            Err(e) => {
                eprintln!(
                    "Warning: entry stored locally, but the git-notes carry failed, \
                     so it will not travel with the repo: {e:#}"
                );
            }
        }

        // ── Carry the OLD entity's supersede edge too ────────────────────────
        // `--supersedes` already archived OLD in the primary store above; the
        // edge itself only travels once a state-update record is appended for
        // OLD's own entry, pointing at NEW's `entity_id` — writing it on NEW's
        // record (the one just written above) would be backwards. Best-effort
        // and non-fatal like the write above: SQLite already holds the
        // authoritative archive.
        //
        // Reuses the pre-flight read above (`old_note_for_carrier`) rather than
        // re-reading OLD a second time — it was already validated `active`
        // there, before either write in this function ran (ADR-068 E4).
        if let Some(old_note) = old_note_for_carrier {
            let old_id = old_note.id;
            let invalid_at = old_note.invalid_at.or_else(|| Some(now_secs()));
            if let Err(e) =
                append_state_update(None, &old_note, "archived", invalid_at, Some(new_entity_id))
                    .await
            {
                eprintln!(
                    "Warning: entry stored locally, but carrying #{old_id}'s \
                     supersede edge to git notes failed, so it will not travel \
                     with the repo: {e:#}"
                );
            }
        }
    }

    if created {
        println!("Stored [{kind}] #{id}: {title}", kind = args.kind);
    } else {
        println!(
            "Already recorded as [{kind}] #{id}: {title}",
            kind = args.kind
        );
    }
    if let Some(line) = notes_rewrite_note {
        println!("{line}");
    }
    Ok(())
}

async fn fetch_url_content(url: &str) -> Result<(String, String)> {
    let gh_issue_re =
        regex::Regex::new(r"https?://github\.com/([^/]+)/([^/]+)/(?:issues|pull)/(\d+)").unwrap();

    if let Some(caps) = gh_issue_re.captures(url) {
        let owner = &caps[1];
        let repo = &caps[2];
        let num = &caps[3];
        let api_path = format!("repos/{owner}/{repo}/issues/{num}");
        let out = tokio::process::Command::new("gh")
            .args(["api", &api_path])
            .output()
            .await;
        if let Ok(out) = out
            && out.status.success()
        {
            let json: serde_json::Value =
                serde_json::from_slice(&out.stdout).context("parsing gh api response")?;
            let title = json["title"].as_str().unwrap_or("GitHub Issue").to_string();
            let body = json["body"].as_str().unwrap_or("").to_string();
            return Ok((title, body));
        }
    }

    // Optional user hook: `memory add --from-url` can shell out to a local
    // Markdown-conversion script under `bun` for higher-fidelity extraction
    // than the naive HTML strip below. This is opt-in and guarded: the script
    // must live at a fixed, spelunk-owned path
    // (`~/.config/spelunk/scripts/web-to-md.ts`), *not* anywhere under the
    // home directory. Prior to this guard the CLI ran `~/scripts/web-to-md.ts`
    // whenever it happened to exist — a surprising, undocumented dependency
    // that made any attacker-writable home-dir script (e.g. via a prior
    // unrelated compromise, or a shared/managed machine) an implicit
    // code-execution path every time `memory add --from-url` ran. Scoping the
    // path to `~/.config/spelunk/` narrows this to a location the user
    // explicitly manages for spelunk and documents the mechanism in one place.
    // See docs/memory.md#web-to-md-hook.
    let script = web_to_md_script_path().filter(|p| p.exists());

    if let Some(script_path) = script {
        let out = tokio::process::Command::new("bun")
            .arg(&script_path)
            .arg(url)
            .output()
            .await;
        if let Ok(out) = out
            && out.status.success()
        {
            let md = String::from_utf8_lossy(&out.stdout);
            return parse_web_to_md_output(&md, url);
        }
    }

    // Fall back to a basic HTTP fetch using the reqwest client from
    // ServerInferenceClient's underlying connection (we build a fresh one here
    // since we have no server config at this call site).
    let http = reqwest::Client::builder()
        .user_agent(concat!("spelunk/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let html = http.get(url).send().await?.text().await?;

    let title_re = regex::Regex::new(r"(?i)<title[^>]*>([\s\S]*?)</title>").unwrap();
    let title = title_re
        .captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| html_unescape(m.as_str().trim()))
        .unwrap_or_else(|| url.to_string());

    let no_script =
        regex::Regex::new(r"(?is)<(?:script|style)[^>]*>[\s\S]*?</(?:script|style)>").unwrap();
    let no_tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let ws = regex::Regex::new(r"\s{3,}").unwrap();
    let stripped = no_script.replace_all(&html, " ");
    let stripped = no_tags.replace_all(&stripped, " ");
    let body = ws.replace_all(stripped.trim(), "\n\n").to_string();
    let body = if body.len() > 8192 {
        body[..8192].to_string()
    } else {
        body
    };

    Ok((title, body))
}

/// The fixed, spelunk-owned path the web-to-md hook script must live at to be
/// picked up (`~/.config/spelunk/scripts/web-to-md.ts`). Deliberately does
/// *not* consider the old `~/scripts/web-to-md.ts` location — see the
/// opt-in-guard comment above this function's call site.
///
/// `SPELUNK_SCRIPTS_DIR` overrides the `~/.config/spelunk/scripts` directory
/// wholesale. Useful in tests and on Windows CI, where `dirs::home_dir()`
/// (v6) calls `SHGetKnownFolderPath` rather than reading `HOME`/`USERPROFILE`,
/// making per-process environment overrides of `HOME` ineffective — see the
/// identical note on `spelunk_state_dir` in `capability.rs`.
fn web_to_md_script_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("SPELUNK_SCRIPTS_DIR") {
        return Some(std::path::PathBuf::from(dir).join("web-to-md.ts"));
    }
    dirs::home_dir().map(|h| {
        h.join(".config")
            .join("spelunk")
            .join("scripts")
            .join("web-to-md.ts")
    })
}

fn parse_web_to_md_output(md: &str, url: &str) -> Result<(String, String)> {
    let md = md.trim();
    if let Some(rest) = md.strip_prefix("# ") {
        let (title_line, body) = rest.split_once('\n').unwrap_or((rest, ""));
        Ok((title_line.trim().to_string(), body.trim_start().to_string()))
    } else {
        Ok((url.to_string(), md.to_string()))
    }
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Try to embed `text` via spelunk-server.
///
/// Returns `None` (with a log warning) if the server is not configured or
/// unreachable, so that callers can store entries without embeddings rather
/// than failing outright. Semantic search will not surface unembedded entries.
async fn try_embed_via_server(cfg: &Config, text: &str) -> Option<Vec<u8>> {
    use crate::embeddings::vec_to_blob;
    let Some(client) = ServerInferenceClient::from_config(cfg) else {
        tracing::warn!(
            "No server_url configured — memory entry stored without embedding vector; \
             semantic search will not surface it."
        );
        return None;
    };
    let sp = super::super::ui::spinner("Embedding…");
    let result: anyhow::Result<Vec<u8>> = async {
        let vec = client
            .embed_text(text)
            .await
            .context("embedding memory entry")?;
        Ok(vec_to_blob(&vec))
    }
    .await;
    sp.finish_and_clear();
    match result {
        Ok(blob) => Some(blob),
        Err(e) => {
            tracing::warn!(
                "Server embedding failed — entry stored without vector; \
                 semantic search will not surface it. ({e})"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    /// Run `f` with `SPELUNK_SCRIPTS_DIR` pointed at `dir` for the duration of
    /// the call. `#[serial]` on each test guards the shared process-wide env
    /// var. Deliberately overrides `SPELUNK_SCRIPTS_DIR` rather than `HOME` —
    /// `dirs::home_dir()` (v6) doesn't read `HOME` on Windows (it calls
    /// `SHGetKnownFolderPath`), so a `HOME`-only override is silently
    /// ineffective there. See the identical note on `web_to_md_script_path`.
    fn with_scripts_dir<F: FnOnce()>(dir: &std::path::Path, f: F) {
        let prev = std::env::var_os("SPELUNK_SCRIPTS_DIR");
        // SAFETY: guarded by #[serial] — no other thread in this test binary
        // reads/writes SPELUNK_SCRIPTS_DIR concurrently.
        unsafe { std::env::set_var("SPELUNK_SCRIPTS_DIR", dir) };
        f();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("SPELUNK_SCRIPTS_DIR", v),
                None => std::env::remove_var("SPELUNK_SCRIPTS_DIR"),
            }
        }
    }

    /// `web_to_md_script_path` must resolve to the new, spelunk-owned path
    /// (`~/.config/spelunk/scripts/web-to-md.ts`, or `SPELUNK_SCRIPTS_DIR` if set).
    #[test]
    #[serial]
    fn web_to_md_script_path_is_config_spelunk_scripts() {
        let tmp = TempDir::new().unwrap();
        with_scripts_dir(tmp.path(), || {
            let path = web_to_md_script_path().expect("SPELUNK_SCRIPTS_DIR is set");
            assert_eq!(path, tmp.path().join("web-to-md.ts"));
        });
    }

    /// Regression guard for the opt-in fix: a script left
    /// at the *old*, unguarded location (`~/scripts/web-to-md.ts`) must NOT be
    /// picked up any more — only the fixed `~/.config/spelunk/scripts/` path
    /// counts. Prior to the fix, any attacker-writable home-dir script at the
    /// old path was an implicit code-execution path on every `memory add
    /// --from-url` call; this test ensures that door stays shut.
    #[test]
    #[serial]
    fn old_home_scripts_path_is_not_used() {
        let tmp = TempDir::new().unwrap();
        let old_dir = tmp.path().join("scripts");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("web-to-md.ts"), b"// legacy script").unwrap();

        let new_dir = tmp.path().join("new-scripts");
        with_scripts_dir(&new_dir, || {
            let path = web_to_md_script_path().expect("SPELUNK_SCRIPTS_DIR is set");
            assert_ne!(
                path,
                old_dir.join("web-to-md.ts"),
                "resolved script path must not be the old ~/scripts/web-to-md.ts location"
            );
            assert!(
                !path.exists(),
                "a script only present at the old location must not be found at the \
                 resolved (new) path — the old path must be silently ignored, not \
                 still honoured"
            );
        });
    }

    /// Positive case: a script placed at the new, guarded location IS found.
    #[test]
    #[serial]
    fn new_config_spelunk_scripts_path_is_used_when_present() {
        let tmp = TempDir::new().unwrap();
        let new_dir = tmp.path().join(".config").join("spelunk").join("scripts");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("web-to-md.ts"), b"// new script").unwrap();

        with_scripts_dir(&new_dir, || {
            let path = web_to_md_script_path().expect("SPELUNK_SCRIPTS_DIR is set");
            assert!(
                path.exists(),
                "script placed at the new opt-in path should be found"
            );
        });
    }

    #[test]
    fn parse_web_to_md_output_extracts_title_from_heading() {
        let md = "# My Title\n\nSome body text.";
        let (title, body) = parse_web_to_md_output(md, "https://example.com").unwrap();
        assert_eq!(title, "My Title");
        assert_eq!(body, "Some body text.");
    }

    #[test]
    fn parse_web_to_md_output_falls_back_to_url_without_heading() {
        let md = "No heading here, just body text.";
        let (title, body) = parse_web_to_md_output(md, "https://example.com/page").unwrap();
        assert_eq!(title, "https://example.com/page");
        assert_eq!(body, "No heading here, just body text.");
    }
}
