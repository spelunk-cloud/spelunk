use anyhow::{Context, Result};

use crate::{
    config::{Config, resolve_db},
    embeddings::vec_to_blob,
    server_client::ServerInferenceClient,
    storage::Database,
};

/// Resolve the DB path via `resolve_db`, error if not found, open and return
/// both the path and the opened database.
pub(crate) fn open_project_db(
    db: Option<&std::path::Path>,
    cfg_path: &std::path::Path,
) -> Result<(std::path::PathBuf, Database)> {
    let db_path = resolve_db(db, cfg_path);
    if !db_path.exists() {
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` inside your project first."
        );
    }
    let database = Database::open(&db_path)?;
    Ok((db_path, database))
}

/// Build a `ServerInferenceClient` from config, returning an error if
/// `server_url` is not configured.
pub(crate) fn require_server_client(cfg: &Config, feature: &str) -> Result<ServerInferenceClient> {
    ServerInferenceClient::from_config(cfg).ok_or_else(|| {
        anyhow::anyhow!(
            "'spelunk {feature}' requires spelunk-server.\n\
             Set server_url in ~/.config/spelunk/config.toml to enable this feature."
        )
    })
}

/// Embed a query with the given F2LLM instruction and return the raw float vector.
///
/// `task` is the full instruction string (e.g. "Given a question, retrieve …").
/// The format matches F2LLM-v2-330M's expected query prompt:
/// `Instruct: <task>\nQuery: <query>`.
pub(crate) async fn embed_query_vec(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<f32>> {
    let query_text = format!("Instruct: {task}\nQuery: {query}");
    client.embed_text(&query_text).await
}

/// Embed a query with the given task prefix and return the blob bytes suitable
/// for KNN search.
pub(crate) async fn embed_query(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<u8>> {
    let vec = embed_query_vec(client, task, query).await?;
    Ok(vec_to_blob(&vec))
}

/// Return the final path component of `path` as a display name, falling back
/// to the full path string if there is no file name component.
pub(crate) fn project_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Detach: re-exec this binary with the same CLI arguments but without
/// `--detach`, with all stdio closed, so the caller (e.g. a git hook) regains
/// its prompt immediately while spelunk continues in the background.
pub(crate) fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();
    std::process::Command::new(exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning detached background process")?;
    Ok(())
}
