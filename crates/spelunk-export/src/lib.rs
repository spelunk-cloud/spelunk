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

impl ExportOutcome {
    /// What the run tells the user it did.
    ///
    /// The claim has to be exactly as strong as what was established and no
    /// stronger. A dump that omits links whose endpoint is missing from the
    /// store is still a faithful dump of everything the format can express, but
    /// a run that says only "verified" invites the reader to conclude nothing
    /// was left behind. So the omissions are counted on the same screen as the
    /// success, not left to a warning stream that a caller may not be reading.
    pub fn summary(&self, out: &Path) -> String {
        let mut s = format!("Wrote {}\n", out.display());
        if self.counts.entity.is_empty() && self.counts.relationship.is_empty() {
            s.push_str("  the store held nothing to carry\n");
        } else {
            for (kind, n) in &self.counts.entity {
                s.push_str(&format!("  {n} {kind}\n"));
            }
            for (kind, n) in &self.counts.relationship {
                s.push_str(&format!("  {n} {kind}\n"));
            }
            s.push_str("  the dump reads back as exactly what was read from the store\n");
        }
        if !self.warnings.is_empty() {
            let n = self.warnings.len();
            s.push_str(&format!(
                "  {n} link(s) were NOT carried: each names an entry that is not in the store. \
                 The store was read as of a single point in time, so this is damage in the \
                 store itself, not a write that arrived mid-export. See the warnings above.\n"
            ));
        }
        s
    }
}

/// Read `store`, write a dump at `out`, and prove the dump matches the store
/// before leaving it in place.
pub fn export(store: &Path, out: &Path, generated_at: i64) -> Result<ExportOutcome> {
    let conn = source::open_read_only(store)?;
    // An empty store is not an error. A caller sweeping several stores should
    // get a valid dump for each, and a valid empty dump is what tells a reader
    // "there was nothing here" rather than "this was never exported".
    let extracted =
        source::extract(&conn).with_context(|| format!("reading {}", store.display()))?;
    let (text, per_record, footer) = extracted.dump.render(generated_at)?;

    write_verified(&text, &per_record, out)?;

    Ok(ExportOutcome {
        counts: footer.counts,
        warnings: extracted.warnings,
    })
}

/// Put `text` at `out`, but only once the bytes that actually landed have been
/// read back and shown to be those bytes.
///
/// The dump goes to a sibling temporary file and is renamed only after it reads
/// back as a whole, self-consistent dump whose records hash to `expected`, so a
/// failure at any point leaves no dump at all rather than a plausible-looking
/// partial one. Silent partial loss is the risk this whole tool exists to
/// avoid, and a truncated file that no one is told about is exactly that.
///
/// Re-reading the store instead of the rendered bytes would add nothing: the
/// store is read inside one transaction, so a second read returns the same rows
/// by construction. The comparison that can still fail, and therefore the only
/// one worth making, is between the file and what was rendered.
pub fn write_verified(text: &str, expected: &[String], out: &Path) -> Result<()> {
    let temp = out.with_extension("partial");
    std::fs::write(&temp, text.as_bytes())
        .with_context(|| format!("writing {}", temp.display()))?;

    let checked = (|| -> Result<()> {
        let written = std::fs::read_to_string(&temp)
            .with_context(|| format!("reading back {}", temp.display()))?;
        let read_back = dump::verify_rendered(&written)?;
        if read_back.per_record != expected {
            bail!("the dump on disk does not match what was read from the store");
        }
        Ok(())
    })();

    if let Err(e) = checked {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }

    std::fs::rename(&temp, out)
        .with_context(|| format!("moving the verified dump into place at {}", out.display()))
}
