//! Server probing: loopback auto-discovery, explicit `server_url` health
//! checks, and the per-process cached `Tier` this crate reads everywhere.

use tokio::sync::OnceCell;

use crate::config::Config;

use super::diagnostics::{
    ConnFailure, cert_trust_hint, error_chain, find_rustls_cause, record_explicit_probe_failure,
};
use super::state::{Capabilities, EmbedderState, ServerLimits};
use super::tier::Tier;

/// The single state file directory resolver for the whole CLI:
/// `~/.local/state/spelunk/`, or `SPELUNK_STATE_DIR` when set.
///
/// Every reader and writer of runtime state goes through this one function:
/// `spelunk server start/stop/status/logs` (server pid/port/log/db-path
/// files, `cli/cmd/server.rs`), the embed worker's liveness files
/// (`cli/cmd/embed_worker.rs`), and this module's own loopback
/// auto-discovery probe below. A second, independent resolution here was a
/// real bug: it let the override apply to some readers/writers and not
/// others, so a status reader could miss a worker's pid file written to a
/// different directory (or vice versa) and misreport liveness.
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
/// (spelunk#316).
///
/// `SPELUNK_STATE_DIR` is a supported override of the entire path, not
/// dev-only cruft: it is load-bearing on Windows CI, where `dirs::home_dir()`
/// 6.x calls `SHGetKnownFolderPath` (a Windows Registry lookup) rather than
/// reading `USERPROFILE`, making per-process environment overrides of `HOME`
/// ineffective. It is also used directly by end users who want state files
/// somewhere other than the default (e.g. an ephemeral or sandboxed HOME).
///
/// Errors only when the home directory can't be resolved and no override is
/// set.
pub(crate) fn spelunk_state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("SPELUNK_STATE_DIR") {
        return Ok(std::path::PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join("spelunk"))
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

/// Read the port written by `spelunk server start` into
/// `~/.local/state/spelunk/server.port`. Returns `None` if absent or unreadable.
fn read_server_port_file() -> Option<u16> {
    let path = spelunk_state_dir().ok()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}

static TIER: OnceCell<Tier> = OnceCell::const_new();

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
/// configs: they would always see the tier determined by the first call.
pub async fn get_tier(cfg: &Config) -> &'static Tier {
    // An *explicit* offline mode (config `mode = "offline"`,
    // `SPELUNK_MODE=offline`, or the `SPELUNK_NO_SERVER=1` kill-switch) skips all
    // server probes: the user has asked for a provable no-cloud run.
    //
    // The *defaulted* offline (no `server_url` and no explicit `mode`) must NOT
    // skip probing: loopback auto-discovery is inference-only (it never owns
    // memory, ADR-004) and is what gives a local-only project semantic search.
    // Conflating the two would silently disable the loopback embedder.
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    let url = cfg.server_url.clone();
    let server_ca = cfg.server_ca.clone();
    TIER.get_or_init(|| async move {
        if explicit_offline {
            tracing::debug!("sync mode is explicitly offline: skipping all server probes");
            return Tier::Offline;
        }
        probe(
            url.as_deref(),
            server_ca.as_deref().map(std::path::Path::new),
        )
        .await
    })
    .await
}

/// One fresh, uncached tier probe, honouring the same explicit-offline
/// short-circuits as [`get_tier`].
///
/// For pollers only: `get_tier`'s process-lifetime cache pins whatever state
/// the first probe saw, so a caller that must observe a *transition* (the
/// detached embed worker waiting for the embedder to finish loading) has to
/// re-probe. Everything else should keep using [`get_tier`].
pub async fn probe_tier_fresh(cfg: &Config) -> Tier {
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    if explicit_offline {
        return Tier::Offline;
    }
    probe(
        cfg.server_url.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )
    .await
}

/// Remote-server probe timeout (explicit `server_url` in config/env).
const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Loopback probe timeout (auto-discovery of a locally-running server).
const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Default loopback port for `spelunk-server`.
const DEFAULT_LOOPBACK_PORT: u16 = 7777;

async fn probe(url: Option<&str>, server_ca: Option<&std::path::Path>) -> Tier {
    // ── 1. SPELUNK_NO_SERVER short-circuit ───────────────────────────────────
    if matches!(
        std::env::var("SPELUNK_NO_SERVER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        tracing::debug!("SPELUNK_NO_SERVER set: skipping all server probes");
        return Tier::Offline;
    }

    // ── 2. Explicit server_url from config / env ─────────────────────────────
    if let Some(url) = url {
        return match probe_url(url, REMOTE_PROBE_TIMEOUT, false, server_ca).await {
            Ok(tier) => tier,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        };
    }

    // ── 3. Loopback auto-discovery ───────────────────────────────────────────
    probe_loopback().await
}

/// Loopback auto-discovery only: never consults `cfg.server_url`.
///
/// Step 3a: port file written by `spelunk server start`. Step 3b: fall back to
/// the default port 7777. Both steps use the 250 ms loopback timeout and treat
/// any probe failure as `Tier::Offline` (never a hard error: loopback
/// auto-discovery finding nothing is the normal "no local server" case, not a
/// misconfiguration).
///
/// Split out of [`probe`] so [`get_inference_tier`] can run the identical
/// discovery independent of an explicit `server_url`: `local_first` always
/// prefers the local embedder, even when `server_url` targets a remote.
async fn probe_loopback() -> Tier {
    // Step 3a: port file written by `spelunk server start`
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        // Loopback probes never produce hard errors (auto_discovered=true), so unwrap is safe.
        // Loopback is plaintext http: a custom CA is irrelevant here.
        let tier = probe_url(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true, None)
            .await
            .unwrap_or(Tier::Offline);
        if tier.is_server() {
            return tier;
        }
        tracing::debug!("loopback probe on port {port} failed: falling back to default port");
    }

    // Step 3b: default port 7777
    let default_url = format!("http://127.0.0.1:{DEFAULT_LOOPBACK_PORT}");
    tracing::debug!("loopback auto-discovery: probing default {default_url}");
    let tier = probe_url(&default_url, LOOPBACK_PROBE_TIMEOUT, true, None)
        .await
        .unwrap_or(Tier::Offline);
    if tier.is_server() {
        return tier;
    }

    tracing::debug!("loopback auto-discovery: no local server found: offline mode");
    Tier::Offline
}

/// Resolve the tier used specifically to route **inference** (embeddings +
/// LLM), which can differ from [`get_tier`]'s general-purpose capability tier.
///
/// Per the founder's 2026-07-23 routing decision (ADR-004
/// revision): `local_first` (and the serde-default mode reached when no
/// `server_url` is set) always routes inference to the local loopback
/// embedder, even when `server_url` is explicitly configured — there, an
/// explicit `server_url` is a memory sync replica only, never an inference
/// target. Only `cloud_first` lets an explicit `server_url` serve inference
/// too, in which case this reuses [`get_tier`]'s cached probe of that URL
/// (unchanged behaviour for that mode).
///
/// Explicit offline (`SPELUNK_NO_SERVER` / `mode = "offline"`) skips every
/// probe, mirroring `get_tier`.
///
/// Not cached via a `OnceCell` like `get_tier`: `local_first` always runs a
/// fresh loopback probe rather than reusing whatever `get_tier` already
/// cached for `cfg.server_url` (a different, unrelated target in that mode).
pub async fn get_inference_tier(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Cached).await
}

/// Fresh-probing counterpart to [`get_inference_tier`], for callers that must
/// observe a *transition* rather than a point-in-time snapshot: the detached
/// embed worker's readiness wait (`wait_for_embedder`) polls repeatedly for
/// the embedder to flip from `loading` to `ready`, so it can never read
/// through a value pinned by [`get_tier`]'s per-process `OnceCell`.
///
/// Routes identically to [`get_inference_tier`] (same mode-based branch,
/// same explicit-offline short-circuit), except the `cloud_first` branch
/// re-probes the configured `server_url` on every call via
/// [`probe_tier_fresh`] instead of reading [`get_tier`]'s cache: the same
/// relationship `probe_tier_fresh` already has to `get_tier`, applied one
/// level up. The `local_first` branch needs no change here: it already calls
/// `probe_loopback()` directly, which was never cached.
pub async fn get_inference_tier_fresh(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Fresh).await
}

/// Which probe the `cloud_first` branch of [`inference_tier`] takes: the
/// per-process cache ([`get_tier`], for one-shot callers) or a fresh probe
/// ([`probe_tier_fresh`], for pollers that must observe a transition).
enum CloudBranchProbe {
    Cached,
    Fresh,
}

/// Shared mode-based routing behind [`get_inference_tier`] and
/// [`get_inference_tier_fresh`]; see their docs for the routing rules. The two
/// differ only in which probe serves the `cloud_first` branch.
async fn inference_tier(cfg: &Config, cloud_branch: CloudBranchProbe) -> Tier {
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    if explicit_offline {
        return Tier::Offline;
    }
    if cfg.resolve_mode() == spelunk_core::config::SyncMode::CloudFirst {
        return match cloud_branch {
            CloudBranchProbe::Cached => get_tier(cfg).await.clone(),
            CloudBranchProbe::Fresh => probe_tier_fresh(cfg).await,
        };
    }
    probe_loopback().await
}

/// Probe a single URL and return the resulting `Tier`, or a hard error string
/// for an explicit-URL dimension mismatch.
///
/// `auto_discovered = true` means the URL was found via the loopback probe rather
/// than set explicitly in config or environment. The distinction controls whether a
/// dimension mismatch is a soft downgrade (loopback) or a hard error (explicit URL).
async fn probe_url(
    url: &str,
    timeout: std::time::Duration,
    auto_discovered: bool,
    server_ca: Option<&std::path::Path>,
) -> Result<Tier, String> {
    // Non-loopback plaintext http:// is invalid config: reject before sending
    // anything. No opt-out: the fix is always "use https, or loopback".
    spelunk_core::config::validate_transport_url(url)?;

    let builder = match spelunk_core::config::apply_server_ca(reqwest::Client::builder(), server_ca)
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("could not load custom CA for server probe: {e}");
            return Ok(Tier::Offline);
        }
    };
    let client = match builder.timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Ok(Tier::Offline);
        }
    };

    // `/v1/health` is an unauthenticated endpoint: do not send a bearer to it.
    let req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let (caps, server_dim, embedder_state, server_limits) = parse_health(url, resp).await;

            // If the server advertises index.embed, its embedding dimension must match ours.
            if caps.index_embed && server_dim != 0 {
                let expected = spelunk_core::embeddings::EMBEDDING_DIM;
                if server_dim != expected {
                    if auto_discovered {
                        // Loopback auto-discovery: downgrade gracefully: the user did
                        // not explicitly configure this server.
                        tracing::warn!(
                            "spelunk-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim. Ignoring loopback server. \
                             Restart the server (`spelunk server start`) or set \
                             SPELUNK_NO_SERVER=1 to suppress this probe."
                        );
                        return Ok(Tier::Offline);
                    } else {
                        // Explicit server_url: surface as a hard error so the user
                        // gets actionable guidance before any command runs.
                        return Err(format!(
                            "spelunk-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim.\n\
                             Upgrade or replace the server, or remove server_url from \
                             ~/.config/spelunk/config.toml."
                        ));
                    }
                }
            }

            Ok(Tier::Server {
                url: url.to_string(),
                caps,
                auto_discovered,
                embedder_state,
                server_limits,
            })
        }
        Ok(resp) => {
            if !auto_discovered {
                tracing::warn!(
                    "spelunk-server at {url} returned {}: running in offline mode",
                    resp.status()
                );
            }
            Ok(Tier::Offline)
        }
        Err(e) => {
            if !auto_discovered {
                let chain = error_chain(&e);
                match find_rustls_cause(&e) {
                    Some(cause) => {
                        record_explicit_probe_failure(ConnFailure::Tls(cause.clone()));
                        let hint = if server_ca.is_some() {
                            cert_trust_hint()
                        } else {
                            String::new()
                        };
                        tracing::warn!(
                            "spelunk-server at {url} reachable, but TLS trust failed: {cause}; \
                             running in offline mode.\n  full error chain: {chain}{hint}"
                        );
                    }
                    None => {
                        record_explicit_probe_failure(ConnFailure::Unreachable);
                        tracing::warn!(
                            "spelunk-server at {url} unreachable, running in offline mode: {chain}"
                        );
                    }
                }
            }
            Ok(Tier::Offline)
        }
    }
}

/// Parse the health response body and return `(Capabilities, embedding_dim,
/// embedder_state, server_limits)`.
///
/// `embedding_dim` is `0` when the field is absent (old server without the field)
/// or when no embedder is loaded. A `0` dim skips the dimension check in `probe_url`
/// for backward compatibility.
///
/// `embedder_state` mirrors the `/v1/health` `embedder.state` field
/// (`embedder: { state, detail }`). It is `Unknown` when the sub-object is
/// absent (older server) or the body is legacy plain-text.
///
/// `server_limits` mirrors `/v1/health`'s `limits` object. `None` when absent :
/// a server that pre-dates the field, which is exactly the version-skew case:
/// it still enforces the old blanket 30s `/index/embed` budget with no
/// exemption, regardless of what the CLI's own calibration would otherwise
/// target.
async fn parse_health(
    url: &str,
    resp: reqwest::Response,
) -> (Capabilities, usize, EmbedderState, Option<ServerLimits>) {
    #[derive(serde::Deserialize)]
    struct EmbedderBody {
        #[serde(default)]
        state: EmbedderState,
    }

    #[derive(serde::Deserialize)]
    struct HealthBody {
        #[serde(default)]
        capabilities: Vec<String>,
        instance_id: Option<String>,
        started_by: Option<u32>,
        /// Embedding dimension produced by this server's embedder.
        /// Absent on old servers that pre-date this field; defaults to 0 (skip check).
        #[serde(default)]
        embedding_dim: usize,
        /// Embedder readiness sub-object. Absent on older servers
        /// → `embedder_state` stays `Unknown`.
        #[serde(default)]
        embedder: Option<EmbedderBody>,
        /// Server-enforced `/index/embed` limits.
        /// Absent on older servers → `server_limits` stays `None`.
        #[serde(default)]
        limits: Option<ServerLimits>,
        /// Whether the server accepts a client-pushed embedding vector on
        /// `POST /memory/batch`. Top-level bool, not a
        /// `capabilities` entry. Absent on servers without the accept side
        /// (older servers, the OSS team server) → defaults false.
        #[serde(default)]
        accepts_pushed_vectors: bool,
    }

    match resp.json::<HealthBody>().await {
        Ok(body) => {
            let embedder_state = body
                .embedder
                .as_ref()
                .map(|e| e.state)
                .unwrap_or(EmbedderState::Unknown);
            // Warn if the server was started by a different user on this host.
            if let Some(server_uid) = body.started_by {
                let my_uid = current_uid();
                if let Some(my_uid) = my_uid
                    && my_uid != server_uid
                {
                    tracing::warn!(
                        "spelunk-server at {url} was started by UID {server_uid} \
                         but you are UID {my_uid}; on a multi-user host this may \
                         expose another user's memory: consider running your own server"
                    );
                }
            }
            if let Some(ref id) = body.instance_id {
                tracing::debug!("server instance_id: {id}");
            }
            let cap_strs: Vec<&str> = body.capabilities.iter().map(String::as_str).collect();
            let mut caps = Capabilities::from_server_caps(&cap_strs);
            // `accepts_pushed_vectors` is a top-level health bool, not a
            // `capabilities` array entry, so it is applied after the array parse.
            caps.accepts_pushed_vectors = body.accepts_pushed_vectors;
            (caps, body.embedding_dim, embedder_state, body.limits)
        }
        Err(_) => {
            // Legacy server returns plain-text "ok": conservative fallback.
            // embedding_dim = 0 skips the dimension check; state Unknown; no limits.
            (
                Capabilities::legacy_memory_only(),
                0,
                EmbedderState::Unknown,
                None,
            )
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

#[cfg(test)]
mod tests {
    use super::super::diagnostics::{
        explicit_probe_failure, reset_explicit_probe_failure_for_test,
    };
    use super::*;

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

    // ── Embedding-dim pre-flight checks ──────────────────────────────────────

    // Helper: build a health JSON body with the given capabilities and dim.
    fn health_body(caps: &[&str], dim: usize) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "0.9.0",
            "capabilities": caps,
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": dim
        })
    }

    // Auto-discovered loopback server with wrong dim → `Tier::Offline` (soft downgrade).
    #[tokio::test]
    async fn probe_loopback_dim_mismatch_downgrades_to_offline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Return a health body claiming 768-dim embeddings: wrong for the current CLI (896).
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                768,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Offline)),
            "auto-discovered loopback with wrong dim must downgrade to Offline; got {result:?}"
        );
    }

    // Auto-discovered loopback server with correct dim → `Tier::Server`.
    #[tokio::test]
    async fn probe_loopback_dim_match_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "auto-discovered loopback with correct dim must return Server; got {result:?}"
        );
    }

    // ── accepts_pushed_vectors (top-level health bool) ──────────────────────────

    // A server advertising `accepts_pushed_vectors: true` must parse into
    // `caps.accepts_pushed_vectors == true`: the gate the sync push reads
    // before attaching a client-computed vector.
    #[tokio::test]
    async fn probe_url_parses_accepts_pushed_vectors_true() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], spelunk_core::embeddings::EMBEDDING_DIM);
        body["accepts_pushed_vectors"] = serde_json::json!(true);
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        assert!(
            tier.caps().unwrap().accepts_pushed_vectors,
            "health `accepts_pushed_vectors: true` must set the capability"
        );
    }

    // A server that omits the field (older server, or the OSS team server)
    // must default to `false`: the push stays text-only there.
    #[tokio::test]
    async fn probe_url_accepts_pushed_vectors_defaults_false_when_absent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `health_body` carries no `accepts_pushed_vectors` field.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        assert!(
            !tier.caps().unwrap().accepts_pushed_vectors,
            "absent `accepts_pushed_vectors` must default to false (text-only)"
        );
    }

    // ── ServerLimits parsing (/v1/health `limits` object) ──────────────────────

    // A server that DOES advertise `limits` must have it parsed into
    // `Tier::Server.server_limits`. This is the non-version-skew case: a
    // current-build server carrying the `/index/embed` timeout exemption.
    #[tokio::test]
    async fn probe_url_parses_server_limits_when_present() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(
            &["memory", "index.embed", "search.semantic"],
            spelunk_core::embeddings::EMBEDDING_DIM,
        );
        body["limits"] = serde_json::json!({
            "embed_request_timeout_secs": 1800,
            "max_batch_chunks": 256,
            "embedder_token_cap": 5792,
        });
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        let limits = result
            .server_limits()
            .expect("server_limits must be Some when the health body carries `limits`");
        assert_eq!(limits.embed_request_timeout_secs, 1800);
        assert_eq!(limits.max_batch_chunks, 256);
        assert_eq!(limits.embedder_token_cap, Some(5792));
    }

    // A server that does NOT advertise `limits` (pre-dates the field) must
    // leave `Tier::Server.server_limits` as `None`: this is the exact
    // version-skew case: an old server still enforcing the legacy 30s
    // `/index/embed` budget with no exemption. `None` must never be
    // confused with "no limit" by a caller.
    #[tokio::test]
    async fn probe_url_server_limits_none_when_absent_legacy_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // health_body() deliberately has no `limits` field (models a server
        // that pre-dates this fix).
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        assert_eq!(
            result.server_limits(),
            None,
            "a server that omits `limits` must be treated as version-skewed, not unlimited"
        );
    }

    // `embedder_token_cap` specifically must round-trip as `None` when the
    // server reports it as JSON `null` (e.g. embedder not ready, or an
    // external non-native backend with no known cap): distinct from the
    // whole `limits` object being absent.
    #[tokio::test]
    async fn probe_url_parses_server_limits_with_null_token_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], 0);
        body["limits"] = serde_json::json!({
            "embed_request_timeout_secs": 1800,
            "max_batch_chunks": 256,
            "embedder_token_cap": null,
        });
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        let limits = result.server_limits().expect("limits object was present");
        assert_eq!(limits.embedder_token_cap, None);
    }

    // Auto-discovered loopback server with no embedder (dim 0) → `Tier::Server`
    // (dim 0 means no `index.embed` check is relevant).
    #[tokio::test]
    async fn probe_loopback_dim_zero_no_embedder_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // No index.embed capability, dim 0.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "server with no embedder (dim 0) must still return Server; got {result:?}"
        );
    }

    // Explicit server_url with wrong dim → hard `Err` with an actionable message.
    #[tokio::test]
    async fn probe_explicit_url_dim_mismatch_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                768,
            )))
            .mount(&server)
            .await;

        // auto_discovered = false → explicit server_url path → must be a hard Err.
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            result.is_err(),
            "explicit server_url with wrong dim must return Err; got {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("768"),
            "error must mention the server's dim (768): {msg}"
        );
        let expected = spelunk_core::embeddings::EMBEDDING_DIM;
        assert!(
            msg.contains(&expected.to_string()),
            "error must mention the expected dim ({expected}): {msg}"
        );
        assert!(
            msg.contains("server_url"),
            "error must mention 'server_url' for actionable guidance: {msg}"
        );
    }

    // ── transport-scheme validation ──────────────────────────────────────────

    // A non-loopback `http://` URL must be rejected before any request is
    // sent: no mock is mounted, so a request would fail with "connection
    // refused" or similar rather than surfacing the validation error; the
    // assertion on the error message proves the reject happened pre-flight.
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // Deliberately no MockServer / no listener on this address: if
        // `probe_url` tried to send a request it would get a connection error,
        // not this validation message.
        let result = probe_url("http://team-server:7777", REMOTE_PROBE_TIMEOUT, false, None).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    // Same rejection applies to the loopback auto-discovery path (defensive;
    // auto-discovery URLs are always loopback in practice).
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_even_when_auto_discovered() {
        let result = probe_url(
            "http://team-server:7777",
            LOOPBACK_PROBE_TIMEOUT,
            true,
            None,
        )
        .await;
        assert!(result.is_err());
    }

    // Loopback `http://` and `https://` URLs are accepted (proceed to the
    // actual health request against a mock server).
    #[tokio::test]
    async fn probe_url_accepts_loopback_http_and_https() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        // wiremock serves over http on 127.0.0.1, which is loopback.
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "loopback http:// must be accepted; got {result:?}"
        );
    }

    // `/v1/health` must never carry an `Authorization` header: it is an
    // unauthenticated endpoint.
    #[tokio::test]
    async fn probe_url_health_request_carries_no_bearer_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .expect(1)
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Server { .. })), "got {result:?}");

        // Assert no request in wiremock's log carried an Authorization header.
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "the /v1/health probe must not send an Authorization header"
        );
    }

    // Health body carrying the PR-A `embedder: { state, detail }` sub-object.
    fn health_body_with_embedder(state: &str) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "0.9.1",
            "capabilities": ["memory"],
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": 0,
            "embedder": { "state": state, "detail": null }
        })
    }

    // `probe_url` must surface the server's `embedder.state` on `Tier::Server`.
    #[tokio::test]
    async fn probe_url_carries_embedder_state_loading() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("loading")),
            )
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
    }

    #[tokio::test]
    async fn probe_url_carries_embedder_state_unavailable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("unavailable")),
            )
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Unavailable));
    }

    // A server that pre-dates the `embedder` field → `Unknown` (not an error).
    #[tokio::test]
    async fn probe_url_absent_embedder_field_is_unknown() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `health_body` (no `embedder` key) simulates an older server.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Unknown));
    }

    // ── get_tier process-cache semantics ─────────────────────────────────────

    // `TIER` is a `OnceCell`: `get_tier` must probe at most once per process
    // and every later call must return the identical cached `Tier`, not
    // re-probe. This is what makes `EXPLICIT_PROBE_FAILURE` safe to read from
    // `Tier::Offline` rendering: there is no later probe in the same process
    // that could silently swap a fresh success in underneath a stale failure
    // annotation (or vice versa).
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn get_tier_probes_at_most_once_and_caches_the_result() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener); // nothing listens on `port` from here on: connection refused.

        let cfg = Config {
            server_url: Some(format!("http://127.0.0.1:{port}")),
            ..Default::default()
        };

        let first = get_tier(&cfg).await;
        assert!(matches!(first, Tier::Offline), "got {first:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "connection-refused must classify as Unreachable, not Tls"
        );

        let second = get_tier(&cfg).await;
        assert!(
            std::ptr::eq(first, second),
            "get_tier must return the same cached &'static Tier on a later call, not re-probe"
        );
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "a cached second get_tier call must not disturb the recorded probe failure"
        );
    }

    // ── classification matrix: real reqwest errors, not hand-built chains ────

    // A genuine TCP connection-refused error through the real `reqwest`
    // client must classify as `Unreachable`, never `Tls`: no TLS layer is
    // ever reached, so `find_rustls_cause` must return `None` on it.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_connection_refused_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "connection-refused must not be mislabelled as a TLS trust failure"
        );
    }

    // A genuine client-side timeout (the peer accepts the TCP connection but
    // never answers) must also classify as `Unreachable`, not `Tls`: a slow
    // or hung server is not a certificate problem.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_timeout_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        std::thread::spawn(move || {
            // Accept and hold every connection open without ever writing a
            // response, forcing the client-side timeout below to fire.
            for stream in listener.incoming().flatten() {
                std::thread::sleep(std::time::Duration::from_secs(5));
                drop(stream);
            }
        });

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, std::time::Duration::from_millis(100), false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "a timeout must not be mislabelled as a TLS trust failure"
        );
    }

    // A reachable server that answers with a non-2xx status (e.g. a
    // misconfigured reverse proxy, a 500, garbage) is neither `[tls: ...]`
    // nor `[unreachable]`: the transport and TLS both worked fine. This
    // path must leave `EXPLICIT_PROBE_FAILURE` unset entirely.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_non_success_status_does_not_set_any_probe_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Must pass regardless of what other `capability::` test populated
        // EXPLICIT_PROBE_FAILURE earlier in this process, so reset first
        // rather than relying on execution order.
        reset_explicit_probe_failure_for_test();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            None,
            "a reachable server answering with a non-2xx status must not populate \
             EXPLICIT_PROBE_FAILURE: that would render a stale/wrong [tls:] or \
             [unreachable] label for a request that was neither"
        );
    }

    // Auto-discovered (loopback) probe failures must never populate
    // `EXPLICIT_PROBE_FAILURE`: that cache exists only to annotate an
    // *explicit* `server_url` miss. A common "no local server running"
    // loopback miss must not leave behind a failure cause that a later
    // status render could misattribute to an unrelated explicit `server_url`.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_auto_discovered_connection_refused_leaves_probe_failure_unset() {
        // Must pass regardless of what other `capability::` test populated
        // EXPLICIT_PROBE_FAILURE earlier in this process, so reset first
        // rather than relying on execution order.
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, LOOPBACK_PROBE_TIMEOUT, true, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            None,
            "loopback auto-discovery misses must never populate EXPLICIT_PROBE_FAILURE"
        );
    }

    // ── get_inference_tier (2026-07-23 founder decision) ───
    //
    // These tests set `SPELUNK_STATE_DIR` / `SPELUNK_NO_SERVER`, both
    // process-global. Reusing the `spelunk_no_server_env` serial group (rather
    // than a new name) keeps them mutually exclusive with
    // `spelunk_no_server_forces_offline` above too: `get_inference_tier` reads
    // `SPELUNK_NO_SERVER` internally, so it must never run concurrently with a
    // test that transiently sets it.

    // `local_first` (the default reached once `server_url` is set, with no
    // explicit `mode`) must probe the LOCAL loopback embedder for inference,
    // never the configured `server_url`. The loopback mock is discovered via
    // the `server.port` file (step 3a); `server_url` is left pointed at an
    // address nothing mounts anything on, so the test would fail loudly
    // (connection error, not a silent pass) if the code ever tried it.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn get_inference_tier_local_first_prefers_loopback_over_explicit_server_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&loopback)
            .await;

        let loopback_port: u16 = loopback
            .uri()
            .rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric");

        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{loopback_port}\n")).unwrap();

        let prev_state_dir = std::env::var_os("SPELUNK_STATE_DIR");
        unsafe { std::env::set_var("SPELUNK_STATE_DIR", &state_dir) };

        let cfg = Config {
            // Deliberately never mocked: any accidental fallback to this
            // "remote" would surface as a connection/DNS error, not a silent
            // wrong-but-passing result.
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            spelunk_core::config::SyncMode::LocalFirst
        );

        let tier = get_inference_tier(&cfg).await;

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                None => std::env::remove_var("SPELUNK_STATE_DIR"),
            }
        }

        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{loopback_port}")).as_deref(),
            "local_first must route inference to the loopback server, not the \
             configured (and unreachable) server_url; got {tier:?}"
        );
    }

    // Explicit offline (`mode = "offline"`) must short-circuit before any
    // probe, exactly like `get_tier`. `server_url` is set to an address
    // nothing mounts anything on, so any attempted probe would hang/error
    // rather than silently returning `Offline` for the right reason.
    //
    // Uses `cfg.mode = Some(SyncMode::Offline)` rather than
    // `SPELUNK_NO_SERVER=1` deliberately: that env var is process-global and
    // read by every concurrently-running test's `probe()`/`get_tier()`
    // call (e.g. `get_tier_probes_at_most_once_and_caches_the_result`
    // above, which is not in this lock group), so mutating it here would
    // reintroduce the exact cross-test race this comment is warning about.
    // `mode` is per-`Config` and carries no such risk.
    #[tokio::test]
    async fn get_inference_tier_explicit_offline_short_circuits() {
        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(spelunk_core::config::SyncMode::Offline),
            ..Default::default()
        };
        let tier = get_inference_tier(&cfg).await;
        assert!(matches!(tier, Tier::Offline), "got {tier:?}");
    }

    // `get_inference_tier_fresh`'s `cloud_first` branch must re-probe the
    // server on every call, never freezing on an earlier observation. This
    // is the one behavioural difference from `get_inference_tier` (whose
    // `cloud_first` branch reuses `get_tier`'s process-lifetime cache) and
    // the entire reason `wait_for_embedder` uses the `_fresh` variant: a
    // bug that made this branch delegate to `get_tier` too (i.e. collapse
    // to being identical to `get_inference_tier`) would still pass every
    // other `get_inference_tier_fresh` test in this file, since those only
    // ever make a single call each. This test calls it twice against a
    // mock whose response changes between calls and asserts the second
    // call observes the change, directly at the tier-fetch level (not
    // indirected through `wait_for_embedder`'s poll loop).
    //
    // Deliberately does not touch `get_tier`/`TIER` (the process-wide
    // `OnceCell`) at all, unlike a test that called `get_inference_tier`'s
    // `Cached` branch would have to: that cell is shared by every test in
    // this binary with no reset hook, so asserting on it directly here
    // would make this test's pass/fail depend on unrelated test ordering.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn get_inference_tier_fresh_cloud_first_reprobes_every_call() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        let server = MockServer::start().await;
        // First health check: embedder still loading, no index.embed.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("loading")),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Every call after the first: embedder ready.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let cfg = Config {
            server_url: Some(server.uri()),
            project_id: Some("team/proj".to_string()),
            mode: Some(spelunk_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };

        let first = get_inference_tier_fresh(&cfg).await;
        assert_eq!(
            first.embedder_state(),
            Some(EmbedderState::Loading),
            "first call must observe the first mock response; got {first:?}"
        );

        let second = get_inference_tier_fresh(&cfg).await;
        assert!(
            matches!(second.caps(), Some(c) if c.index_embed),
            "second call must re-probe and observe the loading -> ready \
             transition, not return a value pinned by the first call; got {second:?}"
        );
    }
}
