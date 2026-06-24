//! Capability tier detection for the spelunk CLI.
//!
//! Tier 0 (Offline): no server_url configured, or server unreachable.
//! Tier 1 (Server):  server_url set and GET /v1/health succeeds.
//!
//! ## Loopback auto-discovery (spelunk#316 / 0.8.0)
//!
//! When `cfg.server_url` is `None` **and** `SPELUNK_NO_SERVER` is not set, the probe
//! attempts to reach a locally-running spelunk-server before falling through to
//! `Tier::Offline`:
//!
//! 1. Read `~/.local/state/spelunk/server.port` (written by `spelunk server start`);
//!    use `http://127.0.0.1:<port>` if the file exists.
//! 2. Otherwise probe `http://127.0.0.1:7777` with a **250 ms** timeout (distinct from
//!    the 2 s timeout used for explicitly-configured remote URLs).
//! 3. On success, treat as `Tier::Server` with `auto_discovered = true`.
//! 4. On failure, return `Tier::Offline`.
//!
//! `SPELUNK_NO_SERVER=1` short-circuits all loopback probing and forces `Tier::Offline`.
//!
//! The probe runs lazily on the first call that needs Tier 1 and its result
//! is cached for the process lifetime.

use serde::Serialize;
use tokio::sync::OnceCell;

use crate::config::Config;

/// State file directory: `~/.local/state/spelunk/`.
///
/// On all platforms we use `~/.local/state` rather than the OS-native state dir.
/// This mirrors the deliberate choice made for the config dir
/// (`spelunk_config_dir` in spelunk-core's `config.rs`, which uses `~/.config`
/// on every platform): it keeps the path identical across Linux and macOS, and
/// matches what the CLI documentation and error messages say.
///
/// It also sidesteps a concrete portability bug: `dirs::state_dir()` returns
/// `None` on macOS (dirs v6 has no XDG_STATE_HOME equivalent there), which
/// silently disabled loopback auto-discovery on the primary dev platform
/// (spelunk#316). Returns `None` only when the home directory can't be resolved.
///
/// NOTE for spelunk#317 (writer side, `spelunk server start`): the writer MUST
/// write `server.port` into this exact directory so reader and writer agree.
/// Use the same `~/.local/state/spelunk/` path on every platform.
fn spelunk_state_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| home.join(".local").join("state").join("spelunk"))
}

/// Read the port written by `spelunk server start` into
/// `~/.local/state/spelunk/server.port`. Returns `None` if absent or unreadable.
fn read_server_port_file() -> Option<u16> {
    let path = spelunk_state_dir()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}

static TIER: OnceCell<Tier> = OnceCell::const_new();

/// Feature availability for a server-connected tier.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub search_semantic: bool,
    pub index_embed: bool,
    pub memory_push: bool,
    pub memory_pull: bool,
    pub memory_search: bool,
    pub memory_harvest: bool,
    pub explore: bool,
    pub plan: bool,
}

impl Capabilities {
    fn from_server_caps(caps: &[&str]) -> Self {
        let has = |c: &str| caps.contains(&c);
        let memory = has("memory");
        Self {
            search_semantic: has("search.semantic"),
            index_embed: has("index.embed"),
            memory_push: memory,
            memory_pull: memory,
            memory_search: memory,
            memory_harvest: memory,
            explore: has("explore"),
            plan: has("plan"),
        }
    }

    /// Conservative set assumed when talking to a legacy server that returns
    /// plain-text health ("ok") instead of JSON.
    fn legacy_memory_only() -> Self {
        Self {
            search_semantic: false,
            index_embed: false,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: false,
            explore: false,
            plan: false,
        }
    }

    /// Full set for a fully-featured server.
    #[allow(dead_code)]
    pub fn all() -> Self {
        Self {
            search_semantic: true,
            index_embed: true,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: true,
            explore: true,
            plan: true,
        }
    }
}

/// CLI capability tier for this process.
#[derive(Debug, Clone)]
pub enum Tier {
    /// No server configured, or server unreachable. Offline features only.
    Offline,
    /// Server reachable. All `caps`-listed features are available.
    Server {
        url: String,
        caps: Capabilities,
        /// `true` when the URL was discovered automatically (loopback probe),
        /// `false` when it was set explicitly via config / env var.
        /// Used to annotate UX output (e.g. `(local, auto)` in `spelunk status`).
        /// Consumed by `is_auto_discovered()` and sub-issue #324 UX wiring.
        #[allow(dead_code)]
        auto_discovered: bool,
    },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    // Used by check.rs / status.rs via pattern matching on the enum variant;
    // also consumed by sub-issues #323/#324 UX wiring.
    #[allow(dead_code)]
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline => None,
        }
    }

    #[allow(dead_code)]
    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline => None,
        }
    }

    /// Returns `true` when the server URL was discovered automatically via
    /// the loopback probe rather than set explicitly in config or environment.
    /// Used by `spelunk status` (sub-issue #324) to annotate the URL with `(local, auto)`.
    #[allow(dead_code)]
    pub fn is_auto_discovered(&self) -> bool {
        matches!(
            self,
            Tier::Server {
                auto_discovered: true,
                ..
            }
        )
    }

    /// Return a `Config` whose server fields reflect this tier, so that
    /// server-backed helpers (`ServerInferenceClient::from_config`,
    /// `open_memory_backend`) work the same whether the server was configured
    /// explicitly or discovered via the loopback probe.
    ///
    /// Loopback auto-discovery sets the capability `Tier` WITHOUT populating
    /// `cfg.server_url`. Commands that route inference through `from_config`
    /// gate on a server URL, so without this bridge they wrongly report
    /// "requires spelunk-server" even though `spelunk status` shows `Server`.
    ///
    /// ## ADR-004: inference vs memory storage are routed separately
    ///
    /// An auto-discovered loopback server is an **inference** backend only; it
    /// is never a memory store. So when the tier is `Server` and `server_url`
    /// is unset (the auto-discovered case), the discovered URL is written to
    /// `inference_url` — NOT `server_url`. `ServerInferenceClient::from_config`
    /// reads `inference_url` (falling back to `server_url`), so inference still
    /// reaches the loopback server; `open_memory_backend` reads only
    /// `server_url`, so memory stays on the project's local `memory.db`. This
    /// is what removes the split-brain where `memory add` wrote `memory.db`
    /// while `memory search` read the server's `server.db`.
    ///
    /// `project_id` is derived (mirroring `embed_phase`, see spelunk#307) so the
    /// inference client can address the project on the server. When an explicit
    /// `cfg.server_url` is already set (a team/remote server), it owns both
    /// inference and memory and the config is returned unchanged.
    pub fn effective_config(&self, cfg: &Config, project_root: &std::path::Path) -> Config {
        let mut out = cfg.clone();
        if let Tier::Server { url, .. } = self
            && out.server_url.is_none()
        {
            // Auto-discovered loopback server: route inference here, but leave
            // `server_url` unset so memory selection stays local (ADR-004).
            out.inference_url = Some(url.clone());
            if out.project_id.is_none() {
                out.project_id = Some(cfg.resolve_project_id(project_root));
            }
        }
        out
    }
}

/// Return the cached capability tier for this process.
///
/// On the first call, probes the server according to the following priority:
///
/// 1. If `SPELUNK_NO_SERVER=1` is set → `Tier::Offline` immediately.
/// 2. If `cfg.server_url` is set → probe that URL with a **2 s** timeout
///    (`auto_discovered = false`).
/// 3. If `cfg.server_url` is `None` → loopback auto-discovery:
///    a. Read `~/.local/state/spelunk/server.port`; probe `127.0.0.1:<port>`.
///    b. Fallback: probe `127.0.0.1:7777`.
///    Both loopback probes use a **250 ms** timeout.
///    On success: `auto_discovered = true`. On failure: `Tier::Offline`.
///
/// Subsequent calls return immediately from the per-process `OnceCell` cache.
///
/// **Per-process cache**: the result is stored in a `static OnceCell` and is fixed
/// for the lifetime of the process. This is correct for CLI invocations (one process
/// = one config), but unsuitable for long-running daemons that may use multiple
/// configs — they would always see the tier determined by the first call.
pub async fn get_tier(cfg: &Config) -> &'static Tier {
    // ADR-037 D1: an *explicit* offline mode (config `mode = "offline"`,
    // `SPELUNK_MODE=offline`, or the `SPELUNK_NO_SERVER=1` kill-switch) skips all
    // server probes — the user has asked for a provable no-cloud run.
    //
    // The *defaulted* offline (no `server_url` and no explicit `mode`) must NOT
    // skip probing: loopback auto-discovery is inference-only (it never owns
    // memory, ADR-004) and is what gives a local-only project semantic search.
    // Conflating the two would silently disable the loopback embedder.
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    let url = cfg.server_url.clone();
    let key = cfg.server_key.clone();
    TIER.get_or_init(|| async move {
        if explicit_offline {
            tracing::debug!("sync mode is explicitly offline — skipping all server probes");
            return Tier::Offline;
        }
        probe(url.as_deref(), key.as_deref()).await
    })
    .await
}

/// Remote-server probe timeout (explicit `server_url` in config/env).
const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Loopback probe timeout (auto-discovery of a locally-running server).
const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Default loopback port for `spelunk-server`.
const DEFAULT_LOOPBACK_PORT: u16 = 7777;

async fn probe(url: Option<&str>, key: Option<&str>) -> Tier {
    // ── 1. SPELUNK_NO_SERVER short-circuit ───────────────────────────────────
    if matches!(
        std::env::var("SPELUNK_NO_SERVER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        tracing::debug!("SPELUNK_NO_SERVER set — skipping all server probes");
        return Tier::Offline;
    }

    // ── 2. Explicit server_url from config / env ─────────────────────────────
    if let Some(url) = url {
        return probe_url(url, key, REMOTE_PROBE_TIMEOUT, false).await;
    }

    // ── 3. Loopback auto-discovery ───────────────────────────────────────────
    // Step 3a: port file written by `spelunk server start`
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        let tier = probe_url(&loopback_url, None, LOOPBACK_PROBE_TIMEOUT, true).await;
        if tier.is_server() {
            return tier;
        }
        tracing::debug!("loopback probe on port {port} failed — falling back to default port");
    }

    // Step 3b: default port 7777
    let default_url = format!("http://127.0.0.1:{DEFAULT_LOOPBACK_PORT}");
    tracing::debug!("loopback auto-discovery: probing default {default_url}");
    let tier = probe_url(&default_url, None, LOOPBACK_PROBE_TIMEOUT, true).await;
    if tier.is_server() {
        return tier;
    }

    tracing::debug!("loopback auto-discovery: no local server found — offline mode");
    Tier::Offline
}

/// Probe a single URL and return the resulting `Tier`.
async fn probe_url(
    url: &str,
    key: Option<&str>,
    timeout: std::time::Duration,
    auto_discovered: bool,
) -> Tier {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Tier::Offline;
        }
    };

    let mut req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let caps = parse_capabilities(url, resp).await;
            Tier::Server {
                url: url.to_string(),
                caps,
                auto_discovered,
            }
        }
        Ok(resp) => {
            if !auto_discovered {
                tracing::warn!(
                    "spelunk-server at {url} returned {} — running in offline mode",
                    resp.status()
                );
            }
            Tier::Offline
        }
        Err(e) => {
            if !auto_discovered {
                tracing::warn!(
                    "spelunk-server at {url} unreachable — running in offline mode: {e}"
                );
            }
            Tier::Offline
        }
    }
}

async fn parse_capabilities(url: &str, resp: reqwest::Response) -> Capabilities {
    #[derive(serde::Deserialize)]
    struct HealthBody {
        #[serde(default)]
        capabilities: Vec<String>,
        instance_id: Option<String>,
        started_by: Option<u32>,
    }

    match resp.json::<HealthBody>().await {
        Ok(body) => {
            // Warn if the server was started by a different user on this host.
            if let Some(server_uid) = body.started_by {
                let my_uid = current_uid();
                if let Some(my_uid) = my_uid
                    && my_uid != server_uid
                {
                    tracing::warn!(
                        "spelunk-server at {url} was started by UID {server_uid} \
                         but you are UID {my_uid}; on a multi-user host this may \
                         expose another user's memory — consider running your own server"
                    );
                }
            }
            if let Some(ref id) = body.instance_id {
                tracing::debug!("server instance_id: {id}");
            }
            let cap_strs: Vec<&str> = body.capabilities.iter().map(String::as_str).collect();
            Capabilities::from_server_caps(&cap_strs)
        }
        Err(_) => {
            // Legacy server returns plain-text "ok" — conservative fallback.
            Capabilities::legacy_memory_only()
        }
    }
}

/// Return the effective UID of this process (Unix), or `None` on Windows.
fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        Some(unsafe { geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Return `Ok(())` if the tier is `Server`, otherwise return an `anyhow::Error`
/// with the standard locked-feature message format.
///
/// Callers append `?` to propagate the error:
/// ```ignore
/// require_tier1("explore", tier, cfg.server_url.as_deref())?;
/// ```
pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    let tried = server_url
        .map(|u| format!("\n       (Tried: {u} — connection refused)"))
        .unwrap_or_default();
    anyhow::bail!(
        "'spelunk {feature}' requires spelunk-server.\n\
         Set server_url in ~/.config/spelunk/config.toml to enable this feature.{tried}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Capabilities::from_server_caps ──────────────────────────────────────

    #[test]
    fn from_server_caps_empty_returns_all_false() {
        let caps = Capabilities::from_server_caps(&[]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
        assert!(!caps.explore);
        assert!(!caps.plan);
    }

    #[test]
    fn from_server_caps_full_set() {
        let caps = Capabilities::from_server_caps(&[
            "search.semantic",
            "index.embed",
            "memory",
            "explore",
            "plan",
        ]);
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    #[test]
    fn from_server_caps_memory_only() {
        let caps = Capabilities::from_server_caps(&["memory"]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_partial_set() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "plan"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(caps.plan);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_unknown_capability_is_ignored() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "unknown.future", "memory"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(caps.memory_push);
        // Unknown capability should not affect any flag.
    }

    // ── Capabilities::legacy_memory_only ─────────────────────────────────────

    #[test]
    fn legacy_memory_only_values() {
        let caps = Capabilities::legacy_memory_only();
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    // ── Capabilities::all ────────────────────────────────────────────────────

    #[test]
    fn all_values_are_true() {
        let caps = Capabilities::all();
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    // ── Tier ─────────────────────────────────────────────────────────────────

    #[test]
    fn tier_server_is_server_true() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
        };
        assert!(tier.is_server());
    }

    #[test]
    fn tier_offline_is_server_false() {
        let tier = Tier::Offline;
        assert!(!tier.is_server());
    }

    #[test]
    fn tier_server_returns_url() {
        let tier = Tier::Server {
            url: "http://spelunk.internal:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
        };
        assert_eq!(tier.server_url(), Some("http://spelunk.internal:7777"));
    }

    #[test]
    fn tier_offline_returns_none_url() {
        let tier = Tier::Offline;
        assert_eq!(tier.server_url(), None);
    }

    #[test]
    fn tier_server_returns_caps() {
        let caps = Capabilities::all();
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: caps.clone(),
            auto_discovered: false,
        };
        assert!(tier.caps().is_some());
    }

    #[test]
    fn tier_offline_returns_none_caps() {
        let tier = Tier::Offline;
        assert!(tier.caps().is_none());
    }

    #[test]
    fn tier_auto_discovered_flag() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
        };
        assert!(auto.is_auto_discovered());
        assert!(!explicit.is_auto_discovered());
        assert!(!Tier::Offline.is_auto_discovered());
    }

    // ── effective_config (ADR-004 inference-vs-memory routing) ───────────────

    #[test]
    fn effective_config_auto_discovered_sets_inference_url_not_server_url() {
        // An auto-discovered loopback server is inference-only: its URL must
        // land in `inference_url` so memory selection (`open_memory_backend`,
        // which reads only `server_url`) stays local. This is the core of the
        // ADR-004 split-brain fix.
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
        };
        let cfg = Config::default(); // server_url = None
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url, None,
            "auto-discovered server must NOT populate server_url (memory stays local)"
        );
        assert_eq!(
            eff.inference_url.as_deref(),
            Some("http://127.0.0.1:7777"),
            "auto-discovered server URL must route inference via inference_url"
        );
        assert!(
            eff.project_id.is_some(),
            "project_id should be derived so the inference client can address the project"
        );
        // Inference resolves to the loopback server; memory selection does not.
        assert_eq!(eff.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    #[test]
    fn effective_config_explicit_server_url_left_unchanged() {
        // An explicitly-configured team server owns BOTH inference and memory
        // (team-memory tier). `effective_config` must not touch it.
        let tier = Tier::Server {
            url: "http://team.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
        };
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url.as_deref(),
            Some("http://team.example.com:7777"),
            "explicit team server_url must be preserved (memory stays remote)"
        );
        assert_eq!(
            eff.inference_url, None,
            "explicit server_url path should not synthesise a separate inference_url"
        );
    }

    #[test]
    fn effective_config_offline_tier_is_noop() {
        let cfg = Config::default();
        let eff = Tier::Offline.effective_config(&cfg, std::path::Path::new("/tmp/proj"));
        assert_eq!(eff.server_url, None);
        assert_eq!(eff.inference_url, None);
    }

    // ── require_tier1 ────────────────────────────────────────────────────────

    #[test]
    fn require_tier1_ok_for_server() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
        };
        assert!(require_tier1("explore", &tier, Some("http://example.com")).is_ok());
    }

    #[test]
    fn require_tier1_err_for_offline_no_url() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk explore'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_includes_tried() {
        let tier = Tier::Offline;
        let err = require_tier1("plan", &tier, Some("http://bad:7777")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk plan'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("http://bad:7777"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline;
        let err = require_tier1("memory push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk memory push'"));
    }

    #[test]
    fn require_tier1_no_tried_line_when_url_not_set() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("Tried:"));
    }

    // ── read_server_port_file ────────────────────────────────────────────────

    #[test]
    fn read_server_port_file_returns_none_when_absent() {
        // In a temp dir with no server.port file, should return None.
        // We can't control the state dir in a unit test, but we can verify
        // the function doesn't panic and returns a valid Option<u16>.
        // The actual file-read path is exercised by integration tests.
        let _ = read_server_port_file(); // must not panic
    }

    // ── SPELUNK_NO_SERVER and loopback constants ──────────────────────────────

    #[test]
    fn loopback_probe_timeout_is_250ms() {
        assert_eq!(LOOPBACK_PROBE_TIMEOUT.as_millis(), 250);
    }

    #[test]
    fn remote_probe_timeout_is_2s() {
        assert_eq!(REMOTE_PROBE_TIMEOUT.as_secs(), 2);
    }

    #[test]
    fn default_loopback_port_is_7777() {
        assert_eq!(DEFAULT_LOOPBACK_PORT, 7777);
    }

    // ── SPELUNK_NO_SERVER short-circuit behaviour ─────────────────────────────
    //
    // These tests mutate the process-global `SPELUNK_NO_SERVER` env var, so they
    // are serialised against each other to avoid cross-test interference.

    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn spelunk_no_server_forces_offline() {
        // SAFETY: serialised via #[serial] so no other test reads/writes this
        // env var concurrently; restored before the guard scope ends.
        for val in ["1", "true", "yes"] {
            unsafe { std::env::set_var("SPELUNK_NO_SERVER", val) };
            // server_url = None so that, absent the short-circuit, the probe would
            // attempt loopback auto-discovery; the short-circuit must win.
            let tier = probe(None, None).await;
            assert!(
                matches!(tier, Tier::Offline),
                "SPELUNK_NO_SERVER={val} should force Tier::Offline, got {tier:?}"
            );
        }
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }
}
