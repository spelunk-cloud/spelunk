use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(test)]
use tempfile::TempDir;

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
    fn default_plans_dir() -> PathBuf {
        PathBuf::from("docs/plans")
    }
    fn default_specs_dir() -> PathBuf {
        PathBuf::from("docs/specs")
    }
    fn default_llm_context_length() -> usize {
        8192
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
            api_base_url: Self::default_api_base_url(),
            server_url: None,
            server_key: None,
            project_id: None,
            plans_dir: Self::default_plans_dir(),
            specs_dir: Self::default_specs_dir(),
            llm_context_length: Self::default_llm_context_length(),
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

    /// Whether a local embedding model is configured for this CLI.
    ///
    /// A local embedder is considered "configured" when the OpenAI-compatible
    /// `api_base_url` has been set to a non-default endpoint (e.g. the user
    /// pointed spelunk at their LM Studio / Ollama / vLLM server). On a fresh
    /// install `api_base_url` keeps its built-in default, so this returns
    /// `false` and `open_memory_backend()` falls through to the zero-infra
    /// git-meta backend (see decision #73). Used by `open_memory_backend()` to
    /// decide between the SQLite (local semantic) and git-meta backends.
    pub fn local_embedder_configured(&self) -> bool {
        self.api_base_url != Self::default_api_base_url()
    }

    /// Validate cross-field constraints. Call after `load()`.
    pub fn validate(&self) -> Result<()> {
        if self.server_url.is_some() && self.project_id.is_none() {
            anyhow::bail!(
                "server_url is set but project_id is missing.\n\
                 Add `project_id = \"my-project\"` to .spelunk/config.toml \
                 or set SPELUNK_PROJECT_ID."
            );
        }
        Ok(())
    }
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
