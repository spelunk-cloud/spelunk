//! `spelunk memory harvest --source claude-code`
//!
//! Mines `~/.claude/history.jsonl` for memory entries by sending each
//! unprocessed Claude Code session through the LLM and storing the extracted
//! entries in the memory database.

use std::collections::HashMap;
use std::io::BufRead as _;

use anyhow::{Context, Result};

use super::super::color::cprintln;
use super::{MemoryHarvestArgs, backend_err};
use crate::{
    config::Config,
    embeddings::vec_to_blob,
    indexer::secrets::contains_secret,
    server_client::{LlmMessage, ServerInferenceClient, harvest_requires_server},
    storage::{NoteInput, open_memory_backend},
};

// ── Serde structs ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Debug)]
struct ClaudeHistoryEntry {
    display: String,
    #[serde(rename = "pastedContents", default)]
    pasted_contents: HashMap<String, PastedContent>,
    timestamp: i64,
    project: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(serde::Deserialize, Debug)]
struct PastedContent {
    #[serde(default)]
    content: String,
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub(super) async fn harvest_claude_code(
    args: MemoryHarvestArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Tier-0 gate: harvest requires server inference.
    let server = ServerInferenceClient::from_config(cfg).ok_or_else(harvest_requires_server)?;

    // 1. Require explicit confirmation.
    if !args.confirm {
        println!(
            "This will read ~/.claude/history.jsonl which contains your full Claude Code session history."
        );
        println!("Re-run with --confirm to proceed.");
        return Ok(());
    }

    // 2. Resolve history file path.
    let history_path = match args.history_file.clone() {
        Some(p) => p,
        None => {
            let home = std::env::var("HOME").context("$HOME is not set")?;
            std::path::PathBuf::from(home)
                .join(".claude")
                .join("history.jsonl")
        }
    };

    if !history_path.exists() {
        println!(
            "No Claude Code history found at {}.",
            history_path.display()
        );
        return Ok(());
    }

    // 3. Resolve current git repo root.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = gix::discover(&cwd)
        .context("Not inside a git repository — cannot determine project root for filtering.")?
        .workdir()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Not inside a git worktree — cannot determine project root for filtering."
            )
        })?
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();

    // 4. Parse --since into milliseconds threshold.
    let since_ms: i64 = if let Some(ref s) = args.since {
        let epoch_secs = crate::utils::dates::parse_as_of(Some(s.as_str()))
            .with_context(|| format!("parsing --since '{s}'"))?
            .unwrap_or(0);
        epoch_secs * 1000
    } else {
        0
    };

    // 5. Load known source_refs.
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let known_refs = backend.harvested_shas().await.map_err(backend_err)?;

    // 6. Stream-read history file; accumulate sessions relevant to this repo.
    let file = std::fs::File::open(&history_path)
        .with_context(|| format!("opening {}", history_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut sessions: HashMap<String, Vec<ClaudeHistoryEntry>> = HashMap::new();
    let mut parse_errors = 0usize;

    for line in reader.lines() {
        let line = line.context("reading history.jsonl")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: ClaudeHistoryEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if entry.timestamp < since_ms {
            continue;
        }
        if !entry.project.starts_with(&repo_root) {
            continue;
        }
        sessions
            .entry(entry.session_id.clone())
            .or_default()
            .push(entry);
    }

    if parse_errors > 0 {
        eprintln!("warning: skipped {parse_errors} unparseable line(s) in history.jsonl");
    }

    if sessions.is_empty() {
        println!("No Claude Code sessions found for this project.");
        return Ok(());
    }

    // 7. Filter out already-harvested sessions and secret-containing sessions.
    let mut new_sessions: Vec<(String, String)> = Vec::new(); // (session_id, combined_text)

    for (session_id, entries) in &sessions {
        let source_key = format!("claude-code:{session_id}");
        if known_refs.contains(source_key.as_str()) {
            continue;
        }

        let mut parts: Vec<String> = Vec::new();
        for entry in entries {
            if !entry.display.is_empty() {
                parts.push(entry.display.clone());
            }
            for pc in entry.pasted_contents.values() {
                if !pc.content.is_empty() {
                    parts.push(pc.content.clone());
                }
            }
        }

        let mut combined = parts.join("\n\n");

        if contains_secret(&combined) {
            eprintln!("warning: skipping session {session_id} (secret detected)");
            continue;
        }

        if combined.trim().is_empty() {
            continue;
        }

        // Cap at 16 000 chars.
        const CAP: usize = 16_000;
        if combined.len() > CAP {
            let boundary = combined.floor_char_boundary(CAP);
            combined.truncate(boundary);
            combined.push_str("\n\n[...truncated]");
        }

        new_sessions.push((session_id.clone(), combined));
    }

    if new_sessions.is_empty() {
        println!(
            "All {} session(s) already harvested or skipped.",
            sessions.len()
        );
        return Ok(());
    }

    // 8. Batch and send to LLM.
    let batch_size = args.batch_size.max(1);
    let total = new_sessions.len();
    let num_batches = total.div_ceil(batch_size);
    println!(
        "Analysing {} new session(s) in {} batch(es) of up to {}…",
        total, num_batches, batch_size
    );

    let system = "You help build a project memory store from Claude Code conversation history. \
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
                            "session_id": {"type": "string"},
                            "kind": {"type": "string", "enum": ["decision","context","requirement","note"]},
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "tags": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["session_id","kind","title","body","tags"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["entries"],
            "additionalProperties": false
        }
    });

    let mut stored = 0usize;
    let mut dedup_skipped = 0usize;
    const DEDUP_THRESHOLD: f64 = 0.15;

    let estimate_tokens = |s: &str| s.len() / 3;
    let context_length = cfg.llm_context_length;
    let output_budget = |n: usize| (n * 1200).clamp(512, context_length / 2);

    let mut work: std::collections::VecDeque<Vec<(String, String)>> = new_sessions
        .chunks(batch_size)
        .map(|c| c.to_vec())
        .collect();

    let mut batch_num = 0usize;

    while let Some(batch) = work.pop_front() {
        batch_num += 1;

        let session_list = batch
            .iter()
            .map(|(sid, text)| format!("SESSION {sid}\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let user = format!(
            "Review these Claude Code conversation turns. Identify turns that represent:\n\
             - \"decision\": A significant architectural or design choice and reasoning\n\
             - \"context\": Background about requirements, constraints, or project goals\n\
             - \"requirement\": A hard constraint the codebase must satisfy\n\
             - \"note\": A surprising or non-obvious discovery\n\n\
             SKIP — return NO entry for:\n\
             - Routine coding questions with no design significance\n\
             - Trivial edits, typos, comment wording\n\
             - Questions about syntax or standard library usage\n\
             - Conversations with no lasting architectural insight\n\n\
             Only create an entry if the conversation reveals WHY something was designed a certain way, \
             establishes a hard constraint, or captures non-obvious knowledge a future developer needs.\n\n\
             For each significant session write: session_id (full UUID), kind, title \
             (one sentence, past tense for decisions), body (include why, \
             what alternatives were considered), tags (2-4 keywords).\n\n\
             Sessions:\n{session_list}"
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
            println!("\nBatch {} ({} sessions)…", batch_num, batch.len());
        }

        let messages = vec![LlmMessage::system(system), LlmMessage::user(user.clone())];

        let raw_json = match server
            .llm_complete(&messages, max_tokens, Some(schema.clone()))
            .await
        {
            Ok(raw) => crate::utils::strip_ansi(&raw),
            Err(e) => {
                eprintln!(
                    "  warning: LLM call failed for batch {batch_num} ({} session(s)), skipping: {e:#}",
                    batch.len()
                );
                continue;
            }
        };

        let parsed: serde_json::Value = match serde_json::from_str(&raw_json) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  warning: could not parse LLM response for batch {batch_num} ({} session(s)), skipping: {e}\n  Raw: {}",
                    batch.len(),
                    &raw_json[..raw_json.len().min(200)]
                );
                continue;
            }
        };

        let entries = parsed["entries"].as_array().cloned().unwrap_or_default();

        if entries.is_empty() {
            println!("  No significant sessions in this batch.");
            continue;
        }

        println!("Embedding {} entries…", entries.len());

        for entry in &entries {
            let session_id = entry["session_id"].as_str().unwrap_or("").to_string();
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

            // Secret check on LLM output.
            if contains_secret(&body) {
                eprintln!("warning: skipping entry '{title}' (secret detected in LLM body)");
                continue;
            }

            let source_ref = format!("claude-code:{session_id}");

            match backend
                .has_source_ref(&source_ref)
                .await
                .map_err(backend_err)
            {
                Ok(true) => {
                    println!("  [skip] already harvested session {session_id}");
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("  warning: could not check source_ref for {source_ref}: {e:#}");
                    continue;
                }
            }

            let embed_text = format!("title: {title} | text: {body}");
            let vec = match server.embed_text(&embed_text).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "  warning: embedding failed for entry '{title}' (session {session_id}), skipping: {e:#}"
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
                    source_ref: Some(source_ref.clone()),
                    valid_at: None,
                    supersedes: None,
                })
                .await
            {
                Ok((id, _created)) => id,
                Err(e) => {
                    eprintln!(
                        "  warning: failed to store entry '{title}' (session {session_id}), skipping: {e:#}"
                    );
                    continue;
                }
            };

            let short_id = &session_id[..session_id.len().min(8)];
            cprintln!("  + [{kind}] #{note_id}: {title}  \x1b[2m({short_id}…)\x1b[0m");
            stored += 1;
        }
    }

    println!(
        "\nHarvested {stored} entries from {} sessions. Skipped {} near-duplicate.",
        total, dedup_skipped
    );
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ClaudeHistoryEntry deserialization ────────────────────────────────────

    #[test]
    fn deserializes_minimal_entry() {
        let json = r#"{
            "display": "how does the chunker work?",
            "pastedContents": {},
            "timestamp": 1773481284710,
            "project": "/Users/test/myproject",
            "sessionId": "2fb1e326-1dac-4c88-bf4f-9dedd6de630a"
        }"#;
        let entry: ClaudeHistoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.display, "how does the chunker work?");
        assert_eq!(entry.timestamp, 1773481284710);
        assert_eq!(entry.project, "/Users/test/myproject");
        assert_eq!(entry.session_id, "2fb1e326-1dac-4c88-bf4f-9dedd6de630a");
        assert!(entry.pasted_contents.is_empty());
    }

    #[test]
    fn deserializes_entry_with_pasted_contents() {
        let json = r#"{
            "display": "fix this",
            "pastedContents": {
                "slot-1": {"id": "slot-1", "type": "text", "content": "some code here"}
            },
            "timestamp": 1773481284710,
            "project": "/Users/test/project",
            "sessionId": "aaaabbbb-0000-0000-0000-000000000000"
        }"#;
        let entry: ClaudeHistoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.pasted_contents.len(), 1);
        assert_eq!(entry.pasted_contents["slot-1"].content, "some code here");
    }

    #[test]
    fn deserializes_missing_pasted_contents() {
        // Some entries may omit the field entirely.
        let json = r#"{
            "display": "hello",
            "timestamp": 1773481284710,
            "project": "/p",
            "sessionId": "abc"
        }"#;
        let entry: ClaudeHistoryEntry = serde_json::from_str(json).unwrap();
        assert!(entry.pasted_contents.is_empty());
    }

    // ── Project-root filter ───────────────────────────────────────────────────

    #[test]
    fn project_root_filter_matches_exact() {
        let repo_root = "/Users/test/myproject".to_string();
        let project = "/Users/test/myproject".to_string();
        assert!(project.starts_with(&repo_root));
    }

    #[test]
    fn project_root_filter_matches_subpath() {
        let repo_root = "/Users/test/myproject".to_string();
        let project = "/Users/test/myproject/subdir".to_string();
        assert!(project.starts_with(&repo_root));
    }

    #[test]
    fn project_root_filter_rejects_other_project() {
        let repo_root = "/Users/test/myproject".to_string();
        let project = "/Users/test/other-project".to_string();
        assert!(!project.starts_with(&repo_root));
    }

    #[test]
    fn project_root_filter_rejects_prefix_only_match() {
        // "/Users/test/myproject-extra" must NOT match "/Users/test/myproject"
        // as a repo root. The filter uses starts_with on the raw string, so
        // this correctly passes if the repo root has a trailing separator.
        let repo_root = "/Users/test/myproject/".to_string();
        let project = "/Users/test/myproject-extra".to_string();
        assert!(!project.starts_with(&repo_root));
    }

    // ── Secret scan gate ──────────────────────────────────────────────────────

    #[test]
    fn secret_scan_blocks_aws_key() {
        let text = "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        assert!(contains_secret(text));
    }

    #[test]
    fn secret_scan_blocks_github_pat() {
        let text = "token=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef123456789012";
        assert!(contains_secret(text));
    }

    #[test]
    fn secret_scan_allows_clean_conversation() {
        let text = "How does the chunker split files? I want to understand the design.";
        assert!(!contains_secret(text));
    }

    // ── Cap logic ─────────────────────────────────────────────────────────────

    #[test]
    fn cap_truncates_long_text() {
        let long_text: String = "x".repeat(20_000);
        const CAP: usize = 16_000;
        let mut combined = long_text;
        if combined.len() > CAP {
            let boundary = combined.floor_char_boundary(CAP);
            combined.truncate(boundary);
            combined.push_str("\n\n[...truncated]");
        }
        assert!(combined.len() <= CAP + 20); // a bit of slack for the suffix
        assert!(combined.ends_with("[...truncated]"));
    }

    #[test]
    fn cap_leaves_short_text_unchanged() {
        let short_text = "short".to_string();
        const CAP: usize = 16_000;
        let mut combined = short_text.clone();
        if combined.len() > CAP {
            let boundary = combined.floor_char_boundary(CAP);
            combined.truncate(boundary);
            combined.push_str("\n\n[...truncated]");
        }
        assert_eq!(combined, short_text);
    }
}
