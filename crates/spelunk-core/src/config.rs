use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(test)]
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Sync mode (ADR-037 D1)
// ---------------------------------------------------------------------------

/// Persistent, per-project control over where memory reads/writes go and whether
/// the CLI ever contacts the cloud (ADR-037 D1).
///
/// Replaces the implicit "is the server reachable" branch that previously drove
/// backend selection. The mode is resolved once from config + environment (see
/// [`Config::resolve_mode`]) and then gates both the capability tier probe and
/// the memory backend selector.
///
/// | mode          | reads          | writes                    | cloud contact            |
/// |---------------|----------------|---------------------------|--------------------------|
/// | `offline`     | local          | local                     | never (even if `server_url` set) |
/// | `local_first` | local          | local, then async sync    | best-effort              |
/// | `cloud_first` | cloud, local fallback | cloud, queue locally | required-ish (debug/override only) |
///
/// `cloud_first` is an **explicit debug/override mode only** — a deliberate,
/// per-invocation override of ADR-005's local-as-source-of-truth invariant. It
/// is not intended for day-to-day use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Local-only: never contacts the cloud, even when `server_url` is set.
    /// The provable no-cloud guarantee for OSS testing and air-gapped use.
    Offline,
    /// Default when `server_url` is set: reads and writes are local; a best-effort
    /// background sync converges the cloud replica. Offline-resilient.
    LocalFirst,
    /// Debug/override only: reads prefer the cloud (local fallback) and writes go
    /// to the cloud (queued locally if unreachable). Not for day-to-day use.
    CloudFirst,
}

impl SyncMode {
    /// Parse a mode from its serialized string form (case-insensitive).
    ///
    /// Accepts `offline`, `local_first`, and `cloud_first`. Returns `None` for
    /// any other value so callers can decide how to handle an invalid override.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "offline" => Some(Self::Offline),
            "local_first" | "local-first" | "localfirst" => Some(Self::LocalFirst),
            "cloud_first" | "cloud-first" | "cloudfirst" => Some(Self::CloudFirst),
            _ => None,
        }
    }

    /// String form used in config files and `SPELUNK_MODE`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::LocalFirst => "local_first",
            Self::CloudFirst => "cloud_first",
        }
    }

    /// Whether this mode permits any contact with the cloud server.
    ///
    /// `offline` never contacts the cloud; the other two may. Used by the
    /// capability tier probe and the memory backend selector to honour the
    /// kill-switch semantics of `offline`.
    pub fn allows_cloud(&self) -> bool {
        !matches!(self, Self::Offline)
    }
}

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

    /// Bearer token for cloud/spelunk-server auth — the single token every auth
    /// path sends as `Authorization: Bearer …`.
    /// Set in `~/.config/spelunk/config.toml` (personal), written by
    /// `spelunk login`, or overridden via `SPELUNK_SERVER_KEY`.
    /// Do NOT commit this to `.spelunk/config.toml`.
    /// The old `memory_server_key` TOML key is accepted as a backward-compat alias.
    #[serde(default, alias = "memory_server_key")]
    pub server_key: Option<String>,

    /// Project slug for the spelunk-server (e.g. `acme/my-app`).
    /// Required when `server_url` is set.
    /// Set in `.spelunk/config.toml` (project-level) or via `SPELUNK_PROJECT_ID`.
    #[serde(default)]
    pub project_id: Option<String>,

    /// Sync mode (ADR-037 D1): `offline` / `local_first` / `cloud_first`.
    ///
    /// Stored as `Option` so the serde default can preserve today's behaviour:
    /// when absent, [`Config::resolve_mode`] derives the effective mode from
    /// `server_url` (no `server_url` ⇒ `offline`; `server_url` present ⇒
    /// `local_first`). An explicit value here pins the mode; `SPELUNK_MODE`
    /// overrides it, and `SPELUNK_NO_SERVER=1` forces `offline` regardless.
    /// Always read it through [`Config::resolve_mode`], never directly.
    #[serde(default)]
    pub mode: Option<SyncMode>,

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

    /// WorkOS device-flow tokens persisted by `spelunk login`, stored under the
    /// `[auth]` table in the global config.
    ///
    /// When present and the access token is unexpired, it is the source of the
    /// `Authorization: Bearer` token every cloud request sends — [`Config::load`]
    /// copies the access token into [`Config::server_key`] so existing call
    /// sites keep working unchanged. The `refresh_token` is used to rotate an
    /// expired access token and to silently switch organisations. Read the
    /// effective bearer through [`Config::server_key`]; reach for the refresh
    /// token via this field only in the token-refresh path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthTokens>,
}

/// WorkOS tokens persisted under the `[auth]` table of the global config.
///
/// Written by `spelunk login` / `spelunk org switch`; rotated by the token
/// refresh path. The file is written `0600` (see [`save_auth_tokens_to`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokens {
    /// Short-lived WorkOS access token, sent as `Authorization: Bearer`.
    pub access_token: String,
    /// Long-lived rotating refresh token. Exchanged at `/v1/auth/token` to
    /// rotate the access token or switch organisation.
    pub refresh_token: String,
    /// Absolute expiry of `access_token`, as a Unix timestamp (seconds).
    pub expires_at: i64,
    /// WorkOS organisation the tokens are scoped to.
    pub org_id: String,
}

impl AuthTokens {
    /// Whether the access token is at or past its expiry, with a small skew
    /// margin so a token that is about to expire is refreshed pre-emptively
    /// rather than failing mid-request.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(chrono::Utc::now().timestamp())
    }

    /// Expiry check against an explicit `now` (Unix seconds) — testable form of
    /// [`AuthTokens::is_expired`]. Treats the token as expired 30 s early.
    pub fn is_expired_at(&self, now: i64) -> bool {
        const SKEW_SECS: i64 = 30;
        now >= self.expires_at - SKEW_SECS
    }
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
            api_base_url: Self::default_api_base_url(),
            server_url: None,
            server_key: None,
            project_id: None,
            mode: None,
            inference_url: None,
            plans_dir: Self::default_plans_dir(),
            specs_dir: Self::default_specs_dir(),
            llm_context_length: Self::default_llm_context_length(),
            store_in_git_notes: Self::default_store_in_git_notes(),
            auth: None,
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

        // Legacy bare `server_key` after the global + project-level merges but
        // before env / `[auth]` resolution. Used as the lowest-precedence
        // fallback so pre-WorkOS users (and team `.spelunk/config.toml` keys)
        // keep working until they re-run `spelunk login`.
        let legacy_server_key = cfg.server_key.clone();

        // ── 3. Environment variable overrides ────────────────────────────────
        if let Ok(v) = std::env::var("SPELUNK_SERVER_URL") {
            cfg.server_url = Some(v);
        } else if let Ok(v) = std::env::var("SPELUNK_MEMORY_SERVER_URL") {
            tracing::warn!(
                "SPELUNK_MEMORY_SERVER_URL is deprecated; use SPELUNK_SERVER_URL instead"
            );
            cfg.server_url = Some(v);
        }
        let env_server_key = std::env::var("SPELUNK_SERVER_KEY").ok();
        if let Some(v) = &env_server_key {
            cfg.server_key = Some(v.clone());
        }
        if let Ok(v) = std::env::var("SPELUNK_PROJECT_ID") {
            cfg.project_id = Some(v);
        }
        // ADR-037 D1: SPELUNK_MODE overrides the configured sync mode. An
        // unrecognised value is a hard error — silently falling back to a
        // default would defeat the deterministic-mode guarantee the Founder
        // needs to separate OSS-local test runs from cloud dogfood runs.
        if let Ok(v) = std::env::var("SPELUNK_MODE") {
            let parsed = SyncMode::parse(&v).with_context(|| {
                format!(
                    "SPELUNK_MODE={v:?} is not a valid sync mode \
                     (expected one of: offline, local_first, cloud_first)"
                )
            })?;
            cfg.mode = Some(parsed);
        }

        // ── 4. Resolve the effective Bearer token (ADR-045) ──────────────────
        // Precedence for `Authorization: Bearer`:
        //   1. `SPELUNK_SERVER_KEY` env var (CI / explicit override) — wins.
        //   2. `[auth].access_token` from `spelunk login` (WorkOS device flow).
        //   3. Legacy bare `server_key` (pre-WorkOS users keep working until
        //      they re-run `spelunk login`).
        // The `[auth]` tokens are kept in `cfg.auth` so the refresh-on-expiry /
        // org-switch paths can reach the refresh token; every other call site
        // only ever reads the resolved `cfg.server_key`.
        if env_server_key.is_none() {
            if let Some(auth) = &cfg.auth {
                cfg.server_key = Some(auth.access_token.clone());
            } else {
                cfg.server_key = legacy_server_key;
            }
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

    /// Resolve the effective sync mode (ADR-037 D1).
    ///
    /// Precedence (highest first):
    /// 1. `SPELUNK_NO_SERVER=1` (or `true`/`yes`) → [`SyncMode::Offline`] — a hard
    ///    kill-switch that wins over everything else.
    /// 2. An explicit `mode` in config / `SPELUNK_MODE` (already folded into
    ///    `self.mode` by [`Config::load`]).
    /// 3. Serde default: no `server_url` ⇒ [`SyncMode::Offline`]; `server_url`
    ///    present ⇒ [`SyncMode::LocalFirst`]. This preserves today's behaviour
    ///    for configs written before this field existed.
    ///
    /// This is the single source of truth for the mode — backend selection and
    /// the tier probe both call it rather than reading `self.mode` directly.
    pub fn resolve_mode(&self) -> SyncMode {
        if no_server_env_set() {
            return SyncMode::Offline;
        }
        if let Some(mode) = self.mode {
            return mode;
        }
        // Default mirrors the pre-ADR-037 implicit behaviour.
        if self.server_url.is_some() {
            SyncMode::LocalFirst
        } else {
            SyncMode::Offline
        }
    }
}

/// Write (or update) `server_key` in `~/.config/spelunk/config.toml`.
///
/// This is the token `spelunk login` persists. Uses a line-level
/// read-modify-write so that other keys in the file are preserved.  The file is
/// created (with the `server_key` line) if absent.
///
/// The value is **not** shell-quoted before writing — it is written as a bare
/// TOML string with double quotes, e.g. `server_key = "sk-sp-…"`.
pub fn save_server_key(key: &str) -> Result<()> {
    save_server_key_to(key, &spelunk_config_dir().join("config.toml"))
}

/// Same as [`save_server_key`] but writes to an explicit path (useful in tests).
pub fn save_server_key_to(key: &str, config_path: &Path) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }

    let existing = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?
    } else {
        String::new()
    };

    let new_line = format!("server_key = {}\n", toml_quote(key));
    let updated = upsert_toml_line(&existing, "server_key", &new_line);

    std::fs::write(config_path, updated)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok(())
}

/// Remove `server_key` from `~/.config/spelunk/config.toml`.
///
/// This is what `spelunk logout` clears. No-op if the file does not exist or the
/// key is absent.  Other keys are preserved.
pub fn remove_server_key() -> Result<()> {
    remove_server_key_from(&spelunk_config_dir().join("config.toml"))
}

/// Same as [`remove_server_key`] but operates on an explicit path (useful in tests).
pub fn remove_server_key_from(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;

    let updated: String = existing
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Keep every line that is NOT a `server_key = …` assignment.
            // strip_prefix avoids a raw byte-index slice and handles the
            // "server_key" prefix unambiguously.
            let after_key = trimmed.strip_prefix("server_key");
            !matches!(after_key, Some(rest) if rest.trim_start().starts_with('='))
        })
        .map(|line| format!("{line}\n"))
        .collect();

    std::fs::write(config_path, updated)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// `[auth]` table persistence (WorkOS device-flow tokens, ADR-045)
// ───────────────────────────────────────────────────────────────────────────

/// Persist WorkOS tokens to the `[auth]` table of `~/.config/spelunk/config.toml`.
///
/// Replaces any existing `[auth]` table; all other top-level keys and tables
/// are preserved. The file is written with `0600` permissions so the refresh
/// token is not world-readable.
pub fn save_auth_tokens(tokens: &AuthTokens) -> Result<()> {
    save_auth_tokens_to(tokens, &spelunk_config_dir().join("config.toml"))
}

/// Same as [`save_auth_tokens`] but writes to an explicit path (useful in tests).
pub fn save_auth_tokens_to(tokens: &AuthTokens, config_path: &Path) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }

    let mut doc = read_config_table(config_path)?;
    let auth_value = toml::Value::try_from(tokens).context("serialising auth tokens")?;
    doc.insert("auth".to_string(), auth_value);

    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    write_config_secure(config_path, &serialised)
}

/// Remove the `[auth]` table from `~/.config/spelunk/config.toml`.
///
/// What `spelunk logout` clears (alongside the legacy `server_key`). No-op if
/// the file or the table is absent. Other keys are preserved.
pub fn remove_auth_tokens() -> Result<()> {
    remove_auth_tokens_from(&spelunk_config_dir().join("config.toml"))
}

/// Same as [`remove_auth_tokens`] but operates on an explicit path (tests).
pub fn remove_auth_tokens_from(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let mut doc = read_config_table(config_path)?;
    if doc.remove("auth").is_none() {
        return Ok(());
    }
    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    write_config_secure(config_path, &serialised)
}

/// Parse the config file into a `toml::Table`, returning an empty table when the
/// file does not exist.
fn read_config_table(config_path: &Path) -> Result<toml::Table> {
    if !config_path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    raw.parse::<toml::Table>()
        .with_context(|| format!("parsing {}", config_path.display()))
}

/// Write `contents` to `config_path` and tighten permissions to `0600` on Unix
/// so secrets in the file are owner-only.
fn write_config_secure(config_path: &Path, contents: &str) -> Result<()> {
    std::fs::write(config_path, contents)
        .with_context(|| format!("writing {}", config_path.display()))?;
    set_owner_only_permissions(config_path)?;
    Ok(())
}

/// Set `0600` permissions on Unix; a no-op on other platforms.
#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Wrap a string value in TOML double-quote syntax, escaping `\` and `"`.
fn toml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Insert or replace the line that sets `key` in a TOML file body.
///
/// Only handles top-level bare-key assignments (`key = …`).  Table sections
/// are left untouched — the function scans for a line starting with `key`
/// followed by optional whitespace and `=`.  If found, it is replaced; if not
/// found, the new line is appended.
fn upsert_toml_line(content: &str, key: &str, new_line: &str) -> String {
    let mut found = false;
    let mut result: String = content
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            // Match `key = …` at the top level (no table-section header).
            let is_match = trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='));
            if is_match {
                found = true;
                new_line.to_string()
            } else {
                format!("{line}\n")
            }
        })
        .collect();

    if !found {
        // Ensure there is a trailing newline before appending.
        if !result.ends_with('\n') && !result.is_empty() {
            result.push('\n');
        }
        result.push_str(new_line);
    }
    result
}

/// Returns `true` when `SPELUNK_NO_SERVER` is set to a truthy value.
///
/// This is the hard offline kill-switch shared by [`Config::resolve_mode`] and
/// the CLI capability probe; both must agree on what "no server" means.
pub fn no_server_env_set() -> bool {
    matches!(
        std::env::var("SPELUNK_NO_SERVER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Return `true` if `s` parses as a canonical UUID (any version).
///
/// Used by the cloud-api slug→UUID resolution path (ADR-005): a `project_id`
/// that is already a UUID is used directly against `/v1/projects/{uuid}/…`,
/// while a non-UUID value is treated as a human slug and resolved via
/// `GET /v1/projects`.
pub fn looks_like_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
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
            std::env::remove_var("SPELUNK_MODE");
            std::env::remove_var("SPELUNK_NO_SERVER");
        }
    }

    // ── SyncMode parse / as_str (ADR-037 D1) ─────────────────────────────────

    #[test]
    fn sync_mode_parse_accepts_canonical_and_variant_forms() {
        assert_eq!(SyncMode::parse("offline"), Some(SyncMode::Offline));
        assert_eq!(SyncMode::parse("LOCAL_FIRST"), Some(SyncMode::LocalFirst));
        assert_eq!(SyncMode::parse("local-first"), Some(SyncMode::LocalFirst));
        assert_eq!(SyncMode::parse("cloud_first"), Some(SyncMode::CloudFirst));
        assert_eq!(SyncMode::parse(" cloudfirst "), Some(SyncMode::CloudFirst));
        assert_eq!(SyncMode::parse("bogus"), None);
    }

    #[test]
    fn sync_mode_as_str_round_trips() {
        for m in [
            SyncMode::Offline,
            SyncMode::LocalFirst,
            SyncMode::CloudFirst,
        ] {
            assert_eq!(SyncMode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn sync_mode_allows_cloud() {
        assert!(!SyncMode::Offline.allows_cloud());
        assert!(SyncMode::LocalFirst.allows_cloud());
        assert!(SyncMode::CloudFirst.allows_cloud());
    }

    #[test]
    fn sync_mode_serde_snake_case() {
        // Serialised form must be snake_case so config.toml / wire stay stable.
        let json = serde_json::to_string(&SyncMode::LocalFirst).unwrap();
        assert_eq!(json, "\"local_first\"");
        let parsed: SyncMode = serde_json::from_str("\"cloud_first\"").unwrap();
        assert_eq!(parsed, SyncMode::CloudFirst);
    }

    // ── resolve_mode defaults (ADR-037 D1) ───────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn resolve_mode_defaults_offline_without_server_url() {
        clear_spelunk_env();
        let cfg = Config::default();
        assert_eq!(cfg.resolve_mode(), SyncMode::Offline);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_defaults_local_first_with_server_url() {
        clear_spelunk_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_explicit_mode_wins_over_default() {
        clear_spelunk_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::CloudFirst);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_no_server_env_forces_offline() {
        clear_spelunk_env();
        // Even an explicit cloud_first mode is overridden by the kill-switch.
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
        assert_eq!(cfg.resolve_mode(), SyncMode::Offline);
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_mode_overrides_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "mode = \"offline\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_MODE", "cloud_first") };
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::CloudFirst));
        unsafe { std::env::remove_var("SPELUNK_MODE") };
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_mode_invalid_is_hard_error() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        unsafe { std::env::set_var("SPELUNK_MODE", "sideways") };
        let err = Config::load(Some(&config_path)).unwrap_err();
        assert!(err.to_string().contains("SPELUNK_MODE"));
        unsafe { std::env::remove_var("SPELUNK_MODE") };
    }

    #[test]
    #[serial_test::serial]
    fn config_toml_mode_parses() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "mode = \"local_first\"\n").unwrap();
        let cfg = Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::LocalFirst));
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

    // ── looks_like_uuid (ADR-005) ────────────────────────────────────────────

    #[test]
    fn looks_like_uuid_accepts_canonical_uuids() {
        assert!(looks_like_uuid("018f4e2a-1234-7abc-8def-000000000001"));
        assert!(looks_like_uuid("00000000-0000-0000-0000-000000000000"));
        // uppercase hex is valid
        assert!(looks_like_uuid("018F4E2A-1234-7ABC-8DEF-000000000001"));
    }

    #[test]
    fn looks_like_uuid_rejects_slugs() {
        assert!(!looks_like_uuid("spelunk"));
        assert!(!looks_like_uuid("acme/my-app"));
        assert!(!looks_like_uuid("local/9f2a8b3c4d5e6f70"));
        assert!(!looks_like_uuid(""));
        // a UUID missing a section is not a UUID
        assert!(!looks_like_uuid("018f4e2a-1234-7abc-8def"));
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

    // ── [auth] WorkOS tokens (ADR-045) ───────────────────────────────────────

    fn sample_tokens() -> AuthTokens {
        AuthTokens {
            access_token: "at-sample".to_string(),
            refresh_token: "rt-sample".to_string(),
            expires_at: 4_000_000_000,
            org_id: "org_sample".to_string(),
        }
    }

    /// Persisted `[auth]` tokens round-trip and the access token becomes the
    /// effective `server_key` bearer.
    #[test]
    #[serial_test::serial]
    fn auth_tokens_resolve_to_server_key_bearer() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
        let auth = cfg.auth.expect("auth table should load");
        assert_eq!(auth.refresh_token, "rt-sample");
        assert_eq!(auth.org_id, "org_sample");
    }

    /// `SPELUNK_SERVER_KEY` (CI) overrides the `[auth]` access token.
    #[test]
    #[serial_test::serial]
    fn env_server_key_wins_over_auth_tokens() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        unsafe { std::env::set_var("SPELUNK_SERVER_KEY", "ci-token") };
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("ci-token"));
        // The refresh token is still available for the refresh path.
        assert_eq!(cfg.auth.unwrap().refresh_token, "rt-sample");
        unsafe { std::env::remove_var("SPELUNK_SERVER_KEY") };
    }

    /// A legacy bare `server_key` keeps working when no `[auth]` table exists.
    #[test]
    #[serial_test::serial]
    fn legacy_server_key_used_when_no_auth_table() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();

        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("sk-legacy"));
        assert!(cfg.auth.is_none());
    }

    /// `[auth]` access token takes precedence over a legacy bare `server_key`
    /// present in the same file.
    #[test]
    #[serial_test::serial]
    fn auth_token_precedence_over_legacy_server_key() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
    }

    /// Writing auth tokens preserves other top-level keys (e.g. `server_url`).
    #[test]
    #[serial_test::serial]
    fn save_auth_tokens_preserves_other_keys() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_url = \"http://team.example:7777\"\n").unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_url.as_deref(), Some("http://team.example:7777"));
        assert_eq!(cfg.auth.unwrap().access_token, "at-sample");
    }

    /// `remove_auth_tokens_from` clears the `[auth]` table but leaves the legacy
    /// `server_key` (logout clears that separately).
    #[test]
    #[serial_test::serial]
    fn remove_auth_tokens_clears_only_auth_table() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        remove_auth_tokens_from(&path).unwrap();
        let cfg = Config::load(Some(&path)).unwrap();
        assert!(cfg.auth.is_none());
        // Legacy key still present and now resolves as the bearer fallback.
        assert_eq!(cfg.server_key.as_deref(), Some("sk-legacy"));
    }

    /// `remove_auth_tokens_from` is a no-op when the file is missing.
    #[test]
    fn remove_auth_tokens_no_op_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        remove_auth_tokens_from(&path).unwrap();
    }

    /// On Unix, the config file is written `0600` after persisting tokens.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn save_auth_tokens_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config must be owner-only after save");
    }

    /// Expiry uses a 30 s skew margin.
    #[test]
    fn auth_tokens_expiry_with_skew() {
        let t = sample_tokens(); // expires_at = 4_000_000_000
        assert!(!t.is_expired_at(4_000_000_000 - 31));
        assert!(t.is_expired_at(4_000_000_000 - 30));
        assert!(t.is_expired_at(4_000_000_000 + 100));
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
