use anyhow::Result;

use super::PlumbingReadConventionsArgs;
use crate::storage::Database;

/// Emit stored convention records as JSONL.
///
/// Exit codes:
/// - 0: at least one row emitted
/// - 1: no rows found (table empty or `--lang` filter matched nothing)
/// - 2: DB error (handled by the `plumbing()` dispatcher returning `Err`)
pub(super) fn read_conventions(args: PlumbingReadConventionsArgs, db: &Database) -> Result<()> {
    let lang_lower = args.lang.as_deref().map(|s| s.to_lowercase());
    let lang = lang_lower.as_deref();

    let records = crate::conventions::list_conventions(db, lang)?;

    if records.is_empty() {
        std::process::exit(1);
    }

    for record in &records {
        println!("{}", serde_json::to_string(record)?);
    }
    Ok(())
}
