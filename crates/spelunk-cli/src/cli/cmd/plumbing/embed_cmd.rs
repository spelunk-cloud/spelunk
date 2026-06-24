use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{BufRead as _, IsTerminal as _};

use crate::{
    cli::cmd::helpers::{embed_query_vec, require_server_client},
    config::Config,
};

#[derive(Serialize)]
struct EmbedOutput {
    model: String,
    dimensions: usize,
    vector: Vec<f32>,
}

pub(super) async fn embed_cmd(cfg: &Config, query_mode: bool) -> Result<()> {
    if std::io::stdin().is_terminal() {
        eprintln!("spelunk plumbing embed: reads lines from stdin, emits JSONL embedding per line");
        std::process::exit(2);
    }

    let client = require_server_client(cfg, "plumbing embed")?;
    let model = cfg.embedding_model.clone();

    let stdin = std::io::stdin();
    for (idx, line) in stdin.lock().lines().enumerate() {
        let text = line.context("reading stdin")?;
        if text.trim().is_empty() {
            continue;
        }
        let vector = if query_mode {
            embed_query_vec(
                &client,
                "Given a code search query, retrieve the relevant code snippets",
                &text,
            )
            .await
            .with_context(|| format!("embedding line {idx}"))?
        } else {
            let input = format!("title: none | text: {text}");
            client
                .embed_text(&input)
                .await
                .with_context(|| format!("embedding line {idx}"))?
        };
        let dimensions = vector.len();
        println!(
            "{}",
            serde_json::to_string(&EmbedOutput {
                model: model.clone(),
                dimensions,
                vector,
            })?
        );
    }
    Ok(())
}
