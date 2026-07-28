//! `spelunk memory reindex`: backfill missing local note embeddings.
//!
//! A note's vector is minted only at `memory add` time. When no embedder was
//! reachable then, or the 768→896 store upgrade discarded every prior vector,
//! the note stays present-but-unembedded and semantic `memory search` can no
//! longer surface it, with no catch-up path. This command re-embeds those notes
//! through the same LOCAL embed path `add` uses, committing each vector as it
//! completes so an interrupted run resumes on re-run.

use anyhow::{Context, Result};
use serde::Serialize;

use super::super::helpers::require_server_client;
use super::MemoryReindexArgs;
use crate::{
    capability,
    config::{Config, SyncMode},
    embeddings::vec_to_blob,
    storage::MemoryStore,
};

/// Counts partition the store: `total_active == already_embedded + missing_before`.
/// `remaining` is how many of the targeted notes are still unembedded after the
/// run (0 on full success). `would_embed` is populated under `--dry-run` only.
#[derive(Debug, Serialize)]
struct ReindexSummary {
    total_active: usize,
    missing_before: usize,
    already_embedded: usize,
    embedded: usize,
    remaining: usize,
    would_embed: usize,
    include_archived: bool,
    force: bool,
}

enum Outcome {
    DryRun,
    NothingToDo,
    Done,
}

pub(super) async fn memory_reindex(
    args: MemoryReindexArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Embeddings are a sqlite-vec concern; git notes hold no vectors.
    if backend_override == Some("git-notes") {
        anyhow::bail!(
            "This operation requires the sqlite backend. \
             Re-run without --backend git-notes."
        );
    }

    // `memory reindex` backfills vectors into the LOCAL `memory.db` (opened
    // directly below, bypassing `open_memory_backend`'s mode-based routing).
    // Mirror `open_memory_backend`'s exact `route_remote` condition
    // (`storage/mod.rs`): `cloud_first` only relocates the store of record
    // to `server_url` when one is actually configured. `cloud_first` with no
    // `server_url` set has nothing to route to, so `open_memory_backend`
    // itself falls back to `memory.db` there too, memory.db is the store of
    // record and there IS something local to re-embed. Gating on `mode`
    // alone (ignoring `server_url`) would reject that exact case, contrary
    // to `open_memory_backend`'s own routing (2026-07-23 founder decision).
    if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
        anyhow::bail!(
            "'spelunk memory reindex' is not applicable in cloud_first mode with \
             server_url set: memory.db is not the store of record there (server_url \
             owns memory), so there is nothing local to re-embed."
        );
    }

    let json = crate::utils::effective_format(&args.format) == "json";

    let store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let total_active = store.count().context("counting active notes")? as usize;
    // `missing_before` is always the active-notes-missing count, independent of
    // the flags, so the summary partitions cleanly regardless of --force /
    // --include-archived.
    let missing_before = store
        .notes_missing_embeddings(false)
        .context("finding notes missing embeddings")?
        .len();
    let already_embedded = total_active.saturating_sub(missing_before);

    let candidates = if args.force {
        store
            .all_active_notes_for_reembed(args.include_archived)
            .context("listing notes to re-embed")?
    } else {
        store
            .notes_missing_embeddings(args.include_archived)
            .context("finding notes missing embeddings")?
    };

    let mut summary = ReindexSummary {
        total_active,
        missing_before,
        already_embedded,
        embedded: 0,
        remaining: 0,
        would_embed: 0,
        include_archived: args.include_archived,
        force: args.force,
    };

    // Dry-run and nothing-to-do both exit 0 without touching the embedder, so a
    // count/no-op never requires a running server.
    if args.dry_run {
        summary.would_embed = candidates.len();
        emit_summary(&summary, json, Outcome::DryRun);
        return Ok(());
    }
    if candidates.is_empty() {
        emit_summary(&summary, json, Outcome::NothingToDo);
        return Ok(());
    }

    // Reuse add's LOCAL embed path so reindex works with no explicit server_url:
    // an auto-discovered loopback server sets the tier without populating
    // `server_url`, so bridge it into an effective config first (ADR-004),
    // exactly as `memory add` / `memory search` do (`project_root` is the store's
    // parent).
    let project_root = mem_path.parent().unwrap_or(mem_path);
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder, even with an explicit server_url set.
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    // No embedder reachable → actionable error + non-zero exit, before any
    // write. Deliberately unlike reconcile (which imports without embeddings):
    // here embedding IS the point, so silence-and-succeed would recreate the bug.
    let client = require_server_client(&eff_cfg, "memory reindex")?;

    let total = candidates.len();
    let mut embedded = 0usize;
    for (id, title, body) in &candidates {
        // Byte-identical to add.rs's document string: a backfilled vector must
        // match an add-time one, so this format must not drift. `embed_text`
        // embeds the raw document (NOT `embed_query`, which prepends the F2LLM
        // `Instruct:/Query:` prefix and would produce a query-side vector).
        let doc = format!("title: {title} | text: {body}");
        let vec = match client.embed_text(&doc).await {
            Ok(v) => v,
            Err(e) => {
                // Every note embedded so far is already durably committed
                // (`insert_embedding` commits per call), so a re-run resumes the
                // remainder rather than starting over.
                return Err(e.context(format!(
                    "embedding note #{id} ({embedded} of {total} done and durably stored; \
                     re-run 'spelunk memory reindex' to resume the rest)"
                )));
            }
        };
        let blob = vec_to_blob(&vec);
        store
            .insert_embedding(*id, &blob)
            .with_context(|| format!("storing embedding for note #{id}"))?;
        embedded += 1;
        eprintln!("[spelunk] embedded {embedded}/{total}…");
    }

    summary.embedded = embedded;
    summary.remaining = total - embedded;
    emit_summary(&summary, json, Outcome::Done);
    Ok(())
}

fn emit_summary(s: &ReindexSummary, json: bool, outcome: Outcome) {
    if json {
        println!("{}", serde_json::to_string(s).unwrap_or_default());
        return;
    }
    match outcome {
        Outcome::DryRun => println!(
            "Dry run: {} note(s) would be embedded ({} active total, {} already embedded). \
             Nothing written.",
            s.would_embed, s.total_active, s.already_embedded
        ),
        Outcome::NothingToDo => println!(
            "Nothing to reindex: all {} active note(s) already embedded.",
            s.total_active
        ),
        Outcome::Done => println!(
            "Reindex complete: {} embedded, {} remaining ({} missing before, {} active total).",
            s.embedded, s.remaining, s.missing_before, s.total_active
        ),
    }
}
