use anyhow::{Context, Result};
use std::io::Read as _;

use super::PlumbingKnnArgs;
use crate::storage::Database;

pub(super) async fn knn(args: PlumbingKnnArgs, db: &Database) -> Result<()> {
    // Read entire stdin and parse as JSON: {"model":"...","dimensions":N,"vector":[...]}
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading vector from stdin")?;

    let parsed: serde_json::Value =
        serde_json::from_str(input.trim()).context("parsing stdin as JSON")?;

    let vector_arr = parsed
        .get("vector")
        .and_then(|v| v.as_array())
        .context("expected JSON with a \"vector\" array field")?;

    let vector: Vec<f32> = vector_arr
        .iter()
        .map(|v| v.as_f64().map(|f| f as f32))
        .collect::<Option<Vec<_>>>()
        .context("\"vector\" array must contain numbers")?;

    let mut results = db.search_similar(&vector, args.limit + 20)?;

    // Filter by min_score (distance ≤ 1 - min_score for cosine) and language.
    // sqlite-vec returns cosine distance; convert to similarity for --min-score.
    results.retain(|r| {
        let score = 1.0 - r.distance;
        if score < args.min_score {
            return false;
        }
        if let Some(ref lang) = args.lang
            && &r.language != lang
        {
            return false;
        }
        true
    });
    results.truncate(args.limit);

    if results.is_empty() {
        std::process::exit(1);
    }

    for r in &results {
        // Augment with a `score` field without changing SearchResult struct.
        let score = 1.0 - r.distance;
        let mut val = serde_json::to_value(r)?;
        if let serde_json::Value::Object(ref mut m) = val {
            m.insert("score".into(), serde_json::json!(score));
        }
        println!("{}", serde_json::to_string(&val)?);
    }
    Ok(())
}
