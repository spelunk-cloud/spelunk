use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod paths;
mod persist;
mod predicates;
mod project_id;
mod sync_mode;
mod tls;

pub mod llm_key;
pub mod secret_store;
pub mod server_keys;

use paths::{find_project_config, spelunk_config_dir};
use secret_store::{KEY_SERVER_KEY, SecretStore};

pub use paths::{
    find_project_db, find_project_dir, require_project_db, require_project_db_at, resolve_db,
};
pub use persist::{
    remove_auth_tokens, remove_auth_tokens_from, remove_server_key, remove_server_key_from,
    remove_server_key_with, save_auth_tokens, save_auth_tokens_to, save_server_key,
    save_server_key_with, write_project_slug,
};
pub use predicates::{
    is_loopback_url, is_loopback_url_missing_port, no_server_env_set, validate_transport_url,
};
pub use project_id::derive_project_id;
pub use sync_mode::SyncMode;
pub use tls::apply_server_ca;

#[cfg(test)]
use tempfile::TempDir;

/// Serde default helper: `true`.
fn default_true() -> bool {
    true
}

/// The `[index]` config table: controls the built-in index-time file filter that
/// skips generated/vendored/minified/machine-data files (see
/// `spelunk_core::indexer::filter`). Distinct from the unconditional
/// sensitive-file exclusion (`.env`, keys), which is not configurable here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Extra gitignore-syntax exclude lines layered on top of the built-ins.
    /// A `!pattern` line re-includes a path the defaults would drop (last match
    /// wins). Cannot re-include a sensitive file (that layer is separate).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whether to apply the built-in default exclude set. Default `true`.
    #[serde(default = "default_true")]
    pub use_default_excludes: bool,
    /// Whether to skip files whose head self-declares as generated
    /// (`@generated` or `// Code generated ... DO NOT EDIT.`). Default `true`.
    #[serde(default = "default_true")]
    pub detect_generated: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            use_default_excludes: true,
            detect_generated: true,
        }
    }
}

/// Per-field override of [`IndexConfig`] from a project `.spelunk/config.toml`.
/// Every field is `Option` so an absent key leaves the layered value untouched.
#[derive(Debug, Default, Deserialize)]
struct ProjectIndexConfig {
    exclude: Option<Vec<String>>,
    use_default_excludes: Option<bool>,
    detect_generated: Option<bool>,
}

/// Fields that can be set in `.spelunk/config.toml` (project-level, checked-in).
/// Only contains fields safe to share with the team (no secrets).
///
/// `server_key` is deliberately absent (ADR-071 D4): a credential in a
/// committed file is in the repo's history for good and readable by anyone
/// with repo access. A file that still has a `server_key` line keeps working
/// for its other fields: serde silently drops the unrecognized key, the
/// same way the removed `memory_server_*` aliases are dropped. Use
/// `spelunk auth set-key --server <url>` instead.
#[derive(Debug, Default, Deserialize)]
struct ProjectConfig {
    /// Canonical server URL (preferred).
    server_url: Option<String>,
    project_id: Option<String>,
    /// Path to a PEM CA bundle to trust in addition to the built-in roots, for a
    /// team server presenting a self-signed / internal-CA certificate.
    server_ca: Option<String>,
    /// `[index]` table: per-field override of the built-in file filter.
    index: Option<ProjectIndexConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the SQLite database file
    #[serde(default = "Config::default_db_path")]
    pub db_path: PathBuf,

    /// Chat model id, resolved by spelunk-server, for `ask` and `memory harvest`.
    /// When unset, commands that require a chat model are unavailable.
    /// `SPELUNK_LLM_MODEL` overrides this.
    #[serde(default)]
    pub llm_model: Option<String>,

    /// Base URL of an OpenAI-compatible chat completions endpoint (a local
    /// LM Studio / Ollama, or a self-hosted gateway), passed on to the
    /// auto-spawned `spelunk-server` so it gains LLM capability.
    ///
    /// Personal config (`~/.config/spelunk/config.toml`) or `SPELUNK_LLM_URL`
    /// only, never `.spelunk/config.toml`: a checked-in endpoint points the
    /// whole team at one developer's machine, and it is the natural sibling of
    /// the LLM credential that `ProjectConfig` already excludes (ADR-071 D4).
    #[serde(default)]
    pub llm_url: Option<String>,

    // ── spelunk-server (optional) ─────────────────────────────────────────────
    /// URL of the spelunk-server instance, e.g. `https://spelunk.internal.example.com`
    /// (or `http://127.0.0.1:7777` for loopback; non-loopback `http://` is rejected).
    /// When set, the CLI operates in Tier 1 (server-connected) mode, enabling
    /// semantic search, embedding, and explore.
    /// Set in `.spelunk/config.toml` (project-level) or via `SPELUNK_SERVER_URL` only:
    /// [`Config::load_with_store`] discards any value from the global personal
    /// config, since a team server is a project-wide choice, not a
    /// per-developer one.
    #[serde(default)]
    pub server_url: Option<String>,

    /// The **cloud-kind** bearer only (ADR-071 D2): `SPELUNK_SERVER_KEY` env
    /// override, else the `[auth].access_token` written by `spelunk login`.
    /// Resolved once at load time because both tiers are origin-independent.
    ///
    /// This is NOT the effective bearer for a self-hosted `server_url`:
    /// that credential is scoped per server origin and resolved lazily via
    /// [`Config::bearer_for`], which also branches to this same field for the
    /// cloud origin. Do NOT commit any bearer to `.spelunk/config.toml`.
    #[serde(default)]
    pub server_key: Option<String>,

    /// Project slug for the spelunk-server (e.g. `acme/my-app`).
    /// Required when `server_url` is set.
    /// Set in `.spelunk/config.toml` (project-level) or via `SPELUNK_PROJECT_ID`.
    #[serde(default)]
    pub project_id: Option<String>,

    /// Path to a PEM CA bundle trusted (in addition to the built-in roots) when
    /// connecting to a team `server_url` whose certificate is signed by a
    /// self-signed or internal CA. Verification stays ON — this only adds a
    /// trust anchor, it does not disable checks.
    /// `SPELUNK_SERVER_CA` overrides this; set in either config file.
    #[serde(default)]
    pub server_ca: Option<String>,

    /// Sync mode: `offline` / `local_first` / `cloud_first`.
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
    /// `Authorization: Bearer` token every cloud-origin request sends.
    /// [`Config::load`] copies the access token into [`Config::server_key`]
    /// (the cloud-kind bearer). A self-hosted `server_url` never consults
    /// this field (ADR-071 D2); use [`Config::bearer_for`] there. The
    /// `refresh_token` is used to rotate an expired access token and to
    /// silently switch organisations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthTokens>,

    /// `[index]` table: built-in index-time file filter settings. Project
    /// `.spelunk/config.toml` overrides the global value per field (see
    /// [`Config::load_with_store`]).
    #[serde(default)]
    pub index: IndexConfig,
}

/// WorkOS tokens persisted under the `[auth]` table of the global config.
///
/// Written by `spelunk login` / `spelunk org switch`; rotated by the token
/// refresh path. The file is written `0600` (see [`save_auth_tokens_to`]).
///
/// Every field is `#[serde(default)]`, so a partial `[auth]` table never fails
/// the whole config load. `--org` is a documented optional scoping flag and
/// hand-editing the config is a documented workflow, so a login without an org
/// (no `org_id`) or a trimmed table must not brick commands that need no
/// credentials. An absent field is read as its "unset" form, which the
/// consumers already treat sensibly:
/// * missing `access_token` ⇒ empty ⇒ not logged in (no bearer resolved, see
///   [`Config::load_with_store_from`] and [`server_keys::bearer_for`]);
/// * missing `expires_at` ⇒ `0` ⇒ [`AuthTokens::is_expired_at`] reports
///   expired, so an unknown expiry can never read as "still valid";
/// * missing `org_id` ⇒ empty ⇒ no organisation scoping (the same empty-string
///   sentinel the login/refresh paths already use).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokens {
    /// Short-lived WorkOS access token, sent as `Authorization: Bearer`.
    /// Empty (or absent) means "not logged in": no bearer is resolved.
    #[serde(default)]
    pub access_token: String,
    /// Long-lived rotating refresh token. Exchanged directly at WorkOS
    /// `/user_management/authenticate` (refresh grant) to rotate the
    /// access token or switch organisation. Empty when the table predates a
    /// login or was hand-trimmed; the refresh path then simply cannot rotate.
    #[serde(default)]
    pub refresh_token: String,
    /// Absolute expiry of `access_token`, as a Unix timestamp (seconds).
    /// Absent ⇒ `0`, which [`AuthTokens::is_expired_at`] treats as expired.
    #[serde(default)]
    pub expires_at: i64,
    /// WorkOS organisation the tokens are scoped to. Empty when logged in
    /// without an org (`--org` omitted); no scoping is applied.
    #[serde(default)]
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
    ///
    /// A missing `expires_at` deserialises to `0`, so this reports expired for
    /// any realistic `now`: an unknown expiry is never read as "still valid".
    pub fn is_expired_at(&self, now: i64) -> bool {
        const SKEW_SECS: i64 = 30;
        now >= self.expires_at - SKEW_SECS
    }
}

impl Config {
    fn default_db_path() -> PathBuf {
        spelunk_config_dir().join("index.db")
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
            llm_model: None,
            llm_url: None,
            server_url: None,
            server_key: None,
            project_id: None,
            server_ca: None,
            mode: None,
            inference_url: None,
            llm_context_length: Self::default_llm_context_length(),
            store_in_git_notes: Self::default_store_in_git_notes(),
            auth: None,
            index: IndexConfig::default(),
        }
    }
}

impl Config {
    /// Cheaply check whether the personal config sets `llm_model`, without
    /// resolving the bearer credential or touching the secret store.
    ///
    /// Callers that only need this one field (the CLI's pre-parse help gate,
    /// run ahead of and in addition to the real [`Config::load`]) must not pay
    /// for a full load, which pulls the secret store into the process for a
    /// value they never use.
    pub fn llm_model_configured(path: Option<&Path>) -> bool {
        let global_path = match path {
            Some(p) => p.to_path_buf(),
            None => spelunk_config_dir().join("config.toml"),
        };
        let Ok(raw) = std::fs::read_to_string(&global_path) else {
            return false;
        };
        toml::from_str::<Config>(&raw)
            .map(|c| c.llm_model.is_some())
            .unwrap_or(false)
    }

    /// Load config with layered overrides:
    ///   1. Defaults
    ///   2. `~/.config/spelunk/config.toml` (global personal)
    ///   3. `.spelunk/config.toml` discovered by walking up from CWD (project-level, team-wide)
    ///   4. Environment variables: `SPELUNK_SERVER_URL`, `SPELUNK_SERVER_KEY`, `SPELUNK_PROJECT_ID`
    ///
    /// `server_url` is the one field step 2 is not allowed to set (see the
    /// `server_url` field doc): a team server is a project-wide decision, so
    /// only the checked-in project config or an explicit env var may supply
    /// it, never a single developer's personal file.
    ///
    /// Pass `path` to override the global config location (used by `--config` flag).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let store = secret_store::default_store(&spelunk_config_dir())?;
        Self::load_with_store(path, store.as_ref())
    }

    /// Same as [`Config::load`] but with an injected [`SecretStore`].
    ///
    /// Tests pass an in-memory store so the credential resolution + migration
    /// paths can be exercised without a real keychain or daemon. Production code
    /// calls [`Config::load`], which resolves the host's default store.
    pub fn load_with_store(path: Option<&Path>, store: &dyn SecretStore) -> Result<Self> {
        let project_root = std::env::current_dir().ok();
        Self::load_with_store_from(path, store, project_root.as_deref())
    }

    /// Like [`load_with_store`], but the project-level `.spelunk/config.toml` is
    /// discovered by walking up from `project_root` rather than the process CWD.
    /// `None` skips project discovery entirely: a `.spelunk/config.toml` checked
    /// in to this repo must not leak into a hermetic unit-test load. Production
    /// always passes the CWD via [`load_with_store`].
    pub(crate) fn load_with_store_from(
        path: Option<&Path>,
        store: &dyn SecretStore,
        project_root: Option<&Path>,
    ) -> Result<Self> {
        // ── 1. Load global personal config ───────────────────────────────────
        let global_path = match path {
            Some(p) => p.to_path_buf(),
            None => spelunk_config_dir().join("config.toml"),
        };
        let mut cfg: Config = if global_path.exists() {
            let raw = std::fs::read_to_string(&global_path)
                .with_context(|| format!("reading config at {}", global_path.display()))?;
            parse_global_config(&raw, &global_path)?
        } else {
            Config::default()
        };
        // A personal global config must never be able to point the CLI at a
        // team server on its own: everyone on a project needs the same
        // server_url, which only the checked-in project config or an env var
        // can guarantee. Discard whatever the global file set here; step 2
        // below is the only file-based source allowed to populate it.
        cfg.server_url = None;

        // A bare `server_key` in the *personal* global config is the legacy
        // plaintext credential we migrate into the secret store.
        // Captured before the project-level merge so we never migrate a shared
        // team key from a checked-in `.spelunk/config.toml`.
        let global_bare_server_key = cfg.server_key.clone();

        // ── 2. Merge project-level config (.spelunk/config.toml) ─────────────
        // `ProjectConfig` has no `server_key` field (ADR-071 D4): a checked-in
        // file never carries a credential. A file that still has a
        // `server_key` line keeps working for its other fields: the parse
        // above silently drops the unrecognized key.
        if let Some(root) = project_root
            && let Some(proj_path) = find_project_config(root)
        {
            let raw = std::fs::read_to_string(&proj_path)
                .with_context(|| format!("reading project config at {}", proj_path.display()))?;
            let proj: ProjectConfig =
                toml::from_str(&raw).map_err(|e| config_parse_error(&proj_path, e))?;

            if let Some(v) = proj.server_url {
                cfg.server_url = Some(v);
            }
            if let Some(v) = proj.project_id {
                cfg.project_id = Some(v);
            }
            if let Some(v) = proj.server_ca {
                cfg.server_ca = Some(v);
            }
            // `[index]` overrides the global value per field: an absent key in the
            // project table leaves the global (or default) value in place.
            if let Some(pidx) = proj.index {
                if let Some(v) = pidx.exclude {
                    cfg.index.exclude = v;
                }
                if let Some(v) = pidx.use_default_excludes {
                    cfg.index.use_default_excludes = v;
                }
                if let Some(v) = pidx.detect_generated {
                    cfg.index.detect_generated = v;
                }
            }
        }

        // ── 2b. One-time migration of the legacy plaintext bare key ──────────
        // If the personal config still carries a bare `server_key`, move it into
        // the secret store and strip it from the file (transparent, one-time).
        // Skip when the store already has one (already migrated / freshly logged
        // in) to avoid clobbering a rotated credential with a stale file value.
        if let Some(bare) = &global_bare_server_key {
            let already_in_store = store.get(KEY_SERVER_KEY)?.is_some();
            if !already_in_store {
                save_server_key_with(bare, store)
                    .context("migrating plaintext server_key into the secret store")?;
            }
            // Strip the plaintext key from the personal config regardless: it is
            // now in the store (or was already there), so it must not linger in
            // the file. `remove_server_key_from` preserves all other keys.
            remove_server_key_from(&global_path)
                .context("stripping migrated server_key from config.toml")?;
            tracing::info!(
                "migrated plaintext server_key out of {} into the {} secret store",
                global_path.display(),
                store.kind()
            );
        }

        // ── 3. Environment variable overrides ────────────────────────────────
        if let Ok(v) = std::env::var("SPELUNK_SERVER_URL") {
            cfg.server_url = Some(v);
        }
        let env_server_key = std::env::var("SPELUNK_SERVER_KEY").ok();
        if let Ok(v) = std::env::var("SPELUNK_PROJECT_ID") {
            cfg.project_id = Some(v);
        }
        // Env wins over either config file (personal or project-level).
        if let Ok(v) = std::env::var("SPELUNK_SERVER_CA") {
            cfg.server_ca = Some(v);
        }
        if let Ok(v) = std::env::var(llm_key::ENV_LLM_URL) {
            cfg.llm_url = Some(v);
        }
        if let Ok(v) = std::env::var(llm_key::ENV_LLM_MODEL) {
            cfg.llm_model = Some(v);
        }
        // SPELUNK_MODE overrides the configured sync mode. An
        // unrecognised value is a hard error — silently falling back to a
        // default would defeat the deterministic-mode guarantee the Founder
        // needs to separate OSS-local test runs from cloud dogfood runs.
        if let Ok(v) = std::env::var("SPELUNK_MODE") {
            let parsed = SyncMode::parse(&v).with_context(|| {
                format!(
                    "SPELUNK_MODE={v:?} is not a valid sync mode (expected one of: {})",
                    SyncMode::valid_values()
                )
            })?;
            cfg.mode = Some(parsed);
        }

        // ── 4. Resolve the cloud-kind bearer (ADR-071 D2) ────────────────────
        // `cfg.server_key` is now the **cloud-kind** bearer only:
        //   1. `SPELUNK_SERVER_KEY` env var (CI / headless escape hatch) — wins.
        //   2. `[auth].access_token` from `spelunk login` (WorkOS device flow).
        // Both tiers are origin-independent, so resolving them once here (with
        // no secret-store read at all) is correct and cheap. A self-hosted
        // `server_url`'s bearer is a *different* credential, scoped to that
        // server's origin, resolved lazily via [`Config::bearer_for`]. It is
        // never derived from this field, so a cloud login can never leak to a
        // self-hosted server (the bug this ADR fixes).
        cfg.server_key = if let Some(v) = env_server_key {
            Some(v)
        } else {
            // An empty (or absent) `[auth].access_token` means "not logged in":
            // it must resolve to no bearer, never an empty-string `Some("")`.
            cfg.auth
                .as_ref()
                .map(|auth| auth.access_token.clone())
                .filter(|token| !token.is_empty())
        };

        Ok(cfg)
    }

    /// Resolve the effective bearer for a request to `server_url` (ADR-071
    /// D2), using the host's default secret store. See
    /// [`Config::bearer_for_with_store`] for the resolution rules and the
    /// testable, store-injected form.
    pub fn bearer_for(&self, server_url: &str) -> Result<Option<String>> {
        let store = secret_store::default_store(&spelunk_config_dir())?;
        self.bearer_for_with_store(server_url, store.as_ref())
    }

    /// Same as [`Config::bearer_for`] but with an injected [`SecretStore`]
    /// (tests, and callers that already resolved a store).
    ///
    /// Branches on credential kind by `server_url`'s origin before touching
    /// any store (cloud vs. self-hosted server-key: see
    /// [`server_keys::bearer_for`] for the full precedence and the legacy
    /// migration it performs).
    pub fn bearer_for_with_store(
        &self,
        server_url: &str,
        store: &dyn SecretStore,
    ) -> Result<Option<String>> {
        server_keys::bearer_for(self.auth.as_ref(), server_url, store)
    }
}

/// Resolve the host's default [`SecretStore`], honouring [`secret_store::ENV_SECRET_STORE`].
///
/// The public entry point for CLI commands that need to read or write the
/// per-origin key map directly (`spelunk auth set-key` / `list-servers` /
/// `logout --servers`), the same resolution [`Config::load`] and
/// [`Config::bearer_for`] use internally.
pub fn default_secret_store() -> Result<Box<dyn SecretStore>> {
    secret_store::default_store(&spelunk_config_dir())
}

/// Build the warning line for a loopback `server_url` with no port, or
/// `None` when no warning applies. Pure so it's unit-testable without
/// capturing stderr; `Config::validate_with_project` prints the result.
///
/// A loopback `server_url` missing a port can never be the auto-discovered
/// local daemon (which always binds a specific port, default 7777): it is a
/// near-certain leftover misconfiguration, most often a stale `server_url`
/// after a team-server value was pared down to a bare host. This is a
/// warning, not a validation error: unlike a non-loopback plaintext
/// `http://` URL, it isn't a security problem, just a likely mistake.
fn portless_loopback_server_url_warning(url: &str) -> Option<String> {
    if !is_loopback_url_missing_port(url) {
        return None;
    }
    Some(format!(
        "Warning: server_url ({url}) is a loopback host with no port; this can never be \
         the auto-discovered local server (default port 7777). If this is a leftover \
         value, remove server_url from config; otherwise add the port your server \
         actually listens on."
    ))
}

/// Build an actionable error for a `config.toml` that failed to parse.
///
/// The bare `.context("parsing config.toml")` this replaces was unusable: an
/// [`anyhow::Error`]'s `Display` shows only its top context, so the file path
/// and the toml crate's own diagnostic (the offending key/line) never reached
/// the user. This produces a single self-contained message that names the
/// **file**, embeds the toml diagnostic that pinpoints the **offending key**,
/// and states the **remedy**.
fn config_parse_error(path: &Path, source: toml::de::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "could not parse the spelunk config file {path}:\n{source}\n\
         Fix the offending key shown above, or remove it to fall back to the default.",
        path = path.display(),
    )
}

/// Parse the global personal `config.toml`, turning any failure into an
/// actionable error (see [`config_parse_error`]).
///
/// An unrecognised `mode` is singled out so the message names the bad value
/// and lists the accepted set explicitly — the same guidance as the
/// `SPELUNK_MODE` env-var error — rather than relying on however serde/toml
/// happens to render the underlying enum error.
fn parse_global_config(raw: &str, path: &Path) -> Result<Config> {
    toml::from_str::<Config>(raw).map_err(|source| {
        if let Some(bad) = bad_mode_value(raw) {
            return anyhow::anyhow!(
                "invalid `mode` value {bad:?} in the spelunk config file {path} \
                 (expected one of: {valid})",
                path = path.display(),
                valid = SyncMode::valid_values(),
            );
        }
        config_parse_error(path, source)
    })
}

/// If `raw` sets `mode` to a string that is not a valid [`SyncMode`], return
/// that offending value. Runs only on the error path, so the extra parse is
/// off the happy path.
fn bad_mode_value(raw: &str) -> Option<String> {
    let table = raw.parse::<toml::Table>().ok()?;
    let value = table.get("mode")?.as_str()?;
    SyncMode::parse(value).is_none().then(|| value.to_string())
}

impl Config {
    /// Validate cross-field constraints. Call after `load()`.
    ///
    /// When `server_url` points to a loopback address (`127.0.0.1`, `localhost`, `::1`),
    /// `project_id` is allowed to be absent — it will be derived at runtime by
    /// `Config::resolve_project_id()` (see spelunk#307 / section D of #303).
    pub fn validate(&self) -> Result<()> {
        self.validate_with_project(self.project_id.is_some())
    }

    /// Like [`validate`](Self::validate) but lets the caller assert that a
    /// project identity is available from a source outside the config — e.g. an
    /// explicit `spelunk sync --project <slug>` flag.
    ///
    /// `spelunk sync` supplies its slug lazily (the project is created on first
    /// sync; ADR / founder decision 2026-07-01), so at config-validation time
    /// `project_id` may legitimately be `None` while `--project` carries the
    /// slug. Pass `project_available = true` in that case so the non-loopback
    /// `server_url` requirement is satisfied without a persisted `project_id`.
    /// The actual slug resolution (and the halt-with-guidance when *no* slug is
    /// available) is done by the sync command itself.
    pub fn validate_with_project(&self, project_available: bool) -> Result<()> {
        if let Some(url) = &self.server_url
            && !project_available
            && !is_loopback_url(url)
        {
            anyhow::bail!(
                "server_url is set but project_id is missing.\n\
                 Add `project_id = \"my-project\"` to .spelunk/config.toml \
                 or set SPELUNK_PROJECT_ID."
            );
        }
        if let Some(url) = &self.server_url
            && let Some(warning) = portless_loopback_server_url_warning(url)
        {
            eprintln!("{warning}");
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
    /// Always prefers `inference_url` (set for an auto-discovered loopback
    /// server, ADR-004). Whether it also falls back to `server_url` depends on
    /// [`Config::resolve_mode`] (2026-07-23 founder decision, ADR-004
    /// revision):
    ///
    /// - `cloud_first`: falls back to `server_url` — the explicitly-configured
    ///   remote owns both inference and memory.
    /// - `local_first` / `offline`: **never** falls back to `server_url`. An
    ///   explicit `server_url` in these modes is a sync replica only; inference
    ///   always prefers the local loopback embedder, which `inference_url`
    ///   alone carries. Returning `server_url` here was the root cause of a
    ///   past bug: a `local_first` project with a cloud `server_url`
    ///   sent embed requests to `{server_url}/index/embed`, which 404s (cloud
    ///   API has no such route) instead of ever reaching the local embedder.
    ///
    /// Memory storage selection does **not** use this — see `open_memory_backend`.
    pub fn resolve_inference_url(&self) -> Option<&str> {
        if self.resolve_mode() == SyncMode::CloudFirst {
            self.inference_url.as_deref().or(self.server_url.as_deref())
        } else {
            self.inference_url.as_deref()
        }
    }

    /// Resolve the effective sync mode.
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
        // Default mirrors the original implicit behaviour, from before the `mode` field existed.
        if self.server_url.is_some() {
            SyncMode::LocalFirst
        } else {
            SyncMode::Offline
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secret_store::MemoryStore;

    /// `Config::load` with a fresh in-memory secret store, so credential tests
    /// never touch the host keychain or `~/.config/spelunk/secrets.toml`.
    fn load_hermetic(path: &Path) -> Result<Config> {
        load_hermetic_with(path, &MemoryStore::default())
    }

    // No project-config discovery, so a .spelunk/config.toml checked in to this
    // repo cannot leak into the loaded Config (that is what makes it hermetic).
    fn load_hermetic_with(path: &Path, store: &dyn SecretStore) -> Result<Config> {
        Config::load_with_store_from(Some(path), store, None)
    }

    /// Unset all spelunk-related env vars to prevent cross-test contamination.
    fn clear_spelunk_env() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_URL");
            std::env::remove_var("SPELUNK_SERVER_KEY");
            std::env::remove_var("SPELUNK_PROJECT_ID");
            std::env::remove_var("SPELUNK_MODE");
            std::env::remove_var("SPELUNK_NO_SERVER");
            std::env::remove_var("SPELUNK_LLM_URL");
            std::env::remove_var("SPELUNK_LLM_MODEL");
        }
    }

    // ── resolve_mode defaults ─────────────────────────────────────────────────

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
        let cfg = load_hermetic(&config_path).unwrap();
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
        let err = load_hermetic(&config_path).unwrap_err();
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
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::LocalFirst));
    }

    #[test]
    #[serial_test::serial]
    fn config_with_pruned_keys_still_parses() {
        // Guards the forward-compat contract for pre-0.9 config.toml files:
        // `Config` carries no `deny_unknown_fields`, so keys pruned as dead
        // (batch_size, models_dir, api_base_url, lmstudio_base_url, plans_dir,
        // specs_dir, embedding_model) are ignored rather than rejected. Adding
        // `deny_unknown_fields` would break every existing user config, so
        // this must stay green.
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
mode = "local_first"
batch_size = 32
models_dir = "/opt/models"
api_base_url = "http://inference.internal:1234"
lmstudio_base_url = "http://127.0.0.1:1234"
plans_dir = "docs/plans"
specs_dir = "docs/specs"
embedding_model = "some-other-model"
"#,
        )
        .unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
        // Live keys still resolve; the removed keys are simply dropped.
        assert_eq!(cfg.mode, Some(SyncMode::LocalFirst));
    }

    // ── breaking change: the deprecated memory_server_* keys are gone ───────

    #[test]
    #[serial_test::serial]
    fn deprecated_memory_server_keys_are_ignored() {
        // The old aliases were removed pre-1.0: these keys no longer populate
        // server_url/server_key and are silently dropped as unknown.
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
memory_server_url = "http://old.example.com:7777"
memory_server_key = "secret-token"
project_id = "my-proj"
"#,
        )
        .unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.server_key, None);
        assert_eq!(cfg.project_id, Some("my-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn global_config_server_key_live_wins_over_deprecated() {
        // Both the live key and its removed alias present: the live server_key
        // resolves and the deprecated alias is silently dropped (no error, no
        // override). `server_key` is the legacy personal bearer (a bare
        // `server_key` in the *global* config, not a `.spelunk/config.toml`
        // credential; D4 is about the latter): it no longer feeds
        // `cfg.server_key` (cloud-kind only, ADR-071 D2), but the migration
        // into the secret store still runs and the value is still reachable
        // through `bearer_for_with_store` for a self-hosted origin.
        //
        // `server_url` is deliberately absent here: it cannot be set from the
        // global config at all (see `project_level_config_live_key_wins_over_deprecated`
        // for that guarantee's project-level equivalent).
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
server_key = "new-token"
memory_server_key = "old-token"
"#,
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&config_path, &store).unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.server_key, None);
        assert_eq!(
            cfg.bearer_for_with_store("http://new.example.com:7777", &store)
                .unwrap()
                .as_deref(),
            Some("new-token")
        );
    }

    #[test]
    #[serial_test::serial]
    fn loads_without_any_server_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
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

    // ── portless_loopback_server_url_warning ─────────────────────────────────

    #[test]
    fn portless_loopback_server_url_warning_fires_for_bare_localhost() {
        // The exact field-observed misconfig: `server_url = "http://localhost"`
        // with no port, silently accepted and used instead of the healthy
        // auto-discovered daemon on 7777.
        let warning = portless_loopback_server_url_warning("http://localhost")
            .expect("a portless loopback server_url must produce a warning");
        assert!(warning.contains("http://localhost"), "got: {warning}");
        assert!(warning.contains("no port"), "got: {warning}");
    }

    #[test]
    fn portless_loopback_server_url_warning_is_none_when_port_present() {
        assert_eq!(
            portless_loopback_server_url_warning("http://localhost:7777"),
            None
        );
        assert_eq!(
            portless_loopback_server_url_warning("http://127.0.0.1:7777"),
            None
        );
    }

    #[test]
    fn portless_loopback_server_url_warning_is_none_for_non_loopback_host() {
        // A non-loopback https:// URL with no explicit port is normal
        // (default port 443), not a misconfiguration signal.
        assert_eq!(
            portless_loopback_server_url_warning("https://team.example.com"),
            None
        );
    }

    #[test]
    fn validate_still_passes_for_a_portless_loopback_server_url() {
        // The warning is advisory, not a hard error: a portless loopback
        // server_url is still a *valid* transport (is_loopback_url_missing_port
        // is orthogonal to validate_transport_url's scheme/security check).
        let cfg = Config {
            server_url: Some("http://localhost".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "a portless loopback server_url must warn, not fail validate()"
        );
    }

    // ── validate_with_project() — --project satisfies the requirement ──────────

    #[test]
    fn validate_with_project_true_passes_non_loopback_without_project_id() {
        // First-run `spelunk sync --project <slug>`: a non-loopback server_url is
        // set, no project_id is persisted, but the caller asserts a project slug
        // is available from --project. This must pass (previously blocked sync).
        let cfg = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate_with_project(true).is_ok());
    }

    #[test]
    fn validate_with_project_false_still_fails_non_loopback_without_project_id() {
        // No --project and no configured project_id → the requirement still bites.
        let cfg = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate_with_project(false).is_err());
    }

    #[test]
    fn validate_delegates_to_validate_with_project() {
        // validate() == validate_with_project(project_id.is_some()).
        let with_id = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: Some("p".to_string()),
            ..Default::default()
        };
        assert!(with_id.validate().is_ok());
        let without_id = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(without_id.validate().is_err());
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
    #[serial_test::serial]
    fn resolve_inference_url_falls_back_to_server_url_in_cloud_first() {
        // Explicit team/cloud server in `cloud_first` mode: only server_url
        // set; it serves inference too (founder decision 2026-07-23 — the
        // ONLY mode where a configured server_url owns inference).
        clear_spelunk_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("http://team.example.com:7777".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_inference_url(),
            Some("http://team.example.com:7777")
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_local_first_never_falls_back_to_server_url() {
        // Regression guard: `local_first` (the default
        // mode once `server_url` is set, with no explicit `mode` key) must
        // NOT fall back to `server_url` for inference — that is exactly the
        // routing that sent embed requests to a cloud `server_url`'s
        // nonexistent `/index/embed` route and produced the 404s. An
        // explicit `server_url` here is a sync replica only.
        clear_spelunk_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("https://api.spelunk.cloud".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_offline_never_falls_back_to_server_url() {
        clear_spelunk_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("https://api.spelunk.cloud".to_string()),
            mode: Some(SyncMode::Offline),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    fn resolve_inference_url_none_when_neither_set() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_inference_url_wins_over_server_url() {
        // Defensive: if both are somehow set, inference must use the dedicated
        // inference_url (memory backend selection still uses server_url).
        // Holds in every mode; exercised here in cloud_first, the one mode
        // where server_url would otherwise be a candidate too.
        clear_spelunk_env();
        let cfg = Config {
            inference_url: Some("http://127.0.0.1:7777".to_string()),
            server_url: Some("http://team.example.com:7777".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    // ── env var overrides ────────────────────────────────────────────────────
    //
    // Env var tests are #[serial] because they mutate process-global state.

    #[test]
    #[serial_test::serial]
    fn global_personal_config_cannot_set_server_url() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_url = "http://personal.example.com:7777"
"#,
        )
        .unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.server_url, None);
    }

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
        let cfg = load_hermetic(&config_path).unwrap();
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
        let cfg = load_hermetic(&config_path).unwrap();
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
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.project_id, Some("env-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn env_spelunk_memory_server_url_is_ignored() {
        // Breaking change: the deprecated SPELUNK_MEMORY_SERVER_URL env fallback
        // was removed. Setting it alone must NOT populate server_url. (Not in
        // clear_spelunk_env's unset list since nothing reads it — clean up here.)
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        unsafe {
            std::env::set_var("SPELUNK_MEMORY_SERVER_URL", "http://old.example.com:7777");
        }
        let cfg = load_hermetic(&config_path).unwrap();
        unsafe {
            std::env::remove_var("SPELUNK_MEMORY_SERVER_URL");
        }
        assert_eq!(cfg.server_url, None);
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

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://proj.example.com:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));
    }

    // ── [auth] WorkOS tokens ───────────────────────────────────────────────────

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

        let cfg = load_hermetic(&path).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
        let auth = cfg.auth.expect("auth table should load");
        assert_eq!(auth.refresh_token, "rt-sample");
        assert_eq!(auth.org_id, "org_sample");
    }

    // ── [auth] partial-table tolerance ─────────────────────────────────────────
    //
    // `--org` is an optional scoping flag and hand-editing the config is a
    // documented workflow, so a login-without-org or a trimmed `[auth]` table
    // must not brick every command with a parse error. Each field is tolerated
    // when absent (missing token ⇒ not logged in, missing expiry ⇒ expired).

    // A `[auth]` table missing `org_id` still loads: the access token resolves
    // as the bearer and the org is simply empty (no scoping).
    #[test]
    #[serial_test::serial]
    fn auth_block_missing_org_id_still_loads() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             access_token = \"at\"\n\
             refresh_token = \"rt\"\n\
             expires_at = 4000000000\n",
        )
        .unwrap();

        let cfg = load_hermetic(&path).expect("a [auth] table without org_id must still parse");
        assert_eq!(cfg.server_key.as_deref(), Some("at"));
        let auth = cfg.auth.expect("auth table should load");
        assert_eq!(auth.org_id, "", "missing org_id is treated as no scoping");
    }

    // A `[auth]` table missing `expires_at` loads and the token is treated as
    // expired (an unknown expiry must never read as "still valid").
    #[test]
    #[serial_test::serial]
    fn auth_block_missing_expires_at_is_treated_as_expired() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             access_token = \"at\"\n\
             refresh_token = \"rt\"\n\
             org_id = \"org_x\"\n",
        )
        .unwrap();

        let cfg = load_hermetic(&path).expect("a [auth] table without expires_at must still parse");
        let auth = cfg.auth.expect("auth table should load");
        assert!(
            auth.is_expired_at(1),
            "a token with no expiry must be treated as expired"
        );
    }

    // A `[auth]` table with no access token means "not logged in": no bearer is
    // resolved (an empty token must never become a `Some("")` bearer).
    #[test]
    #[serial_test::serial]
    fn auth_block_missing_access_token_means_not_logged_in() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             refresh_token = \"rt\"\n\
             org_id = \"org_x\"\n",
        )
        .unwrap();

        let cfg = load_hermetic(&path).expect("a [auth] table without access_token must parse");
        assert_eq!(
            cfg.server_key, None,
            "a missing/empty access token must resolve to no bearer"
        );
    }

    // The reported failure mode: an otherwise-fine config with a bare `[auth]`
    // header (every field trimmed away) must still load rather than error.
    #[test]
    #[serial_test::serial]
    fn bare_auth_header_does_not_brick_load() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "llm_model = \"gpt-oss\"\n[auth]\n").unwrap();

        let cfg = load_hermetic(&path).expect("a bare [auth] header must not brick the load");
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(cfg.server_key, None);
    }

    // ── actionable parse-error messages ────────────────────────────────────────

    // An unrecognised `mode` names the bad value AND lists the valid modes AND
    // the file, mirroring the `SPELUNK_MODE` env-var message.
    #[test]
    #[serial_test::serial]
    fn invalid_mode_value_error_names_value_modes_and_file() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "mode = \"bogus_mode\"\n").unwrap();

        let err = load_hermetic(&path).unwrap_err().to_string();
        assert!(err.contains("bogus_mode"), "must name the bad value: {err}");
        assert!(err.contains("offline"), "must list valid modes: {err}");
        assert!(err.contains("local_first"), "must list valid modes: {err}");
        assert!(err.contains("cloud_first"), "must list valid modes: {err}");
        assert!(
            err.contains(&path.display().to_string()),
            "must name the config file: {err}"
        );
    }

    // A genuinely malformed config (a type error here) produces a message that
    // names the file and points at a remedy, not a bare "parsing config.toml".
    #[test]
    #[serial_test::serial]
    fn malformed_config_error_names_file_and_remedy() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // `llm_context_length` is a usize; a string is a hard type error.
        std::fs::write(&path, "llm_context_length = \"lots\"\n").unwrap();

        let err = load_hermetic(&path).unwrap_err().to_string();
        assert!(
            err.contains(&path.display().to_string()),
            "must name the config file: {err}"
        );
        assert!(
            err.contains("remove") || err.contains("Fix"),
            "must point at a remedy: {err}"
        );
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
        let cfg = load_hermetic(&path).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("ci-token"));
        // The refresh token is still available for the refresh path.
        assert_eq!(cfg.auth.unwrap().refresh_token, "rt-sample");
        unsafe { std::env::remove_var("SPELUNK_SERVER_KEY") };
    }

    /// A legacy bare `server_key` no longer feeds `cfg.server_key` (cloud-kind
    /// only, ADR-071 D2) when no `[auth]` table exists, but it is still
    /// migrated into the store and resolves via `bearer_for` for a
    /// self-hosted origin.
    #[test]
    #[serial_test::serial]
    fn legacy_server_key_migrates_but_no_longer_feeds_server_key_field() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();

        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key, None);
        assert!(cfg.auth.is_none());
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-legacy")
        );
    }

    /// The `[auth]` access token (cloud kind) and a legacy bare `server_key`
    /// (self-hosted kind) resolve independently by target origin (ADR-071
    /// D2): they no longer compete in a single flat precedence chain.
    #[test]
    #[serial_test::serial]
    fn auth_token_and_legacy_server_key_resolve_by_kind_not_precedence() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        // Cloud-kind field resolves from [auth], unaffected by the legacy key.
        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
        // The legacy key is still migrated and reachable for a self-hosted
        // origin; the cloud token never leaks to it.
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-legacy")
        );
    }

    // Writing auth tokens preserves other top-level keys (e.g. `llm_model`).
    // Not `server_url`: the global config no longer surfaces that field (see
    // `load_with_store`), so it is not a useful "other key" for this test.
    #[test]
    #[serial_test::serial]
    fn save_auth_tokens_preserves_other_keys() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "llm_model = \"gpt-oss\"\n").unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let cfg = load_hermetic(&path).unwrap();
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
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
        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert!(cfg.auth.is_none());
        assert_eq!(cfg.server_key, None);
        // Legacy key still present in the store, reachable via bearer_for.
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-legacy")
        );
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
    fn project_level_config_ignores_deprecated_memory_server_url() {
        // Breaking change: the removed alias no longer resolves in project config.
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

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.project_id, Some("team/old".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn project_level_config_live_key_wins_over_deprecated() {
        // Mixed project config: the live server_url resolves and the removed
        // alias is dropped (no error, no override).
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let spelunk_dir = proj_dir.join(".spelunk");
        std::fs::create_dir_all(&spelunk_dir).unwrap();
        std::fs::write(
            spelunk_dir.join("config.toml"),
            r#"server_url = "http://new.example.com:7777"
memory_server_url = "http://old.example.com:7777"
project_id = "team/new"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://new.example.com:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/new".to_string()));
    }

    // ── keychain secret store migration / precedence ─────────────────────────
    //
    // These exercise the credential paths through an injected `MemoryStore`, so
    // no real keychain or Secret Service daemon is required (CI-safe).

    /// Migration: a bare `server_key` in the personal global config is moved
    /// into the secret store and stripped from the file on next load. It no
    /// longer feeds `cfg.server_key` (cloud-kind only, ADR-071 D2); it
    /// resolves via `bearer_for` for a self-hosted origin instead.
    #[test]
    #[serial_test::serial]
    fn migration_moves_bare_server_key_into_store_and_strips_file() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://team:7777\"\nserver_key = \"sk-sp-legacy\"\nproject_id = \"p\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();

        assert_eq!(cfg.server_key, None);
        // Moved into the store.
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-sp-legacy")
        );
        // Resolves transparently through bearer_for for the configured server.
        assert_eq!(
            cfg.bearer_for_with_store("http://team:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-sp-legacy")
        );
        // Stripped from the file, but other keys preserved.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("server_key"),
            "server_key must be stripped from config.toml after migration, got:\n{on_disk}"
        );
        assert!(on_disk.contains("server_url"), "other keys must survive");
        assert!(on_disk.contains("project_id"), "other keys must survive");
    }

    /// Migration is idempotent: a second load (file already stripped, store
    /// populated) keeps resolving the same bearer with no further changes.
    #[test]
    #[serial_test::serial]
    fn migration_is_idempotent_across_two_loads() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-sp-legacy\"\n").unwrap();

        let store = MemoryStore::default();
        let first = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(first.server_key, None);

        let second = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(second.server_key, None);
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-sp-legacy")
        );
        assert_eq!(
            second
                .bearer_for_with_store("https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-sp-legacy")
        );
    }

    /// Migration must NOT clobber a credential already in the store (e.g. a
    /// freshly-saved key) with a stale value from the file — the store wins, the
    /// stale file value is still stripped.
    #[test]
    #[serial_test::serial]
    fn migration_does_not_clobber_existing_store_credential() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-stale-file\"\n").unwrap();

        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-fresh-store").unwrap();

        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key, None);
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-fresh-store")
        );
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-fresh-store")
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("server_key"), "stale file key stripped");
    }

    /// A credential saved via `save_server_key_with` lands ONLY in the store and
    /// never in `config.toml` — the core acceptance criterion.
    #[test]
    #[serial_test::serial]
    fn saved_credential_is_in_store_not_in_config_file() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://team:7777\"\nproject_id = \"p\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        save_server_key_with("sk-sp-new", &store).unwrap();

        // Config file untouched (no secret written there).
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("sk-sp-new"));
        assert!(!on_disk.contains("server_key"));

        // Resolves from the store via bearer_for, not the eager server_key field.
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key, None);
        assert_eq!(
            cfg.bearer_for_with_store("http://team:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-sp-new")
        );
    }

    /// `logout` (remove_server_key_with) clears the store entry AND any legacy
    /// plaintext key still in config.toml.
    #[test]
    #[serial_test::serial]
    fn logout_clears_store_and_legacy_file_key() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();

        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-in-store").unwrap();

        remove_server_key_with(&store, &path).unwrap();

        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None, "store cleared");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("server_key"),
            "legacy file key also cleared"
        );

        // After logout, nothing resolves as the bearer.
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key, None);
    }

    /// Env-var precedence: `SPELUNK_SERVER_KEY` wins over a stored credential.
    #[test]
    #[serial_test::serial]
    fn env_server_key_wins_over_store() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-in-store").unwrap();

        unsafe { std::env::set_var("SPELUNK_SERVER_KEY", "sk-from-env") };
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("sk-from-env"));
        unsafe { std::env::remove_var("SPELUNK_SERVER_KEY") };
    }

    /// Precedence: `[auth]` access token wins over a stored `server_key`.
    #[test]
    #[serial_test::serial]
    fn auth_token_wins_over_store() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-in-store").unwrap();

        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
    }

    /// ADR-071 D4: a `server_key` line in the project-level, checked-in
    /// `.spelunk/config.toml` is dropped entirely: no read, no warning, no
    /// effect on the resolved bearer at any tier. Mirrors the
    /// `memory_server_*` silent-drop precedent.
    #[test]
    #[serial_test::serial]
    fn project_config_server_key_field_is_silently_dropped() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let spelunk_dir = proj_dir.join(".spelunk");
        std::fs::create_dir_all(&spelunk_dir).unwrap();
        let proj_cfg = spelunk_dir.join("config.toml");
        std::fs::write(
            &proj_cfg,
            "server_url = \"https://team.example:7777\"\nserver_key = \"team-shared-key\"\nproject_id = \"team/proj\"\n",
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let store = MemoryStore::default();
        let original_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&proj_dir).unwrap();
        let cfg = Config::load_with_store(Some(&global_config), &store).unwrap();
        if let Some(d) = original_cwd {
            std::env::set_current_dir(d).unwrap();
        }

        // The file's other fields still load fine (no parse error from the
        // now-unrecognized `server_key` key).
        assert_eq!(
            cfg.server_url,
            Some("https://team.example:7777".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));
        // No credential resolves anywhere: not eagerly, not per-origin.
        assert_eq!(cfg.server_key, None);
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &store)
                .unwrap(),
            None
        );
        // Never touches the personal secret store.
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
        // The checked-in file itself is left untouched (D4 does not rewrite it).
        assert!(
            std::fs::read_to_string(&proj_cfg)
                .unwrap()
                .contains("server_key")
        );
    }

    /// No-keychain fallback contract: the file-backed store stands in for a
    /// keychain when none exists, so `bearer_for` resolves the credential
    /// identically. This mirrors what `default_store` does on a headless host.
    #[test]
    #[serial_test::serial]
    fn file_store_fallback_resolves_credential_like_keychain() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let file_store = secret_store::FileStore::new(tmp.path().join("secrets.toml"));
        save_server_key_with("sk-headless", &file_store).unwrap();

        let cfg = Config::load_with_store(Some(&path), &file_store).unwrap();
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:7777", &file_store)
                .unwrap()
                .as_deref(),
            Some("sk-headless")
        );
    }

    /// No credential anywhere (empty config, empty store, no env) ⇒ no bearer,
    /// and no hard failure — the headless/unauthenticated path stays graceful.
    #[test]
    #[serial_test::serial]
    fn no_credential_anywhere_yields_none_without_error() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cfg.server_key, None);
    }

    /// A `SecretStore` test double that counts `get` calls per key, wrapping a
    /// `MemoryStore`. Used to assert that `load_with_store` never reads the
    /// personal store when a higher-precedence credential (env var or
    /// `[auth]` token) already resolves the bearer — a value that would only
    /// be discarded must never cost a keychain round-trip on real hosts.
    #[derive(Default)]
    struct CountingStore {
        inner: MemoryStore,
        get_calls: std::sync::atomic::AtomicUsize,
    }

    impl SecretStore for CountingStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.get_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(key)
        }

        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.inner.set(key, value)
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }

        fn kind(&self) -> &'static str {
            "counting-test-double"
        }
    }

    /// `SPELUNK_SERVER_KEY` outranks the personal store, so the store must
    /// never be asked for `server_key` at all — not just overridden after the
    /// fact. Regression test for the redundant-keychain-read fix.
    #[test]
    #[serial_test::serial]
    fn env_server_key_skips_store_read_entirely() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let store = CountingStore::default();
        unsafe { std::env::set_var("SPELUNK_SERVER_KEY", "sk-from-env") };
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        unsafe { std::env::remove_var("SPELUNK_SERVER_KEY") };

        assert_eq!(cfg.server_key.as_deref(), Some("sk-from-env"));
        assert_eq!(
            store.get_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the personal secret store must not be read when SPELUNK_SERVER_KEY \
             already resolves the bearer"
        );
    }

    /// A WorkOS `[auth]` access token outranks the personal store the same
    /// way the env var does: the store must never be queried for
    /// `server_key` when `[auth]` already resolves the bearer.
    #[test]
    #[serial_test::serial]
    fn auth_token_skips_store_read_entirely() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let store = CountingStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();

        assert_eq!(cfg.server_key.as_deref(), Some("at-sample"));
        assert_eq!(
            store.get_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the personal secret store must not be read when a WorkOS [auth] \
             token already resolves the bearer"
        );
    }

    // ── llm_model_configured (pre-parse help gate) ────────────────────────────
    //
    // `llm_model_configured` takes no `SecretStore` at all — its signature is
    // `fn(path: Option<&Path>) -> bool`, so there is no store to inject or
    // instrument. Reading its body confirms it only calls
    // `std::fs::read_to_string` + `toml::from_str`, with no reference to
    // `secret_store`/`SecretStore`/`default_store` anywhere: it is
    // structurally incapable of constructing a secret store. These tests
    // cover its actual file-parsing contract, which is what the CLI's
    // pre-parse help gate depends on now that it no longer calls the full
    // `Config::load`.

    /// Resolves purely from the config file on disk: present + non-empty
    /// `llm_model` ⇒ true, absent ⇒ false, missing file ⇒ false (no error).
    #[test]
    fn llm_model_configured_reads_only_the_config_file() {
        let tmp = TempDir::new().unwrap();

        let with_model = tmp.path().join("with_model.toml");
        std::fs::write(&with_model, "llm_model = \"gpt-x\"\n").unwrap();
        assert!(Config::llm_model_configured(Some(&with_model)));

        let without_model = tmp.path().join("without_model.toml");
        std::fs::write(&without_model, "server_url = \"http://x\"\n").unwrap();
        assert!(!Config::llm_model_configured(Some(&without_model)));

        let missing = tmp.path().join("does_not_exist.toml");
        assert!(!Config::llm_model_configured(Some(&missing)));
    }

    #[test]
    #[serial_test::serial]
    fn server_ca_env_overrides_config() {
        // Env `SPELUNK_SERVER_CA` wins over the personal/global config value.
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "server_ca = \"/from/config.pem\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_SERVER_CA", "/from/env.pem") };
        let cfg = load_hermetic(&global).unwrap();
        unsafe { std::env::remove_var("SPELUNK_SERVER_CA") };

        assert_eq!(cfg.server_ca.as_deref(), Some("/from/env.pem"));
    }

    #[test]
    #[serial_test::serial]
    fn server_ca_from_config_when_env_unset() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "server_ca = \"/from/config.pem\"\n").unwrap();

        unsafe { std::env::remove_var("SPELUNK_SERVER_CA") };
        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.server_ca.as_deref(), Some("/from/config.pem"));
    }

    // ── llm_url / llm_model ──────────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn llm_url_loads_from_personal_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_absent_from_personal_config_stays_none() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"gpt-oss\"\n").unwrap();

        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.llm_url, None);
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_in_project_config_is_ignored() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "").unwrap();

        let project = tmp.path().join("proj");
        std::fs::create_dir_all(project.join(".spelunk")).unwrap();
        std::fs::write(
            project.join(".spelunk").join("config.toml"),
            "llm_url = \"http://team-box.example:1234\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = Config::load_with_store_from(Some(&global), &store, Some(&project)).unwrap();

        assert_eq!(
            cfg.llm_url, None,
            "an endpoint URL in a checked-in project config must not configure the CLI"
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_env_overrides_personal_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_URL", "https://gateway.example") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_URL") };

        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("https://gateway.example")
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_env_applies_when_personal_config_sets_nothing() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_URL", "https://gateway.example") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_URL") };

        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("https://gateway.example")
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_overrides_personal_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"from-config\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_MODEL", "from-env") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_applies_when_personal_config_sets_nothing() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_MODEL", "from-env") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn save_auth_tokens_preserves_llm_url() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "llm_model = \"gpt-oss\"\nllm_url = \"http://127.0.0.1:1234\"\n",
        )
        .unwrap();
        save_auth_tokens_to(&sample_tokens(), &path).unwrap();

        let cfg = load_hermetic(&path).unwrap();
        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(cfg.auth.unwrap().access_token, "at-sample");
    }

    // The daemon-spawn path is the only reader of the LLM credential, so a
    // plain `Config::load` must not pay for a secret-store read of it.
    #[test]
    #[serial_test::serial]
    fn config_load_never_reads_the_llm_key_from_the_store() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        let store = RecordingStore::default();
        store
            .set(secret_store::KEY_LLM_KEY, "sk-llm-secret")
            .unwrap();
        store.reads.lock().unwrap().clear();

        let cfg = Config::load_with_store_from(Some(&global), &store, None).unwrap();

        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert!(
            !store
                .reads
                .lock()
                .unwrap()
                .iter()
                .any(|k| k == secret_store::KEY_LLM_KEY),
            "Config::load must not read the LLM key: reads were {:?}",
            store.reads.lock().unwrap()
        );
    }

    // Broader than the guard above, which only names the LLM key: the ordinary
    // load path reads no secret at all, so any store read added later goes red
    // here rather than only a credential-shaped one.
    #[test]
    #[serial_test::serial]
    fn config_load_reads_nothing_at_all_from_an_injected_store() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(
            &global,
            "llm_url = \"http://127.0.0.1:1234\"\nllm_model = \"gpt-oss\"\n",
        )
        .unwrap();

        let store = RecordingStore::default();
        store
            .set(secret_store::KEY_LLM_KEY, "sk-llm-secret")
            .unwrap();
        store.set(KEY_SERVER_KEY, "sk-sp-server").unwrap();
        store.reads.lock().unwrap().clear();

        Config::load_with_store_from(Some(&global), &store, None).unwrap();

        let reads = store.reads.lock().unwrap().clone();
        assert!(
            reads.is_empty(),
            "a config load with no legacy plaintext key to migrate must read no secret: {reads:?}"
        );
    }

    // `Config::load` resolves its own store, so the RecordingStore guards above
    // cannot observe that path at all: a store read added there would go
    // unnoticed, and on macOS would be a keychain authorization on every
    // command. An unparseable secrets.toml makes any read fail whatever the
    // backend, so loading successfully is the proof.
    #[test]
    #[serial_test::serial]
    fn the_public_load_entry_point_reads_no_secret_either() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();
        std::fs::write(tmp.path().join("secrets.toml"), "not = valid = toml\n").unwrap();

        let prev_store = std::env::var(secret_store::ENV_SECRET_STORE).ok();
        unsafe {
            std::env::set_var("SPELUNK_CONFIG_DIR", tmp.path());
            std::env::set_var(secret_store::ENV_SECRET_STORE, "file");
        }
        let loaded = Config::load(Some(&global));
        unsafe {
            std::env::remove_var("SPELUNK_CONFIG_DIR");
            match &prev_store {
                Some(v) => std::env::set_var(secret_store::ENV_SECRET_STORE, v),
                None => std::env::remove_var(secret_store::ENV_SECRET_STORE),
            }
        }

        let cfg = loaded.expect("Config::load must not read the secret store");
        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
    }

    // An explicitly empty SPELUNK_LLM_URL is an override like any other value,
    // so it blanks the personal config rather than falling through to it. The
    // spawn path then normalizes the blank away and configures no endpoint.
    #[test]
    #[serial_test::serial]
    fn llm_url_env_set_to_empty_still_overrides_the_personal_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_URL", "") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_URL") };

        assert_eq!(cfg.unwrap().llm_url.as_deref(), Some(""));
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_set_to_empty_still_overrides_the_personal_config() {
        clear_spelunk_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"gpt-oss\"\n").unwrap();

        unsafe { std::env::set_var("SPELUNK_LLM_MODEL", "") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("SPELUNK_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some(""));
    }

    // A MemoryStore that records every key passed to `get`.
    #[derive(Default)]
    struct RecordingStore {
        inner: MemoryStore,
        reads: std::sync::Mutex<Vec<String>>,
    }

    impl SecretStore for RecordingStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.reads.lock().unwrap().push(key.to_string());
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }
        fn kind(&self) -> &'static str {
            "recording"
        }
    }
}
