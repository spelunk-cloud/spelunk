//! Export local stores to a portable dump.
//!
//! This is a standalone reader. It depends on no other crate in this
//! workspace, opens every store read-only, and discovers each store's shape
//! from the file rather than from a compiled-in schema version. That is what
//! lets one binary read stores written by any past release, and what keeps it
//! from carrying the product's runtime around with it.

pub mod dump;
pub mod inventory;
pub mod source;

use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct ExportOutcome {
    pub counts: dump::Counts,
    pub warnings: Vec<String>,
}

/// Read `store`, write a dump at `out`, and prove the dump matches the store
/// before leaving it in place.
///
/// The dump is written to a sibling temporary file and renamed only once it has
/// been read back and checked, so a failure at any point leaves no dump at all
/// rather than a plausible-looking partial one. Silent partial loss is the risk
/// this whole tool exists to avoid, so a half-written file is worse than none.
pub fn export(store: &Path, out: &Path, generated_at: i64) -> Result<ExportOutcome> {
    let conn = source::open_read_only(store)?;
    // An empty store is not an error. A caller sweeping several stores should
    // get a valid dump for each, and a valid empty dump is what tells a reader
    // "there was nothing here" rather than "this was never exported".
    let extracted = source::extract(&conn)?;
    let (text, per_record, footer) = extracted.dump.render(generated_at)?;

    let temp = out.with_extension("partial");
    std::fs::write(&temp, text.as_bytes())
        .with_context(|| format!("writing {}", temp.display()))?;

    let checked = (|| -> Result<()> {
        let written = std::fs::read_to_string(&temp)
            .with_context(|| format!("reading back {}", temp.display()))?;
        let read_back = dump::verify_rendered(&written)?;
        if read_back.per_record != per_record {
            bail!("the dump on disk does not match what was read from the store");
        }

        // Re-read the store and re-render, so the check compares the file
        // against the store rather than against the in-memory value that
        // produced it. This catches a store that moved underneath the export
        // and every way the write itself can go wrong. It cannot catch a
        // systematic misreading of the store, because a second read repeats it;
        // that is what the fixture corpus is for.
        let again = source::extract(&conn)?;
        let (_, again_records, _) = again.dump.render(generated_at)?;
        if again_records != per_record {
            bail!("the store changed while it was being exported; nothing was written");
        }
        Ok(())
    })();

    if let Err(e) = checked {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }

    std::fs::rename(&temp, out)
        .with_context(|| format!("moving the verified dump into place at {}", out.display()))?;

    Ok(ExportOutcome {
        counts: footer.counts,
        warnings: extracted.warnings,
    })
}
