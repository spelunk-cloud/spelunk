use anyhow::Result;
use spelunk_core::storage::{PublishOutcome, SkipReason, publish_notes as core_publish_notes};

use super::PlumbingPublishNotesArgs;

/// Publish `refs/notes/spelunk` to a remote (ADR-069 D7).
///
/// Runs against the git repo holding the CWD, so it works before `spelunk init`:
/// git notes are the pre-`init` store of record (ADR-068), and publishing them
/// must not require an index.
pub async fn publish_notes(args: PlumbingPublishNotesArgs) -> Result<()> {
    let remote = args.remote.as_deref().unwrap_or("origin");

    match core_publish_notes(None, remote).await {
        Ok(PublishOutcome::Published { attempts }) => {
            emit(&serde_json::json!({
                "published": true,
                "remote": remote,
                "ref": "refs/notes/spelunk",
                "attempts": attempts,
            }));
            Ok(())
        }
        Ok(PublishOutcome::Skipped(reason)) => {
            // The hook drops stdout, so a JSON-only skip reaches nobody. A user
            // whose memory did not publish has to be told that it did not; the
            // other skips had nothing to publish in the first place.
            if reason == SkipReason::LockUnavailable {
                eprintln!(
                    "spelunk: memory not published: another spelunk process holds the \
                     notes lock."
                );
                eprintln!("spelunk: your code push is unaffected; your next push publishes it.");
            }
            emit(&serde_json::json!({
                "published": false,
                "remote": remote,
                "skipped": reason.as_str(),
            }));
            Ok(())
        }
        // A hook exiting non-zero aborts the user's branch push outright, so a
        // publish failure must not reach the exit status through it (D3).
        Err(e) if args.best_effort => {
            eprintln!("spelunk: {e:#}");
            eprintln!(
                "spelunk: your code push is unaffected. Retry with: \
                 git push {remote} refs/notes/spelunk"
            );
            emit(&serde_json::json!({
                "published": false,
                "remote": remote,
                "error": format!("{e:#}"),
            }));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn emit(value: &serde_json::Value) {
    println!("{value}");
}
