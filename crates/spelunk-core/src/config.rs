use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(test)]
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Project-id derivation
// ---------------------------------------------------------------------------

/// Derive a stable project identifier from `project_root`.
///
/// 1. Read `remote.origin.url` from the git config and normalise to
///    `host/owner/repo`.
/// 2. If no git repo or no origin remote, fall back to
///    `local/<blake3-hex-of-canonical-path>`.
pub fn derive_project_id(project_root: &Path) -> String {
    try_derive_from_git(project_root).unwrap_or_else(|| derive_local_fallback(project_root))
}

fn try_derive_from_git(root: &Path) -> Option<String> {
    let repo = gix::discover(root).ok()?;
    let git_dir = repo.git_dir();

    // For linked worktrees the config lives in the main .git dir, not
    // .git/worktrees/<name>.
    let config_path = if git_dir.parent().and_then(|p| p.file_name())
        == Some(std::ffi::OsStr::new("worktrees"))
    {
        git_dir.parent()?.parent()?.join("config")
    } else {
        git_dir.join("config")
    };

    let content = std::fs::read_to_string(config_path).ok()?;
    let url = extract_origin_url_from_git_config(&content)?;
    Some(normalise_git_url(&url))
}

/// Minimal parser for git config: finds `url` under `[remote "origin"]`.
fn extract_origin_url_from_git_config(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Section header — check if it's [remote "origin"]
            let header = trimmed.trim_start_matches('[').trim_end_matches(']');
            in_origin = header.trim() == r#"remote "origin""#;
        } else if in_origin
            && let Some(rest) = trimmed.strip_prefix("url")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
        {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Normalise a git remote URL to `host/owner/repo` (no scheme, no `.git`).
///
/// Handles `https://`, `ssh://`, and SCP-style `git@host:owner/repo.git`.
fn normalise_git_url(url: &str) -> String {
    let without_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    let without_user = if let Some(pos) = without_scheme.find('@') {
        &without_scheme[pos + 1..]
    } else {
        without_scheme
    };
    // SCP colon → slash
    let normalised = without_user.replacen(':', "/", 1);
    let normalised = normalised.strip_suffix(".git").unwrap_or(&normalised);
    normalised.to_lowercase()
}

fn derive_local_fallback(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    format!("local/{}", hash.to_hex())
}

/// Returns `~/.config/spelunk/`.
///
/// On all platforms we use `~/.config` rather than the OS-native config dir
/// (e.g. `~/Library/Application Support` on macOS) so that the path matches
/// what the CLI documentation and error messages say, and so that config files
/// work the same way across Linux and macOS.
fn spelunk_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("spelunk")
}

/// Walk up from `start` looking for `.spelunk/index.db`.
/// Returns the first match found, or `None` if the filesystem root is reached.
///
/// If `start` is inside a git linked worktree, the walk begins from the main
/// worktree root so that linked worktrees share the same index without a symlink.
pub fn find_project_db(start: &Path) -> Option<PathBuf> {
    let resolved = crate::utils::resolve_main_worktree_root(start);
    let mut dir = resolved;
    loop {
        let candidate = dir.join(".spelunk").join("index.db");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Walk up from `start` looking for `.spelunk/config.toml` (project-level config).
/// Stops at the filesystem root. Returns the path if found.
fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".spelunk").join("config.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Fields that can be set in `.spelunk/config.toml` (project-level, checked-in).
/// Only contains fields safe to share with the team (no secrets).
#[derive(Debug, Default, Deserialize)]
struct ProjectConfig {
    /// Canonical server URL (preferred).
    server_url: Option<String>,
    /// Shared API key — acceptable if the server is behind a VPN/firewall.
    /// For secrets, prefer `SPELUNK_SERVER_KEY` env var instead.
    server_key: Option<String>,
    /// Deprecated alias for server_url.
    memory_server_url: Option<String>,
    /// Deprecated alias for server_key.
    memory_server_key: Option<String>,
    project_id: Option<String>,
    /// Embedding vector dimension (must match the embedding model). Project-level
    /// so a repo can pin the dimension of the model it was indexed with.
    embedding_dim: Option<usize>,
    /// Per-model document embedding-text template (placeholders `{title}`, `{body}`).
    document_prompt_template: Option<String>,
}

/// Resolve the database path.
///
/// Priority: explicit `--db` arg > project DB (walk up from CWD) > `cfg_default`.
pub fn resolve_db(explicit: Option<&Path>, cfg_default: &Path) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(p) = find_project_db(&cwd)
    {
        return p;
    }
    cfg_default.to_path_buf()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the SQLite database file
    #[serde(default = "Config::default_db_path")]
    pub db_path: PathBuf,

    /// Directory where model weights are cached (used by backend-metal)
    #[serde(default = "Config::default_models_dir")]
    pub models_dir: PathBuf,

    /// Model ID for embeddings.
    /// LM Studio: the model's API key shown in the LM Studio UI
    ///   (e.g. `text-embedding-embeddinggemma-300m-qat`).
    /// Metal: HuggingFace repo ID (e.g. `google/embeddinggemma-300m`).
    #[serde(default = "Config::default_embedding_model")]
    pub embedding_model: String,

    /// Model ID for the LLM used by `ask`, `memory harvest`, and `plan create`.
    /// LM Studio: the model's API key (e.g. `google/gemma-3n-e4b`).
    /// When unset, commands that require a chat model are unavailable.
    #[serde(default)]
    pub llm_model: Option<String>,

    /// Default embedding batch size
    #[serde(default = "Config::default_batch_size")]
    pub batch_size: usize,

    /// Embedding vector dimension. Must match the embedding model's output.
    /// The document vector table (`embeddings`) is created with this dimension
    /// on first index; the vec0 table dimension is fixed at creation, so
    /// switching to a model with a different dimension requires re-indexing on a
    /// fresh `.spelunk` (`index --force` after removing the index DB).
    /// Default: 768 (EmbeddingGemma / Nomic Embed Text v1.5).
    #[serde(default = "Config::default_embedding_dim")]
    pub embedding_dim: usize,

    /// Document embedding-text template. Placeholders: `{title}`, `{body}`.
    /// When unset, reproduces EmbeddingGemma's format
    /// (`title: {title} | text: {body}`). Set per embedding model to match its
    /// recommended document format (e.g. nomic: `search_document: {body}`).
    #[serde(default)]
    pub document_prompt_template: Option<String>,

    /// Base URL for the OpenAI-compatible API server (e.g. LM Studio, Ollama, vLLM).
    /// Default: `http://127.0.0.1:1234`
    #[serde(default = "Config::default_api_base_url", alias = "lmstudio_base_url")]
    pub api_base_url: String,

    // ── spelunk-server (optional) ─────────────────────────────────────────────
    /// URL of the spelunk-server instance, e.g. `http://spelunk.internal:7777`.
    /// When set, the CLI operates in Tier 1 (server-connected) mode, enabling
    /// semantic search, embedding, explore, and plan features.
    /// Set in `.spelunk/config.toml` (project-level) or via `SPELUNK_SERVER_URL`.
    /// The old `memory_server_url` TOML key is accepted as a backward-compat alias.
    #[serde(default, alias = "memory_server_url")]
    pub server_url: Option<String>,

    /// Bearer token for spelunk-server auth.
    /// Set in `~/.config/spelunk/config.toml` (personal) or via `SPELUNK_SERVER_KEY`.
    /// Do NOT commit this to `.spelunk/config.toml`.
    /// The old `memory_server_key` TOML key is accepted as a backward-compat alias.
    #[serde(default, alias = "memory_server_key")]
    pub server_key: Option<String>,

    /// Project slug for the spelunk-server (e.g. `acme/my-app`).
    /// Required when `server_url` is set.
    /// Set in `.spelunk/config.toml` (project-level) or via `SPELUNK_PROJECT_ID`.
    #[serde(default)]
    pub project_id: Option<String>,

    /// URL of a server used **only** for inference (embeddings + LLM), never for
    /// memory storage. Populated at runtime (not from config files) by
    /// `Tier::effective_config()` when a loopback server is auto-discovered: the
    /// auto-discovered server is an inference cache over the local `memory.db`,
    /// not a second memory store (ADR-004). Inference clients prefer this field
    /// and fall back to `server_url`; the memory backend selector
    /// (`open_memory_backend`) ignores it entirely, so an auto-discovered server
    /// never diverts memory CRUD away from the project's local `memory.db`.
    #[serde(skip)]
    pub inference_url: Option<String>,

    // ── Directory conventions ─────────────────────────────────────────────────
    /// Directory (relative to project root) where `spelunk plan create` writes plan files.
    /// Default: `docs/plans`
    #[serde(default = "Config::default_plans_dir")]
    pub plans_dir: PathBuf,

    /// Directory (relative to project root) where spec markdown files are discovered
    /// during `spelunk index` and where `spelunk spec` looks for spec files.
    /// Default: `docs/specs`
    #[serde(default = "Config::default_specs_dir")]
    pub specs_dir: PathBuf,

    /// Context-window size (tokens) of the LLM used for `memory harvest` and `ask`.
    /// spelunk uses this to split harvest batches that would overflow the model's window.
    /// Set to match the `n_ctx` / context-length of the model you have loaded.
    /// Default: 8192
    #[serde(default = "Config::default_llm_context_length")]
    pub llm_context_length: usize,

    /// When true (the default), `spelunk memory add` also appends the new entry
    /// as a line of JSON in `refs/notes/spelunk` on HEAD.
    ///
    /// This keeps memory close to commits and is consistent with the product's
    /// "memory travels with code" messaging.  Set `store_in_git_notes = false`
    /// in your config to opt out.
    ///
    /// Failure to write the git note is non-fatal: a warning is logged and the
    /// primary SQLite write is unaffected.
    #[serde(default = "Config::default_store_in_git_notes")]
    pub store_in_git_notes: bool,
}

impl Config {
    fn default_db_path() -> PathBuf {
        spelunk_config_dir().join("index.db")
    }
    fn default_models_dir() -> PathBuf {
        spelunk_config_dir().join("models")
    }
    fn default_embedding_model() -> String {
        "text-embedding-embeddinggemma-300m-qat".to_string()
    }
    fn default_api_base_url() -> String {
        "http://127.0.0.1:1234".to_string()
    }
    fn default_batch_size() -> usize {
        32
    }
    pub fn default_embedding_dim() -> usize {
        768
    }
    fn default_plans_dir() -> PathBuf {
        PathBuf::from("docs/plans")
    }
    fn default_specs_dir() -> PathBuf {
        PathBuf::from("docs/specs")
    }
    fn default_llm_context_length() -> usize {
        8192
    }
    fn default_store_in_git_notes() -> bool {
        true
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: Self::default_db_path(),
            models_dir: Self::default_models_dir(),
            embedding_model: Self::default_embedding_model(),
            llm_model: None,
            batch_size: Self::default_batch_size(),
            embedding_dim: Self::default_embedding_dim(),
            document_prompt_template: None,
            api_base_url: Self::default_api_base_url(),
            server_url: None,
            server_key: None,
            project_id: None,
            inference_url: None,
            plans_dir: Self::default_plans_dir(),
            specs_dir: Self::default_specs_dir(),
            llm_context_length: Self::default_llm_context_length(),
            store_in_git_notes: Self::default_store_in_git_notes(),
        }
    }
}

impl Config {
    /// Load config with layered overrides:
    ///   1. Defaults
    ///   2. `~/.config/spelunk/config.toml` (global personal)
    ///   3. `.spelunk/config.toml` discovered by walking up from CWD (project-level, team-wide)
    ///   4. Environment variables: `SPELUNK_SERVER_URL`, `SPELUNK_SERVER_KEY`, `SPELUNK_PROJECT_ID`
    ///
    /// Pass `path` to override the global config location (used by `--config` flag).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        // ── 1. Load global personal config ───────────────────────────────────
        let global_path = match path {
            Some(p) => p.to_path_buf(),
            None => spelunk_config_dir().join("config.toml"),
        };
        let mut cfg: Config = if global_path.exists() {
            let raw = std::fs::read_to_string(&global_path)
                .with_context(|| format!("reading config at {}", global_path.display()))?;
            toml::from_str(&raw).context("parsing config.toml")?
        } else {
            Config::default()
        };

        // ── 2. Merge project-level config (.spelunk/config.toml) ─────────────
        if let Ok(cwd) = std::env::current_dir()
            && let Some(proj_path) = find_project_config(&cwd)
        {
            let raw = std::fs::read_to_string(&proj_path)
                .with_context(|| format!("reading project config at {}", proj_path.display()))?;
            let proj: ProjectConfig =
                toml::from_str(&raw).context("parsing .spelunk/config.toml")?;

            // Prefer new name; fall back to deprecated alias.
            if let Some(v) = proj.server_url.or(proj.memory_server_url) {
                cfg.server_url = Some(v);
            }
            if let Some(v) = proj.server_key.or(proj.memory_server_key) {
                cfg.server_key = Some(v);
            }
            if let Some(v) = proj.project_id {
                cfg.project_id = Some(v);
            }
            if let Some(v) = proj.embedding_dim {
                cfg.embedding_dim = v;
            }
            if proj.document_prompt_template.is_some() {
                cfg.document_prompt_template = proj.document_prompt_template;
            }
        }

        // ── 3. Environment variable overrides ────────────────────────────────
        if let Ok(v) = std::env::var("SPELUNK_SERVER_URL") {
            cfg.server_url = Some(v);
        } else if let Ok(v) = std::env::var("SPELUNK_MEMORY_SERVER_URL") {
            tracing::warn!(
                "SPELUNK_MEMORY_SERVER_URL is deprecated; use SPELUNK_SERVER_URL instead"
            );
            cfg.server_url = Some(v);
        }
        if let Ok(v) = std::env::var("SPELUNK_SERVER_KEY") {
            cfg.server_key = Some(v);
        }
        if let Ok(v) = std::env::var("SPELUNK_PROJECT_ID") {
            cfg.project_id = Some(v);
        }

        Ok(cfg)
    }

    /// Validate cross-field constraints. Call after `load()`.
    ///
    /// When `server_url` points to a loopback address (`127.0.0.1`, `localhost`, `::1`),
    /// `project_id` is allowed to be absent — it will be derived at runtime by
    /// `Config::resolve_project_id()` (see spelunk#307 / section D of #303).
    pub fn validate(&self) -> Result<()> {
        if let Some(url) = &self.server_url
            && self.project_id.is_none()
            && !is_loopback_url(url)
        {
            anyhow::bail!(
                "server_url is set but project_id is missing.\n\
                 Add `project_id = \"my-project\"` to .spelunk/config.toml \
                 or set SPELUNK_PROJECT_ID."
            );
        }
        Ok(())
    }

    /// Return the effective project id.
    ///
    /// If `project_id` is set in config/env, returns it as-is.  Otherwise
    /// derives one from `project_root` via `derive_project_id()`.
    pub fn resolve_project_id(&self, project_root: &Path) -> String {
        self.project_id
            .clone()
            .unwrap_or_else(|| derive_project_id(project_root))
    }

    /// Return the URL to use for inference (embeddings + LLM), if any.
    ///
    /// Prefers `inference_url` (set for an auto-discovered loopback server,
    /// ADR-004) and falls back to `server_url` (an explicitly-configured
    /// team/remote server, which serves both inference and memory). Memory
    /// storage selection does **not** use this — see `open_memory_backend`.
    pub fn resolve_inference_url(&self) -> Option<&str> {
        self.inference_url.as_deref().or(self.server_url.as_deref())
    }
}

/// Return `true` if `url` targets a loopback address (`127.x.x.x`, `localhost`, `::1`).
///
/// This is a lightweight string check — no DNS resolution.
pub fn is_loopback_url(url: &str) -> bool {
    // Strip scheme and authority prefix up to the host.
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    // Extract the host (before any path or port).
    let host = if let Some(idx) = host_part.find('/') {
        &host_part[..idx]
    } else {
        host_part
    };
    // Drop port if present (handle IPv6 bracketed form too).
    let host = if host.starts_with('[') {
        // IPv6: [::1]:port or [::1]
        host.trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };

    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unset all spelunk-related env vars to prevent cross-test contamination.
    fn clear_spelunk_env() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_URL");
            std::env::remove_var("SPELUNK_MEMORY_SERVER_URL");
            std::env::remove_var("SPELUNK_SERVER_KEY");
            std::env::remove_var("SPELUNK_PROJECT_ID");
        }
    }

    // ── serde alias: memory_server_url → server_url ─────────────────────────

    #[test]
    #[serial_test::serial]
    fn memory_server_url_alias_loads_as_server_url() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
memory_server_url = "http://old.example.com:7777"
project_id = "my-proj"
"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://old.example.com:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("my-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn memory_server_key_alias_loads_as_server_key() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
memory_server_key = "secret-token"
project_id = "my-proj"
"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.server_key, Some("secret-token".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn loads_without_any_server_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.server_key, None);
        assert_eq!(cfg.project_id, None);
    }

    // ── validate() cross-field constraints ───────────────────────────────────

    #[test]
    fn validate_fails_when_server_url_set_without_project_id() {
        let cfg = Config {
            server_url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        let result = cfg.validate();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("server_url"));
        assert!(msg.contains("project_id"));
    }

    #[test]
    fn validate_passes_when_both_server_url_and_project_id_set() {
        let cfg = Config {
            server_url: Some("http://example.com".to_string()),
            project_id: Some("my-proj".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_when_neither_server_url_nor_project_id_set() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_when_only_project_id_set() {
        let cfg = Config {
            project_id: Some("my-proj".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    // ── validate() loopback exemption (spelunk#316) ──────────────────────────

    #[test]
    fn validate_passes_for_loopback_url_without_project_id() {
        for url in &[
            "http://127.0.0.1:7777",
            "http://localhost:7777",
            "http://127.0.0.1:7778/",
        ] {
            let cfg = Config {
                server_url: Some(url.to_string()),
                project_id: None,
                ..Default::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "expected validate() to pass for loopback URL {url}"
            );
        }
    }

    #[test]
    fn validate_fails_for_non_loopback_url_without_project_id() {
        let cfg = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    // ── is_loopback_url ──────────────────────────────────────────────────────

    #[test]
    fn is_loopback_url_recognises_127_0_0_1() {
        assert!(is_loopback_url("http://127.0.0.1:7777"));
        assert!(is_loopback_url("http://127.0.0.1:7777/"));
        assert!(is_loopback_url("http://127.0.0.1"));
    }

    #[test]
    fn is_loopback_url_recognises_localhost() {
        assert!(is_loopback_url("http://localhost:7777"));
        assert!(is_loopback_url("http://localhost"));
    }

    #[test]
    fn is_loopback_url_recognises_ipv6_loopback() {
        assert!(is_loopback_url("http://[::1]:7777"));
        assert!(is_loopback_url("http://[::1]"));
    }

    #[test]
    fn is_loopback_url_recognises_127_subnet() {
        assert!(is_loopback_url("http://127.1.2.3:7777"));
    }

    #[test]
    fn is_loopback_url_rejects_non_loopback() {
        assert!(!is_loopback_url("http://spelunk.internal:7777"));
        assert!(!is_loopback_url("http://192.168.1.100:7777"));
        assert!(!is_loopback_url("https://example.com"));
        assert!(!is_loopback_url("http://10.0.0.1"));
    }

    #[test]
    fn is_loopback_url_rejects_address_with_127_in_path() {
        // Should NOT match just because "127" appears somewhere
        assert!(!is_loopback_url("http://example.com/proxy/127.0.0.1"));
    }

    // ── normalise_git_url ────────────────────────────────────────────────────

    #[test]
    fn normalise_https_url() {
        assert_eq!(
            normalise_git_url("https://github.com/owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    #[test]
    fn normalise_scp_url() {
        assert_eq!(
            normalise_git_url("git@github.com:owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    #[test]
    fn normalise_ssh_url() {
        assert_eq!(
            normalise_git_url("ssh://git@github.com/owner/repo"),
            "github.com/owner/repo"
        );
    }

    // ── derive_project_id: no git repo → local/ fallback ─────────────────────

    #[test]
    fn derive_project_id_non_git_dir_returns_local_prefix() {
        let tmp = TempDir::new().unwrap();
        let id = derive_project_id(tmp.path());
        assert!(id.starts_with("local/"), "expected local/ prefix, got {id}");
        // blake3 hex is 64 chars
        assert_eq!(id.len(), "local/".len() + 64);
    }

    // ── derive_project_id: git repo with origin ───────────────────────────────

    #[test]
    fn derive_project_id_git_repo_with_origin() {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::write(
            repo_dir.join(".git").join("config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/spelunk-cloud/spelunk.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        // derive_project_id falls back to local/ when gix::discover fails on
        // a minimal fake repo, but the git-config parser should find the URL.
        // We test the git-config parser directly instead:
        let config = std::fs::read_to_string(repo_dir.join(".git").join("config")).unwrap();
        let url = extract_origin_url_from_git_config(&config).unwrap();
        assert_eq!(normalise_git_url(&url), "github.com/spelunk-cloud/spelunk");
    }

    // ── resolve_project_id ───────────────────────────────────────────────────

    #[test]
    fn resolve_project_id_returns_set_value_when_present() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            project_id: Some("acme/my-app".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_project_id(tmp.path()), "acme/my-app");
    }

    #[test]
    fn resolve_project_id_derives_when_unset() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config::default();
        let id = cfg.resolve_project_id(tmp.path());
        // Should be the local/ fallback since tmp dir is not a git repo.
        assert!(id.starts_with("local/"), "got {id}");
    }

    // ── resolve_inference_url (ADR-004) ──────────────────────────────────────

    #[test]
    fn resolve_inference_url_prefers_inference_url() {
        // Auto-discovered case: inference_url set, server_url unset.
        let cfg = Config {
            inference_url: Some("http://127.0.0.1:7777".to_string()),
            server_url: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    #[test]
    fn resolve_inference_url_falls_back_to_server_url() {
        // Explicit team server: only server_url set; it serves inference too.
        let cfg = Config {
            inference_url: None,
            server_url: Some("http://team.example.com:7777".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_inference_url(),
            Some("http://team.example.com:7777")
        );
    }

    #[test]
    fn resolve_inference_url_none_when_neither_set() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    fn resolve_inference_url_inference_url_wins_over_server_url() {
        // Defensive: if both are somehow set, inference must use the dedicated
        // inference_url (memory backend selection still uses server_url).
        let cfg = Config {
            inference_url: Some("http://127.0.0.1:7777".to_string()),
            server_url: Some("http://team.example.com:7777".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    // ── env var overrides ────────────────────────────────────────────────────
    //
    // Env var tests are #[serial] because they mutate process-global state.

    #[test]
    #[serial_test::serial]
    fn env_spelunk_server_url_overrides_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_url = "http://config.example.com:7777"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("SPELUNK_SERVER_URL", "http://env.example.com:7777");
        }
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://env.example.com:7777".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_server_key_overrides_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_key = "config-token"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("SPELUNK_SERVER_KEY", "env-token");
        }
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.server_key, Some("env-token".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_project_id_overrides_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"project_id = "config-proj"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("SPELUNK_PROJECT_ID", "env-proj");
        }
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.project_id, Some("env-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn env_memory_server_url_deprecated_fallback() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        // Only set the deprecated var; SPELUNK_SERVER_URL must be absent.
        unsafe {
            std::env::set_var("SPELUNK_MEMORY_SERVER_URL", "http://old.example.com:7777");
        }
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://old.example.com:7777".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_server_url_precedence_over_memory_fallback() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        unsafe {
            std::env::set_var("SPELUNK_SERVER_URL", "http://new.example.com:7777");
            std::env::set_var("SPELUNK_MEMORY_SERVER_URL", "http://old.example.com:7777");
        }
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://new.example.com:7777".to_string())
        );
    }

    // ── .spelunk/config.toml project-level merge ─────────────────────────────

    #[test]
    #[serial_test::serial]
    fn project_level_config_merges_server_url() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let spelunk_dir = proj_dir.join(".spelunk");
        std::fs::create_dir_all(&spelunk_dir).unwrap();
        std::fs::write(
            spelunk_dir.join("config.toml"),
            r#"server_url = "http://proj.example.com:7777"
project_id = "team/proj"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let original_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&proj_dir).unwrap();

        let cfg = Config::load(Some(&global_config)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://proj.example.com:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));

        if let Some(d) = original_cwd {
            std::env::set_current_dir(d).unwrap();
        }
    }

    #[test]
    #[serial_test::serial]
    fn project_level_config_accepts_memory_server_url_alias() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let spelunk_dir = proj_dir.join(".spelunk");
        std::fs::create_dir_all(&spelunk_dir).unwrap();
        std::fs::write(
            spelunk_dir.join("config.toml"),
            r#"memory_server_url = "http://old.example.com:7777"
project_id = "team/old"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let original_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&proj_dir).unwrap();

        let cfg = Config::load(Some(&global_config)).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://old.example.com:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/old".to_string()));

        if let Some(d) = original_cwd {
            std::env::set_current_dir(d).unwrap();
        }
    }
}
