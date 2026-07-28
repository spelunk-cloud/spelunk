use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct ExploreArgs {
    /// The question to answer about the codebase
    pub question: String,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Maximum number of tool-call steps before forcing a final answer
    #[arg(long, default_value_t = 10)]
    pub max_steps: usize,

    /// Print each tool call and result to stderr as they happen
    #[arg(long)]
    pub verbose: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Output result as JSON (deprecated — use --format json)
    #[arg(long, hide = true)]
    pub json: bool,
}

use std::sync::Arc;

use super::helpers::open_project_db;
use super::search::maybe_warn_stale;
use super::ui::spinner;
use crate::{
    capability,
    config::Config,
    search::explore::{ExploreResult, Explorer},
    server_client::{ServerEmbedAdapter, ServerInferenceClient, ServerLlmAdapter},
};

pub async fn explore(args: ExploreArgs, cfg: Config) -> Result<()> {
    // ADR-067: gate on a local project first (fail-closed, no global fallback) so
    // an un-init'd dir refuses before any server probe, and explore never reads
    // the machine-global index.db.
    let (db_path, _db) = open_project_db(args.db.as_deref(), &cfg.db_path)?;

    let tier = capability::get_tier(&cfg).await;
    capability::require_tier1("explore", tier, cfg.server_url.as_deref())?;

    maybe_warn_stale(&db_path);
    crate::storage::record_usage_at(&db_path, "explore");

    // Honor the capability tier: when the server was auto-discovered via the
    // loopback probe, `cfg.server_url` is unset; fill it in from the tier so the
    // inference client can be built (IMP-3 / spelunk#316).
    //
    // `get_inference_tier` (not `tier`/`get_tier` above, which governs the
    // `require_tier1` feature gate): local_first always prefers the local
    // loopback embedder/LLM for inference, even with an explicit server_url
    // set (2026-07-23 founder decision).
    let project_root = db_path.parent().unwrap_or(&db_path);
    let inference_tier = capability::get_inference_tier(&cfg).await;
    let eff_cfg = inference_tier.effective_config(&cfg, project_root);
    let client = ServerInferenceClient::from_config(&eff_cfg).ok_or_else(|| {
        anyhow::anyhow!(
            "'spelunk explore' requires spelunk-server.\n\
             Set server_url in ~/.config/spelunk/config.toml to enable this feature."
        )
    })?;
    let client = Arc::new(client);

    let sp = spinner("Connecting to inference server…");
    let embedder = ServerEmbedAdapter(Arc::clone(&client));
    let llm = ServerLlmAdapter(Arc::clone(&client));
    sp.finish_and_clear();

    let verbose = args.verbose || crate::utils::is_agent_mode();
    let fmt = if args.json {
        "json".to_string()
    } else {
        args.format.clone()
    };
    let use_json = crate::utils::effective_format(&fmt) == "json" || crate::utils::is_agent_mode();

    if !use_json {
        eprintln!("Exploring: {}\n", args.question);
    }

    let explorer = Explorer::new(
        db_path.clone(),
        project_root.to_path_buf(),
        &embedder,
        &llm,
        args.max_steps,
        verbose,
    );
    let result = explorer.explore(&args.question).await?;

    if use_json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_result(&result);
    }

    Ok(())
}

fn print_result(result: &ExploreResult) {
    println!("{}", result.answer);
    if !result.sources.is_empty() {
        println!("\nSources:");
        for src in &result.sources {
            println!("  {src}");
        }
    }
    if !result.steps.is_empty() {
        let tools: Vec<&str> = result.steps.iter().map(|s| s.tool.as_str()).collect();
        println!(
            "\n{} tool call(s): {}",
            result.steps.len(),
            tools.join(", ")
        );
    }
}
