use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::{Parser, Subcommand};

use spelunk_export::{dump, export, inventory};

#[derive(Parser)]
#[command(
    name = "spelunk-export",
    about = "Export local spelunk stores to a portable dump",
    long_about = "Reads local stores and writes a portable, documented dump: line-delimited \
                  JSON, one record per line, readable without any special tooling.\n\n\
                  Stores are opened read-only and are never modified. Only authored data is \
                  carried; derived state such as full-text indexes and embeddings is left out \
                  because it regenerates. No network access is made at any point.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a portable dump of one store.
    Export {
        /// The store to read. Opened read-only and never modified.
        #[arg(long)]
        store: PathBuf,
        /// Where to write the dump. Written only once it has been verified
        /// against the store.
        #[arg(long)]
        out: PathBuf,
    },
    /// Report what a store holds, without touching it.
    Inventory {
        /// One or more stores to describe. Missing paths are reported as
        /// missing rather than treated as an error.
        #[arg(long = "store", num_args = 1..)]
        stores: Vec<PathBuf>,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Export { store, out } => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            let outcome = export(&store, &out, now)?;
            for w in &outcome.warnings {
                eprintln!("warning: {w}");
            }
            report(&outcome.counts, &out);
            Ok(())
        }
        Command::Inventory { stores } => {
            let reports: Vec<_> = stores.iter().map(|p| inventory::describe(p)).collect();
            println!("{}", serde_json::to_string_pretty(&reports)?);
            Ok(())
        }
    }
}

fn report(counts: &dump::Counts, out: &std::path::Path) {
    println!("Wrote {}", out.display());
    if counts.entity.is_empty() && counts.relationship.is_empty() {
        println!("  the store held nothing to carry");
        return;
    }
    for (kind, n) in &counts.entity {
        println!("  {n} {kind}");
    }
    for (kind, n) in &counts.relationship {
        println!("  {n} {kind}");
    }
    println!("  verified against the store: counts and per-record checksums match");
}
