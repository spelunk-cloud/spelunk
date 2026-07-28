use anyhow::Result;

use super::MemoryArchiveArgs;
use crate::{
    config::Config,
    storage::{append_state_update, now_secs, open_memory_backend},
};

pub(super) async fn memory_archive(
    args: MemoryArchiveArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    if backend.archive(args.id).await? {
        println!("Archived memory entry #{}.", args.id);

        // ── Git-notes write-through carrier ──────────────────────────────────
        // Best-effort and non-fatal, matching `memory add`/`memory supersede`'s
        // contract: the primary store above already holds the authoritative
        // archive, so a failed carry means only that it stays local for now,
        // never that the command fails. `GitNotesBackend::archive` stays
        // unsupported as a write-through target (explicit `--backend
        // git-notes` never reaches here: it is the primary store then, and
        // already returned `Ok` above).
        let write_through = cfg.store_in_git_notes && backend_override != Some("git-notes");
        if write_through {
            match backend.get(args.id).await {
                // `append_state_update` derives the entity_id from `note`
                // itself (ADR-068 A6) rather than from the rowid `args.id`.
                Ok(Some(note)) => {
                    let invalid_at = note.invalid_at.or_else(|| Some(now_secs()));
                    if let Err(e) =
                        append_state_update(None, &note, "archived", invalid_at, None).await
                    {
                        eprintln!(
                            "Warning: #{} archived locally, but the git-notes carry failed, \
                             so it will not travel with the repo: {e:#}",
                            args.id
                        );
                    }
                }
                Ok(None) => {
                    eprintln!(
                        "Warning: could not re-read #{} after archiving it, so it was not \
                         carried to git notes.",
                        args.id
                    );
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not re-read #{} after archiving it, so it was not \
                         carried to git notes: {e:#}",
                        args.id
                    );
                }
            }
        }
    } else {
        anyhow::bail!("No active memory entry with id {}.", args.id);
    }

    // ADR-037 P2: best-effort, non-blocking nudge of the local relay so a
    // `local_first` archive's outbox drains promptly. See `outbox.rs`.
    super::outbox::nudge_after_write(cfg, mem_path).await;
    Ok(())
}
