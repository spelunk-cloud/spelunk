use anyhow::{Context, Result};

use super::{MemoryHarvestArgs, backend_err};
use crate::{
    capability,
    config::Config,
    embeddings::vec_to_blob,
    server_client::{LlmMessage, ServerInferenceClient, harvest_requires_server},
    storage::{NoteInput, open_memory_backend},
};

/// Reject a `--branch` / `--git-range` value that could be parsed by `git log`
/// as an option rather than a revision (argument-injection / option-injection
/// guard).
///
/// Every callsite in this file already appends a `--` separator before the
/// pathspec position, which stops git from treating the ref as an option to
/// `git log` itself; this check is defense-in-depth for revision walkers
/// (like `A..B` ranges) where a leading `-` component can still be
/// misinterpreted, and it gives a clear, immediate error instead of relying
/// solely on the separator.
fn reject_option_like_ref(git_ref: &str) -> Result<()> {
    let is_option_like = |s: &str| s.starts_with('-') && s != "-";
    let offending = git_ref
        .split("..")
        .find(|part| is_option_like(part))
        .or_else(|| is_option_like(git_ref).then_some(git_ref));

    if let Some(bad) = offending {
        anyhow::bail!(
            "Invalid --branch/--git-range value '{bad}': refs beginning with '-' are rejected \
             to prevent them from being interpreted as git options."
        );
    }
    Ok(())
}

pub(super) async fn memory_harvest(
    args: MemoryHarvestArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Honor the auto-discovered server tier (IMP-3 / spelunk#316): loopback
    // auto-discovery sets the capability tier without populating
    // `cfg.server_url`. Build an effective config that fills in the inference
    // URL / `project_id` from the tier (mirrors `explore`) and use it for the
    // remainder of this call tree, including the git/failures/claude-code
    // sub-harvesters and `ServerInferenceClient::from_config`.
    //
    // ADR-004: harvest is an inference-driven command. It needs the server for
    // embeddings + LLM extraction (gate on the inference URL), but its memory
    // CRUD goes to the project's local `memory.db` via `open_memory_backend`
    // (which reads only `server_url`). For an auto-discovered server that means
    // local storage; for an explicit team `server_url` memory stays remote.
    let project_root = mem_path.parent().unwrap_or(mem_path);
    let tier = capability::get_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    let cfg = &eff_cfg;

    // Tier-0: harvest requires server inference (#259 locked-feature error).
    // Guidance points at the local auto-server, never team `server_url` setup.
    if cfg.resolve_inference_url().is_none() {
        return Err(harvest_requires_server());
    }

    if args.detach {
        super::super::helpers::spawn_detached()?;
        return Ok(());
    }

    match args.source.as_str() {
        "git" => memory_harvest_git(args, mem_path, cfg, backend_override).await,
        "claude-code" => {
            super::harvest_claude::harvest_claude_code(args, mem_path, cfg, backend_override).await
        }
        "failures" => memory_harvest_failures(args, mem_path, cfg, backend_override).await,
        other => {
            anyhow::bail!("Unknown --source '{other}'. Valid values: git, claude-code, failures")
        }
    }
}

async fn memory_harvest_git(
    args: MemoryHarvestArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let (git_ref, range_label) = match &args.branch {
        Some(branch) => (branch.clone(), format!("full history of '{branch}'")),
        None => (args.git_range.clone(), format!("'{}'", args.git_range)),
    };

    reject_option_like_ref(&git_ref)?;

    let git_out = std::process::Command::new("git")
        .args(["log", &git_ref, "--format=%H%x00%s%x00%b%x00---", "--"])
        .output()
        .context("running git log (is git installed and are we in a git repo?)")?;

    if !git_out.status.success() {
        let msg = String::from_utf8_lossy(&git_out.stderr);
        anyhow::bail!("git log failed: {msg}");
    }

    let raw = String::from_utf8(git_out.stdout).context("git log output not UTF-8")?;
    let commits: Vec<(String, String, String)> = raw
        .split("---\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|entry| {
            let parts: Vec<&str> = entry.splitn(4, '\x00').collect();
            if parts.len() < 3 {
                return None;
            }
            Some((
                parts[0].trim().to_string(),
                parts[1].trim().to_string(),
                parts[2].trim().to_string(),
            ))
        })
        .collect();

    if commits.is_empty() {
        println!("No commits found in {range_label}.");
        return Ok(());
    }

    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let known_shas = backend.harvested_shas().await.map_err(backend_err)?;
    let new_commits: Vec<_> = commits
        .iter()
        .filter(|(sha, _, _)| !known_shas.contains(sha.as_str()))
        .collect();

    if new_commits.is_empty() {
        println!("All {} commits already harvested.", commits.len());
        return Ok(());
    }

    let (new_commits, pre_filtered): (Vec<_>, Vec<_>) = new_commits
        .into_iter()
        .partition(|(_, subject, _)| !is_routine_subject(subject));

    if !pre_filtered.is_empty() {
        println!(
            "Pre-filtered {} routine commit(s) (formatting, merges, etc.).",
            pre_filtered.len()
        );
    }

    if new_commits.is_empty() {
        println!("No commits worth analysing in {range_label}.");
        return Ok(());
    }

    let batch_size = args.batch_size.max(1);
    let total = new_commits.len();
    let num_batches = total.div_ceil(batch_size);
    println!(
        "Analysing {} new commit(s) in '{}' ({} batch(es) of up to {})…",
        total, range_label, num_batches, batch_size
    );

    let system = "You help build a project memory store from git history. \
        Respond ONLY with valid JSON matching the provided schema. No other text.";

    let schema = serde_json::json!({
        "name": "harvest_result",
        "strict": true,
        "schema": {
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "sha": {"type": "string"},
                            "kind": {"type": "string", "enum": ["decision","context","requirement","note"]},
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "tags": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["sha","kind","title","body","tags"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["entries"],
            "additionalProperties": false
        }
    });

    let server = ServerInferenceClient::from_config(cfg).ok_or_else(harvest_requires_server)?;

    let mut stored = 0usize;
    let mut dedup_skipped = 0usize;
    const DEDUP_THRESHOLD: f64 = 0.15;

    let estimate_tokens = |s: &str| s.len() / 3;
    let context_length = cfg.llm_context_length;
    let output_budget = |n: usize| (n * 400).clamp(256, context_length / 2);

    let mut work: std::collections::VecDeque<Vec<(String, String, String)>> = new_commits
        .chunks(batch_size)
        .map(|c| {
            c.iter()
                .map(|(a, b, c)| (a.clone(), b.clone(), c.clone()))
                .collect()
        })
        .collect();

    let mut batch_num = 0usize;

    while let Some(batch) = work.pop_front() {
        batch_num += 1;

        let max_body = if batch.len() == 1 {
            let overhead = estimate_tokens(system) + 600;
            let available_chars = context_length.saturating_sub(overhead) * 3;
            available_chars.clamp(120, 400)
        } else {
            400
        };

        let commit_list = batch
            .iter()
            .map(|(sha, subject, body)| {
                if body.is_empty() {
                    format!("COMMIT {sha}\n{subject}")
                } else {
                    let boundary = body.floor_char_boundary(max_body);
                    let trimmed_body = &body[..boundary];
                    format!("COMMIT {sha}\n{subject}\n\n{trimmed_body}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let user = format!(
            "Review these git commit messages. Identify commits that represent:\n\
             - \"decision\": A significant architectural or design choice and reasoning\n\
             - \"context\": Background about requirements, constraints, or project goals\n\
             - \"requirement\": A hard constraint the codebase must satisfy\n\
             - \"note\": A surprising or non-obvious discovery\n\n\
             SKIP — return NO entry for:\n\
             - Formatting/linting: \"ran prettier\", \"cargo fmt\", \"apply eslint\", \
               \"gofmt\", \"fix whitespace\", \"code style\", \"apply linting\"\n\
             - Version/release: \"bump version\", \"release v1.2.3\", \"update changelog\"\n\
             - Merge commits: subjects starting with \"Merge branch\" or \"Merge pull request\"\n\
             - Trivial fixes: typos, comment wording, variable renames with no design significance\n\
             - Dependency bumps that reveal no architectural constraint\n\n\
             Only create an entry if the commit reveals WHY something was designed a certain way, \
             establishes a hard constraint, or captures non-obvious knowledge a future developer needs.\n\n\
             For each significant commit write: sha (first 8 chars), kind, title \
             (one sentence, past tense for decisions), body (include why, \
             what alternatives were considered), tags (2-4 keywords).\n\n\
             Commits:\n{commit_list}"
        );

        let input_tokens = estimate_tokens(system) + estimate_tokens(&user);
        let out_budget = output_budget(batch.len());

        if input_tokens + out_budget > context_length && batch.len() > 1 {
            println!(
                "\n  Batch {} too large (~{} input + {} output > {} token context), splitting…",
                batch_num, input_tokens, out_budget, context_length
            );
            batch_num -= 1;
            let mid = batch.len() / 2;
            work.push_front(batch[mid..].to_vec());
            work.push_front(batch[..mid].to_vec());
            continue;
        }

        let max_tokens = context_length
            .saturating_sub(input_tokens)
            .min(out_budget)
            .max(128);

        if num_batches > 1 || work.front().is_some() {
            println!("\nBatch {} ({} commits)…", batch_num, batch.len());
        }

        let messages = vec![LlmMessage::system(system), LlmMessage::user(user)];

        let raw_json = match server
            .llm_complete(&messages, max_tokens, Some(schema.clone()))
            .await
        {
            Ok(raw) => crate::utils::strip_ansi(&raw),
            Err(e) => {
                eprintln!(
                    "  warning: LLM call failed for batch {batch_num} ({} commit(s)), skipping: {e:#}",
                    batch.len()
                );
                continue;
            }
        };

        let parsed: serde_json::Value = match serde_json::from_str(&raw_json) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  warning: could not parse LLM response for batch {batch_num} ({} commit(s)), skipping: {e}\n  Raw: {}",
                    batch.len(),
                    &raw_json[..raw_json.len().min(200)]
                );
                continue;
            }
        };

        let entries = parsed["entries"].as_array().cloned().unwrap_or_default();

        if entries.is_empty() {
            println!("  No significant commits in this batch.");
            continue;
        }

        println!("Embedding {} entries…", entries.len());
        for entry in &entries {
            let sha_short = entry["sha"].as_str().unwrap_or("").to_string();
            let kind = entry["kind"].as_str().unwrap_or("note");
            let title = entry["title"].as_str().unwrap_or("").to_string();
            let body = entry["body"].as_str().unwrap_or("").to_string();
            let tags: Vec<String> = entry["tags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let full_sha = batch
                .iter()
                .find(|(s, _, _)| s.starts_with(&sha_short))
                .map(|(s, _, _)| s.clone())
                .unwrap_or(sha_short.clone());

            match backend.has_source_ref(&full_sha).await.map_err(backend_err) {
                Ok(true) => {
                    println!("  [skip] already harvested {full_sha}");
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("  warning: could not check source_ref for {full_sha}: {e:#}");
                    continue;
                }
            }

            let embed_text = format!("title: {title} | text: {body}");
            let vec = match server.embed_text(&embed_text).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "  warning: embedding failed for entry '{title}' ({full_sha}), skipping: {e:#}"
                    );
                    continue;
                }
            };
            let blob = vec_to_blob(&vec);

            let neighbors = match backend.search(&blob, &embed_text, 1, None).await {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("  warning: dedup search failed for '{title}', skipping: {e:#}");
                    continue;
                }
            };
            if let Some(top) = neighbors.first()
                && top.distance.unwrap_or(1.0) < DEDUP_THRESHOLD
            {
                println!(
                    "  [dedup] '{}' too similar to #{} '{}' (dist={:.3})",
                    title,
                    top.id,
                    top.title,
                    top.distance.unwrap_or(0.0)
                );
                dedup_skipped += 1;
                continue;
            }

            let note_id = match backend
                .add(NoteInput {
                    kind: kind.to_string(),
                    title: title.clone(),
                    body: body.clone(),
                    tags: tags.clone(),
                    linked_files: vec![],
                    embedding: Some(blob),
                    source_ref: Some(full_sha.clone()),
                    valid_at: None,
                    supersedes: None,
                })
                .await
            {
                Ok((id, _created)) => id,
                Err(e) => {
                    eprintln!(
                        "  warning: failed to store entry '{title}' ({full_sha}), skipping: {e:#}"
                    );
                    continue;
                }
            };

            let short_sha = &full_sha[..full_sha.len().min(8)];
            println!("  + [{kind}] #{note_id}: {title}  \x1b[2m({short_sha})\x1b[0m");
            stored += 1;
        }
    }

    let llm_skipped = new_commits.len().saturating_sub(stored + dedup_skipped);
    println!(
        "\nStored {stored} memory entries. Skipped {} routine (pre-filter), {} by LLM, {} near-duplicate.",
        pre_filtered.len(),
        llm_skipped,
        dedup_skipped
    );
    Ok(())
}

/// Returns true for commit subjects that are obviously routine.
fn is_routine_subject(subject: &str) -> bool {
    let s = subject.trim().to_lowercase();

    let fmt_tools = [
        "prettier",
        "eslint",
        "gofmt",
        "cargo fmt",
        "rustfmt",
        "black",
        "isort",
        "rubocop",
        "stylelint",
        "clang-format",
        "yapf",
        "autopep8",
        "swiftformat",
        "ktlint",
    ];
    if fmt_tools.iter().any(|t| s.contains(t)) {
        return true;
    }

    let patterns = [
        "format code",
        "formatting",
        "fix whitespace",
        "whitespace",
        "trailing whitespace",
        "lint fix",
        "ran linter",
        "apply linting",
        "merge branch ",
        "merge pull request",
        "merge remote-tracking",
        "bump version",
        "version bump",
        "release v",
        "chore: release",
        "update changelog",
        "update lock",
        "cargo.lock",
    ];
    patterns.iter().any(|p| s.contains(p))
}

/// Harvest antipatterns from failure-signal commits (reverts, bug fixes, regressions).
async fn memory_harvest_failures(
    args: MemoryHarvestArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let git_ref = match &args.branch {
        Some(branch) => branch.clone(),
        None => args.git_range.clone(),
    };
    let range_label = match &args.branch {
        Some(branch) => format!("full history of '{branch}'"),
        None => format!("'{git_ref}'"),
    };

    reject_option_like_ref(&git_ref)?;

    let git_out = std::process::Command::new("git")
        .args(["log", &git_ref, "--format=%H%x00%s%x00%b%x00---", "--"])
        .output()
        .context("running git log")?;
    if !git_out.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&git_out.stderr)
        );
    }

    let raw = String::from_utf8(git_out.stdout).context("git log output not UTF-8")?;
    let all_commits: Vec<(String, String, String)> = raw
        .split("---\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|entry| {
            let parts: Vec<&str> = entry.splitn(4, '\x00').collect();
            if parts.len() < 3 {
                return None;
            }
            Some((
                parts[0].trim().to_string(),
                parts[1].trim().to_string(),
                parts[2].trim().to_string(),
            ))
        })
        .collect();

    // Keep only failure-signal commits.
    let failure_commits: Vec<_> = all_commits
        .iter()
        .filter(|(_, subject, _)| is_failure_subject(subject))
        .collect();

    if failure_commits.is_empty() {
        println!("No failure-signal commits found in {range_label}.");
        println!("(Looking for reverts, bug fixes, regressions, crashes, etc.)");
        return Ok(());
    }

    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let known_shas = backend.harvested_shas().await.map_err(backend_err)?;
    let new_commits: Vec<_> = failure_commits
        .into_iter()
        .filter(|(sha, _, _)| !known_shas.contains(sha.as_str()))
        .collect();

    if new_commits.is_empty() {
        println!("All failure-signal commits already harvested.");
        return Ok(());
    }

    let batch_size = args.batch_size.max(1);
    let total = new_commits.len();
    let num_batches = total.div_ceil(batch_size);
    println!(
        "Analysing {} failure-signal commit(s) in {} ({} batch(es) of up to {})…",
        total, range_label, num_batches, batch_size
    );

    let system = "You help build a project memory store from git history. \
        Respond ONLY with valid JSON matching the provided schema. No other text.";

    let schema = serde_json::json!({
        "name": "harvest_result",
        "strict": true,
        "schema": {
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "sha": {"type": "string"},
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "tags": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["sha", "title", "body", "tags"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["entries"],
            "additionalProperties": false
        }
    });

    let server = ServerInferenceClient::from_config(cfg).ok_or_else(harvest_requires_server)?;

    let mut stored = 0usize;
    let mut dedup_skipped = 0usize;
    const DEDUP_THRESHOLD: f64 = 0.15;

    let estimate_tokens = |s: &str| s.len() / 3;
    let context_length = cfg.llm_context_length;

    let mut work: std::collections::VecDeque<Vec<(String, String, String)>> = new_commits
        .chunks(batch_size)
        .map(|c| {
            c.iter()
                .map(|(a, b, c)| (a.clone(), b.clone(), c.clone()))
                .collect()
        })
        .collect();

    let mut batch_num = 0usize;

    while let Some(batch) = work.pop_front() {
        batch_num += 1;

        let commit_list = batch
            .iter()
            .map(|(sha, subject, body)| {
                if body.is_empty() {
                    format!("COMMIT {sha}\n{subject}")
                } else {
                    let boundary = body.floor_char_boundary(400);
                    format!("COMMIT {sha}\n{subject}\n\n{}", &body[..boundary])
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let user = format!(
            "Review these failure-signal git commits (reverts, bug fixes, regressions, crashes).\n\
             For EACH commit, extract an antipattern rule: what went wrong and what should be \
             avoided in future.\n\n\
             Write each entry as:\n\
             - title: a short imperative rule starting with \"Never\", \"Avoid\", or \"Don't\" \
               (e.g. \"Never call block_on inside an async context\")\n\
             - body: explain what was wrong, what broke, and what to do instead. Include \
               context so a future developer understands the failure mode.\n\
             - tags: 2-4 keywords\n\n\
             SKIP a commit if it is too vague to yield a useful rule (e.g. \"fix typo\").\n\
             Return one entry per meaningful failure pattern found.\n\n\
             Commits:\n{commit_list}"
        );

        let input_tokens = estimate_tokens(system) + estimate_tokens(&user);
        let out_budget = (batch.len() * 400).clamp(256, context_length / 2);

        if input_tokens + out_budget > context_length && batch.len() > 1 {
            batch_num -= 1;
            let mid = batch.len() / 2;
            work.push_front(batch[mid..].to_vec());
            work.push_front(batch[..mid].to_vec());
            continue;
        }

        let max_tokens = context_length
            .saturating_sub(input_tokens)
            .min(out_budget)
            .max(128);

        if num_batches > 1 || work.front().is_some() {
            println!("\nBatch {} ({} commits)…", batch_num, batch.len());
        }

        let messages = vec![LlmMessage::system(system), LlmMessage::user(user)];

        let raw_json = match server
            .llm_complete(&messages, max_tokens, Some(schema.clone()))
            .await
        {
            Ok(raw) => crate::utils::strip_ansi(&raw),
            Err(e) => {
                eprintln!(
                    "  warning: LLM call failed for batch {batch_num} ({} commit(s)), skipping: {e:#}",
                    batch.len()
                );
                continue;
            }
        };

        let parsed: serde_json::Value = match serde_json::from_str(&raw_json) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  warning: could not parse LLM response for batch {batch_num} ({} commit(s)), skipping: {e}\n  Raw: {}",
                    batch.len(),
                    &raw_json[..raw_json.len().min(200)]
                );
                continue;
            }
        };

        let entries = parsed["entries"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            println!("  No actionable antipatterns in this batch.");
            continue;
        }

        println!("Embedding {} entries…", entries.len());
        for entry in &entries {
            let sha_short = entry["sha"].as_str().unwrap_or("").to_string();
            let title = entry["title"].as_str().unwrap_or("").to_string();
            let body = entry["body"].as_str().unwrap_or("").to_string();
            let tags: Vec<String> = entry["tags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let full_sha = batch
                .iter()
                .find(|(s, _, _)| s.starts_with(&sha_short))
                .map(|(s, _, _)| s.clone())
                .unwrap_or(sha_short.clone());

            match backend.has_source_ref(&full_sha).await.map_err(backend_err) {
                Ok(true) => {
                    println!("  [skip] already harvested {full_sha}");
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("  warning: could not check source_ref for {full_sha}: {e:#}");
                    continue;
                }
            }

            let embed_text = format!("title: {title} | text: {body}");
            let vec = match server.embed_text(&embed_text).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "  warning: embedding failed for entry '{title}' ({full_sha}), skipping: {e:#}"
                    );
                    continue;
                }
            };
            let blob = vec_to_blob(&vec);

            let neighbors = match backend.search(&blob, &embed_text, 1, None).await {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("  warning: dedup search failed for '{title}', skipping: {e:#}");
                    continue;
                }
            };
            if let Some(top) = neighbors.first()
                && top.distance.unwrap_or(1.0) < DEDUP_THRESHOLD
            {
                println!(
                    "  [dedup] '{}' too similar to #{} '{}' (dist={:.3})",
                    title,
                    top.id,
                    top.title,
                    top.distance.unwrap_or(0.0)
                );
                dedup_skipped += 1;
                continue;
            }

            let note_id = match backend
                .add(NoteInput {
                    kind: "antipattern".to_string(),
                    title: title.clone(),
                    body: body.clone(),
                    tags,
                    linked_files: vec![],
                    embedding: Some(blob),
                    source_ref: Some(full_sha.clone()),
                    valid_at: None,
                    supersedes: None,
                })
                .await
            {
                Ok((id, _created)) => id,
                Err(e) => {
                    eprintln!(
                        "  warning: failed to store entry '{title}' ({full_sha}), skipping: {e:#}"
                    );
                    continue;
                }
            };

            let short_sha = &full_sha[..full_sha.len().min(8)];
            println!("  + [antipattern] #{note_id}: {title}  \x1b[2m({short_sha})\x1b[0m");
            stored += 1;
        }
    }

    println!("\nStored {stored} antipattern(s). Skipped {dedup_skipped} near-duplicate.");
    Ok(())
}

/// Returns true for commits that signal a failure (revert, bug fix, regression, crash, etc.).
fn is_failure_subject(subject: &str) -> bool {
    let s = subject.trim().to_lowercase();
    if s.starts_with("revert \"") || s.starts_with("revert: ") || s.starts_with("revert ") {
        return true;
    }
    let failure_signals = [
        "fix(",
        "fix!(",
        "fix: ",
        "fix!: ",
        "bug: ",
        "bug(",
        "bugfix",
        "hotfix",
        "regression",
        "crash",
        "broken",
        "broke",
        "incorrect",
        "wrong ",
        "oops",
        "mistake",
        "error: ",
        "panic",
        "deadlock",
        "memory leak",
        "data loss",
        "data corruption",
        "infinite loop",
        "stack overflow",
    ];
    failure_signals.iter().any(|p| s.contains(p))
}

#[cfg(test)]
mod option_injection_guard_tests {
    use super::reject_option_like_ref;

    #[test]
    fn accepts_ordinary_refs_and_ranges() {
        assert!(reject_option_like_ref("main").is_ok());
        assert!(reject_option_like_ref("HEAD~10..HEAD").is_ok());
        assert!(reject_option_like_ref("feature/foo..main").is_ok());
        assert!(reject_option_like_ref("-").is_ok());
    }

    #[test]
    fn rejects_option_like_branch() {
        let err = reject_option_like_ref("--output=/tmp/x").unwrap_err();
        assert!(err.to_string().contains("--output=/tmp/x"));
    }

    #[test]
    fn rejects_option_like_range_endpoint() {
        assert!(reject_option_like_ref("--output=/tmp/x..HEAD").is_err());
        assert!(reject_option_like_ref("HEAD..--output=/tmp/x").is_err());
    }

    /// A ref that is exactly the `--` end-of-options marker. It starts with
    /// `-` and is not the single-char `-` literal, so it is rejected like any
    /// other option-like value — it must not be special-cased into an accept,
    /// since `git log -- --` is ambiguous/nonsensical as a revision anyway.
    #[test]
    fn rejects_bare_double_dash() {
        assert!(reject_option_like_ref("--").is_err());
    }

    /// Short numeric-looking options (`git log -1`, `-n5`, etc.) must be
    /// rejected too, not just long `--flag=value` forms — the guard checks
    /// only the leading `-`, so this should already hold, but it's worth
    /// pinning explicitly since `-1`/`-n` are among the most common ways to
    /// accidentally (or maliciously) alter `git log`'s behavior.
    #[test]
    fn rejects_short_option_like_refs() {
        assert!(reject_option_like_ref("-1").is_err());
        assert!(reject_option_like_ref("-n5").is_err());
        assert!(reject_option_like_ref("-p").is_err());
    }

    /// A very long option-like value must still be rejected (no length-based
    /// bypass / no truncation before the check).
    #[test]
    fn rejects_long_option_like_ref() {
        let long_val = format!("--output={}", "a".repeat(4096));
        assert!(reject_option_like_ref(&long_val).is_err());
    }

    /// Refs containing shell metacharacters are not shell-parsed anywhere in
    /// this codebase (git is always spawned via argv, never a shell), so
    /// these are harmless from a shell-injection standpoint — but they must
    /// still pass straight through unless they are *also* option-like
    /// (leading `-`), proving the guard is narrowly scoped to option-shape
    /// and doesn't accidentally reject or mangle legitimate-looking (if
    /// unusual) ref/range strings.
    #[test]
    fn shell_metacharacters_alone_do_not_trigger_rejection() {
        assert!(reject_option_like_ref("feature/$(whoami)").is_ok());
        assert!(reject_option_like_ref("a;b|c&d").is_ok());
        assert!(reject_option_like_ref("main..feature`x`").is_ok());
    }

    /// Combining a leading dash with shell metacharacters must still be
    /// rejected via the option-like check (belt-and-braces: even though argv
    /// spawning means these can't reach a shell, the leading `-` alone is
    /// sufficient grounds for rejection).
    #[test]
    fn rejects_option_like_ref_with_shell_metacharacters() {
        assert!(reject_option_like_ref("--output=/tmp/x;rm -rf /").is_err());
        assert!(reject_option_like_ref("-$(whoami)").is_err());
    }
}
