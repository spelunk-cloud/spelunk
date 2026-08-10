use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

#[derive(Args, Debug)]
/// Manage and inspect cross-project links.
/// Use `spelunk link <path>` / `spelunk unlink <path>` to add or remove links.
pub struct LinksArgs {
    #[command(subcommand)]
    pub command: LinksCommand,
}

#[derive(Subcommand, Debug)]
pub enum LinksCommand {
    /// List all linked projects with their status
    List(LinksListArgs),
    /// Check all linked project indexes are fresh (exit 1 if any are stale or missing)
    Check,
}

#[derive(Args, Debug)]
pub struct LinksListArgs {
    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

use super::helpers::project_display_name;
use crate::{config::Config, registry::Registry, storage::Database};

pub async fn links(args: LinksArgs, _cfg: Config) -> Result<()> {
    match args.command {
        LinksCommand::List(list_args) => links_list(list_args.format).await,
        LinksCommand::Check => links_check().await,
    }
}

// ── links list ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LinkedProjectInfo {
    name: String,
    root_path: String,
    db_path: String,
    db_exists: bool,
    status: String,
    file_count: Option<i64>,
}

async fn links_list(format: String) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let reg = Registry::open().context("opening registry")?;

    let project = reg.find_project_for_path(&cwd)?.with_context(|| {
        "No indexed project found for the current directory.\n\
             Run `spelunk index .` first."
            .to_string()
    })?;

    let deps = reg.get_deps(project.id).unwrap_or_default();

    let mut infos: Vec<LinkedProjectInfo> = Vec::new();

    for dep in &deps {
        let name = project_display_name(&dep.root_path);

        let db_exists = dep.db_path.exists();

        let (status, file_count) = if db_exists {
            match Database::open(&dep.db_path) {
                Ok(db) => {
                    let fc = db.stats().ok().map(|s| s.file_count);
                    // Resolve indexed (root-relative) paths against the LINKED
                    // project's own root, not this (linking) project's cwd.
                    let staleness = db.staleness_report(&dep.root_path, Some(5)).ok();
                    let status_str = match staleness {
                        Some(r) if r.stale > 0 => "stale".to_string(),
                        Some(_) => "fresh".to_string(),
                        None => "unknown".to_string(),
                    };
                    (status_str, fc)
                }
                Err(_) => ("error".to_string(), None),
            }
        } else {
            ("db missing".to_string(), None)
        };

        infos.push(LinkedProjectInfo {
            name,
            root_path: dep.root_path.to_string_lossy().into_owned(),
            db_path: dep.db_path.to_string_lossy().into_owned(),
            db_exists,
            status,
            file_count,
        });
    }

    match crate::utils::effective_format(&format) {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&infos)?);
        }
        _ => {
            if infos.is_empty() {
                println!("No linked projects.");
                println!("Use `spelunk link <path>` to add a cross-project dependency.");
                return Ok(());
            }

            println!("linked projects ({}):\n", infos.len());

            // Compute column widths for aligned output.
            let name_w = infos.iter().map(|i| i.name.len()).max().unwrap_or(0).max(4);
            let path_w = infos
                .iter()
                .map(|i| abbreviated_path(&i.root_path).len())
                .max()
                .unwrap_or(0)
                .max(4);

            for info in &infos {
                let path_abbrev = abbreviated_path(&info.root_path);
                let fc_str = match info.file_count {
                    Some(n) => format!("({n} files)"),
                    None => String::new(),
                };
                println!(
                    "  {name:<name_w$}  {path:<path_w$}  {status:<10}  {fc}",
                    name = info.name,
                    path = path_abbrev,
                    status = info.status,
                    fc = fc_str,
                    name_w = name_w,
                    path_w = path_w,
                );
            }
        }
    }

    Ok(())
}

// ── links check ───────────────────────────────────────────────────────────────

async fn links_check() -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let reg = Registry::open().context("opening registry")?;

    let project = reg.find_project_for_path(&cwd)?.with_context(|| {
        "No indexed project found for the current directory.\n\
             Run `spelunk index .` first."
            .to_string()
    })?;

    let deps = reg.get_deps(project.id).unwrap_or_default();

    if deps.is_empty() {
        println!("No linked projects — nothing to check.");
        return Ok(());
    }

    let mut all_ok = true;
    let mut problems: Vec<String> = Vec::new();

    for dep in &deps {
        let name = project_display_name(&dep.root_path);

        if !dep.db_path.exists() {
            all_ok = false;
            problems.push(format!("  {name}: DB missing at {}", dep.db_path.display()));
            continue;
        }

        match Database::open(&dep.db_path) {
            // Resolve indexed (root-relative) paths against the LINKED project's
            // own root, not this (linking) project's cwd.
            Ok(db) => match db.staleness_report(&dep.root_path, Some(5)) {
                Ok(r) if r.stale > 0 => {
                    all_ok = false;
                    problems.push(format!(
                        "  {name}: stale ({}/{} sampled files changed)",
                        r.stale, r.sampled
                    ));
                }
                Ok(_) => {
                    // fresh — no problem
                }
                Err(e) => {
                    all_ok = false;
                    problems.push(format!("  {name}: staleness check failed: {e}"));
                }
            },
            Err(e) => {
                all_ok = false;
                problems.push(format!("  {name}: could not open DB: {e}"));
            }
        }
    }

    if all_ok {
        println!("All {} linked project(s) are fresh.", deps.len());
        Ok(())
    } else {
        eprintln!("Some linked projects are not fresh:");
        for p in &problems {
            eprintln!("{p}");
        }
        eprintln!("\nRun `spelunk index <path>` in each stale project to refresh.");
        std::process::exit(1);
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Replace the home directory prefix with `~` for compact display.
fn abbreviated_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}
