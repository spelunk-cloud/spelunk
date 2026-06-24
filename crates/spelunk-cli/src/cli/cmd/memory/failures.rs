use anyhow::Result;

use super::{MemoryFailuresArgs, parse_as_of, print_note_summary};
use crate::{config::Config, storage::open_memory_backend};

pub(super) async fn memory_failures(
    args: MemoryFailuresArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let as_of = parse_as_of(args.as_of.as_deref())?;
    let notes = backend
        .list(Some("antipattern"), args.limit, false, as_of)
        .await?;

    if notes.is_empty() {
        println!("No antipatterns stored yet.");
        println!(
            "Run `spelunk memory harvest --source failures` to extract them from git history,\n\
             or add one manually with `spelunk memory add --kind antipattern --title \"...\" --body \"...\"`"
        );
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&notes)?),
        _ => {
            for n in &notes {
                print_note_summary(n);
            }
        }
    }
    Ok(())
}
