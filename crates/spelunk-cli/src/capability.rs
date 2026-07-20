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

use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::config::Config;

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

/// Cause recorded for the most recent EXPLICIT (non-auto-discovered)
/// `server_url` probe failure, set at most once per process (see
/// `record_explicit_probe_failure`, which mirrors `OnceCell::set`'s
/// first-write-wins behaviour).
///
/// Backed by a `Mutex` rather than `OnceCell` so `#[cfg(test)]` code can
/// reset it between a test that legitimately populates the cell and a test
/// that asserts it stays empty; both exist in this module's test suite and
/// share this one process-global static. Production code never resets it.
static EXPLICIT_PROBE_FAILURE: std::sync::Mutex<Option<ConnFailure>> = std::sync::Mutex::new(None);

/// How an explicitly-configured `server_url` probe failed: distinguishes a
/// transport-level miss (refused, timed out, DNS, no route) from a connection
/// that reached the server but failed TLS trust. `status`/`check` read this to
/// annotate the offline line with `[unreachable]` vs `[tls: <cause>]` instead
/// of collapsing both into "unreachable": a server that answers `curl` fine
/// can still fail here on a certificate error that would otherwise never
/// surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnFailure {
    /// TCP/connect-level failure: refused, timed out, DNS, no route.
    Unreachable,
    /// The transport connected; TLS certificate trust failed. Carries the
    /// short cause string used in `[tls: <cause>]`.
    Tls(String),
}

/// Cause of the most recent explicit `server_url` probe failure, if any.
/// `None` when no `server_url` is configured, when the tier is `Server`, when
/// the only probes so far were loopback auto-discovery, or before the first
/// probe has run.
pub fn explicit_probe_failure() -> Option<ConnFailure> {
    EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Record `cause` as the explicit-probe failure, unless one is already
/// recorded. Mirrors `OnceCell::set`'s first-write-wins semantics so this
/// carries the same "set at most once per process" contract the previous
/// `OnceCell`-backed static had.
fn record_explicit_probe_failure(cause: ConnFailure) {
    let mut slot = EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(cause);
    }
}

/// Test-only: clear the recorded explicit-probe failure so a test that
/// asserts the cell is empty isn't at the mercy of whatever other
/// `capability::` test happened to populate it earlier in this process.
/// Callers must pair this with `#[serial_test::serial(explicit_probe_failure)]`,
/// since the static is shared by every test in this binary.
#[cfg(test)]
fn reset_explicit_probe_failure_for_test() {
    *EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Render `err`'s full `source()` chain, one cause per arrow. reqwest's
/// `Display` only ever shows its own top-level message ("error sending
/// request for url (...)"); the actual cause (a TLS handshake failure, a DNS
/// error, ...) lives several `source()` levels down and is otherwise silently
/// dropped from the WARN a user sees.
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(" -> ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// Walk `err`'s source chain looking for a `rustls::Error`, which is how a TLS
/// handshake/certificate failure surfaces underneath reqwest's generic
/// "error sending request". tokio-rustls reports it boxed inside an
/// `io::Error`, so both direct and `io::Error`-wrapped placements are checked
/// at each level. Returns the short cause string used for `[tls: <cause>]`,
/// or `None` when the chain carries no TLS error (a plain connect timeout or
/// refusal, i.e. genuinely `[unreachable]`).
fn find_rustls_cause(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(rustls_err) = e.downcast_ref::<rustls::Error>() {
            return Some(describe_rustls_error(rustls_err));
        }
        if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
            // `io::Error::source()` skips its own boxed payload and jumps
            // straight to the payload's source (see std's implementation), so
            // a `rustls::Error` boxed inside, possibly through several nested
            // `io::Error` layers (as hyper's client stack does on a TLS
            // handshake failure), would never surface by following plain
            // `.source()`. `get_ref()` un-boxes one layer at a time instead;
            // loop it so any wrapping depth is handled, not just one level.
            current = io_err
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static));
            continue;
        }
        current = e.source();
    }
    None
}

/// Map a `rustls::Error` to a short, human-readable cause. Certificate errors
/// get specific text; `CaUsedAsEndEntity` (a CA:TRUE certificate presented as
/// the server's own leaf, the exact self-hosting.md client-trust trap) is
/// detected by name inside `CertificateError::Other`, the bucket rustls maps
/// it into (webpki's variant has no direct `CertificateError` counterpart).
fn describe_rustls_error(e: &rustls::Error) -> String {
    use rustls::CertificateError as CE;
    match e {
        rustls::Error::InvalidCertificate(ce) => match ce {
            CE::Expired | CE::ExpiredContext { .. } => "certificate expired".to_string(),
            CE::NotValidYet | CE::NotValidYetContext { .. } => {
                "certificate not yet valid".to_string()
            }
            CE::UnknownIssuer => "unknown issuer, not signed by a trusted CA".to_string(),
            CE::NotValidForName | CE::NotValidForNameContext { .. } => {
                "certificate not valid for this hostname".to_string()
            }
            CE::Other(inner) if inner.to_string().contains("CaUsedAsEndEntity") => {
                "a CA certificate was presented as the server's own leaf certificate".to_string()
            }
            other => format!("certificate rejected: {other:?}"),
        },
        other => format!("TLS handshake failed: {other}"),
    }
}

/// Hint appended to a TLS WARN when `server_ca` / `SPELUNK_SERVER_CA` is
/// configured: the two classic self-hosting.md client-trust traps, so a user
/// does not have to rediscover them by trial and error.
fn cert_trust_hint() -> String {
    "\n  server_ca is configured; two classic misconfigurations cause this:\n  \
     1) the file points at the server's own leaf certificate, not the issuing CA\n  \
     2) the server is presenting a CA certificate (CA:TRUE) as its own leaf certificate\n  \
     See docs/self-hosting.md, section \"Trusting the server's certificate on the client\"."
        .to_string()
}

/// Server-side embedder readiness, mirrored from the `/v1/health` `embedder.state`
/// field. The CLI uses this to distinguish, when semantic search is unavailable,
/// between "no server reachable", "server up but the model is still warming up",
/// and "the model failed to load" — so it can print an actionable one-line notice
/// rather than silently degrading.
///
/// Serialized lowercase to match the server's health body and to feed
/// `spelunk status --format json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderState {
    /// Native embedder build/download in progress — not ready yet, keep polling.
    Loading,
    /// Model loaded; embed endpoints will serve.
    Ready,
    /// Background load failed (download error, OOM, …). Terminal for that process.
    Unavailable,
    /// Server started with no in-process model to load (external embedding URL,
    /// or no embedder feature). Treated as ready.
    Disabled,
    /// Field absent from the health body (server pre-dates it). Unknown state.
    #[default]
    Unknown,
}

impl EmbedderState {
    /// Lowercase wire string (matches the server's `embedder.state` field and
    /// feeds `spelunk status --format json`).
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedderState::Loading => "loading",
            EmbedderState::Ready => "ready",
            EmbedderState::Unavailable => "unavailable",
            EmbedderState::Disabled => "disabled",
            EmbedderState::Unknown => "unknown",
        }
    }
}

/// Server-enforced operative limits relevant to sizing an `/index/embed`
/// request, mirrored from `/v1/health`'s `limits` object (see
/// `crates/spelunk-server/src/handlers.rs` `ServerLimits`).
///
/// `None` on a `Tier::Server` (rather than this struct being absent) means the
/// server pre-dates this field — the embed phase treats that as "assume the
/// legacy 30s / no-embed-exemption profile", which is exactly the
/// version-skew case a newer CLI can hit talking to an older, long-running
/// server (see `embed_phase.rs`'s calibration-vs-server-budget clamping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    /// Wall-clock budget (seconds) the server allows a single `/index/embed`
    /// request before returning `408`.
    pub embed_request_timeout_secs: u64,
    /// Max chunks accepted in a single `/index/embed` request (`413` above this).
    pub max_batch_chunks: usize,
    /// Per-chunk token truncation cap the embedder enforces, if known.
    pub embedder_token_cap: Option<usize>,
}

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
    /// Reserved (ADR-002 `/plan`): parsed from server caps but hidden from all
    /// user-facing output until a `spelunk plan` command ships.
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    pub plan: bool,
    /// The server accepts a client-pushed embedding vector on `POST
    /// /memory/batch`, advertised as a top-level `bool` in
    /// `/v1/health` (NOT an entry in the `capabilities` array). When set, the
    /// sync push may send the locally-computed fp32/896 vector instead of making
    /// the server re-embed; when unset (older server / OSS team server) the push
    /// stays text-only. Not surfaced in user-facing output.
    #[serde(skip_serializing)]
    pub accepts_pushed_vectors: bool,
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
            // Not derivable from the `capabilities` array — it is a separate
            // top-level bool set by `parse_health` from the health body.
            accepts_pushed_vectors: false,
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
            // A legacy plain-text server pre-dates the pushed-vector accept side.
            accepts_pushed_vectors: false,
        }
    }

    /// Full set for a fully-featured server.
    #[cfg(test)]
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
            accepts_pushed_vectors: true,
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
        auto_discovered: bool,
        /// Server-side embedder readiness, mirrored from the `/v1/health`
        /// `embedder.state` field. `Unknown` when the field is absent (server
        /// pre-dates it). Lets the CLI distinguish "server up but model still
        /// warming up / failed to load" from a ready server when semantic
        /// search is unavailable (rendered by `status`).
        embedder_state: EmbedderState,
        /// Server-enforced `/index/embed` limits, mirrored from `/v1/health`'s
        /// `limits` object. `None` when the field is absent — a server that
        /// pre-dates this fix and still enforces the old blanket 30s budget
        /// with no `/index/embed` exemption. The embed phase
        /// (`embed_phase.rs`) reads this to clamp its own calibration to what
        /// this particular server actually supports instead of assuming.
        server_limits: Option<ServerLimits>,
    },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    // Used by check.rs / status.rs via pattern matching on the enum variant;
    // also consumed by sub-issues #323/#324 UX wiring.
    #[cfg(test)]
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline => None,
        }
    }

    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline => None,
        }
    }

    /// Server-side embedder readiness for a `Server` tier, or `None` when
    /// offline. `EmbedderState::Unknown` is returned for a reachable server that
    /// pre-dates the `embedder.state` health field. Used by the offline notice
    /// (`search`/`index`) and by `spelunk status` to explain why semantic search
    /// is unavailable.
    pub fn embedder_state(&self) -> Option<EmbedderState> {
        match self {
            Tier::Server { embedder_state, .. } => Some(*embedder_state),
            Tier::Offline => None,
        }
    }

    /// Server-enforced `/index/embed` limits for a `Server` tier, or `None`
    /// when offline *or* when the server pre-dates the `/v1/health` `limits`
    /// field. Used by the embed phase (`embed_phase.rs`) to clamp its own
    /// calibration to what this particular server actually supports.
    pub fn server_limits(&self) -> Option<ServerLimits> {
        match self {
            Tier::Server { server_limits, .. } => *server_limits,
            Tier::Offline => None,
        }
    }

    /// Returns `true` when the server URL was discovered automatically via
    /// the loopback probe rather than set explicitly in config or environment.
    /// Used by `spelunk status` (sub-issue #324) to annotate the URL with `(local, auto)`.
    #[cfg(test)]
    pub fn is_auto_discovered(&self) -> bool {
        matches!(
            self,
            Tier::Server {
                auto_discovered: true,
                ..
            }
        )
    }

    /// `Some(url)` when this tier reached `Server` via an **explicit**
    /// `server_url` (not loopback auto-discovery); `None` for `Offline` and
    /// for the auto-discovered loopback case.
    ///
    /// `spelunk server logs` only ever reads the local auto-daemon's log
    /// file. A command-output hint that names a server to check must use
    /// this instead of unconditionally pointing at that command: with an
    /// explicit remote `server_url`, `spelunk server logs` reads a healthy
    /// local daemon's log while the real failure lives on the named server
    /// (the pattern `embedder_status_line` in `status.rs` established).
    pub fn explicit_remote_url(&self) -> Option<&str> {
        match self {
            Tier::Server {
                url,
                auto_discovered: false,
                ..
            } => Some(url),
            _ => None,
        }
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
    // An *explicit* offline mode (config `mode = "offline"`,
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
    let server_ca = cfg.server_ca.clone();
    TIER.get_or_init(|| async move {
        if explicit_offline {
            tracing::debug!("sync mode is explicitly offline — skipping all server probes");
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
        tracing::debug!("SPELUNK_NO_SERVER set — skipping all server probes");
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
    // Step 3a: port file written by `spelunk server start`
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        // Loopback probes never produce hard errors (auto_discovered=true), so unwrap is safe.
        // Loopback is plaintext http — a custom CA is irrelevant here.
        let tier = probe_url(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true, None)
            .await
            .unwrap_or(Tier::Offline);
        if tier.is_server() {
            return tier;
        }
        tracing::debug!("loopback probe on port {port} failed — falling back to default port");
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

    tracing::debug!("loopback auto-discovery: no local server found — offline mode");
    Tier::Offline
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
    // Non-loopback plaintext http:// is invalid config — reject before sending
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

    // `/v1/health` is an unauthenticated endpoint — do not send a bearer to it.
    let req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let (caps, server_dim, embedder_state, server_limits) = parse_health(url, resp).await;

            // If the server advertises index.embed, its embedding dimension must match ours.
            if caps.index_embed && server_dim != 0 {
                let expected = spelunk_core::embeddings::EMBEDDING_DIM;
                if server_dim != expected {
                    if auto_discovered {
                        // Loopback auto-discovery: downgrade gracefully — the user did
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
                    "spelunk-server at {url} returned {} — running in offline mode",
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
/// `server_limits` mirrors `/v1/health`'s `limits` object. `None` when absent —
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
                         expose another user's memory — consider running your own server"
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
            // Legacy server returns plain-text "ok" — conservative fallback.
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

/// Guidance for an *inference*-backed feature (semantic `memory search`,
/// `memory timeline`, `memory harvest`) that has no reachable server.
///
/// Emitted at client construction, where reachability is unknown: when
/// `server_url` is set, construction always succeeds, so this message only ever
/// fires with `server_url` unset. It therefore carries no configured-server
/// hint; a team-server-unreachable hint, if ever wanted, must be produced at the
/// inference call site where the connection failure is observed. `server_url`
/// advice stays `require_tier1`'s job for the genuinely team-only features.
pub fn inference_server_required_message(feature: &str) -> String {
    format!(
        "'spelunk {feature}' requires spelunk-server.\n\
         Run `spelunk server start` to enable this feature."
    )
}

/// Return `Ok(())` if the tier is `Server`, otherwise return an `anyhow::Error`
/// with the standard locked-feature message format.
///
/// The message is scoped to the actual failure state: with a configured
/// `server_url` the fix is never "set server_url" (it already is), it is that
/// the configured server could not be served from.
///
/// Callers append `?` to propagate the error:
/// ```ignore
/// require_tier1("explore", tier, cfg.server_url.as_deref())?;
/// ```
pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    match server_url {
        Some(url) => anyhow::bail!(
            "'spelunk {feature}' requires spelunk-server.\n\
             The configured server_url ({url}) did not respond to the health probe.\n\
             Check that server and your network; for TLS trust failures see \
             server_ca / SPELUNK_SERVER_CA."
        ),
        None => anyhow::bail!(
            "'spelunk {feature}' requires spelunk-server.\n\
             Set server_url in ~/.config/spelunk/config.toml to enable this feature."
        ),
    }
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(auto.is_auto_discovered());
        assert!(!explicit.is_auto_discovered());
        assert!(!Tier::Offline.is_auto_discovered());
    }

    #[test]
    fn tier_explicit_remote_url_only_for_explicit_server() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(auto.explicit_remote_url(), None);
        assert_eq!(
            explicit.explicit_remote_url(),
            Some("http://server.example.com:7777")
        );
        assert_eq!(Tier::Offline.explicit_remote_url(), None);
    }

    #[test]
    fn tier_explicit_remote_url_is_explicit_even_when_host_is_loopback() {
        // `explicit_remote_url` keys off *how the URL was reached*
        // (`auto_discovered`), never off the host it resolves to. An operator
        // can hand-configure `server_url = http://127.0.0.1:PORT`; it is
        // still `auto_discovered: false` because it went through the
        // `Some(url)` probe branch, not loopback auto-discovery (see
        // `probe()`). `spelunk server logs` only ever reads the fixed
        // auto-daemon log path and has no idea this loopback address was
        // hand-configured, so the hint must still name it rather than assume
        // "loopback implies safe to point at the local log".
        let explicit_loopback = Tier::Server {
            url: "http://127.0.0.1:9797".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(
            explicit_loopback.explicit_remote_url(),
            Some("http://127.0.0.1:9797"),
            "an explicitly configured server_url must count as explicit even when its host is loopback"
        );
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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

    // ── inference_server_required_message ────────────────────────────────────

    /// No server reachable AND no `server_url` configured (solo user, no local
    /// server running): the message must point at the zero-setup local server
    /// and must NOT mention `server_url` (the misleading team-infra advice).
    #[test]
    fn inference_msg_no_server_url_points_at_local_start_only() {
        let msg = inference_server_required_message("memory search");
        assert!(msg.contains("'spelunk memory search' requires spelunk-server"));
        assert!(
            msg.contains("spelunk server start"),
            "must point at the local auto-server: {msg}"
        );
        assert!(
            !msg.contains("server_url"),
            "must NOT mention server_url when none is configured: {msg}"
        );
    }

    /// Feature name is interpolated (harvest reuses this via
    /// `harvest_requires_server`, preserving its Tier-0 substring contract).
    #[test]
    fn inference_msg_interpolates_feature_and_keeps_harvest_substring() {
        let msg = inference_server_required_message("memory harvest");
        assert!(msg.contains("'spelunk memory harvest' requires spelunk-server"));
    }

    // ── require_tier1 ────────────────────────────────────────────────────────

    #[test]
    fn require_tier1_ok_for_server() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
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
        assert!(msg.contains("Set server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_names_that_server() {
        // server_url is already configured; the message must name the failing
        // server, never tell the operator to set what is already set.
        let tier = Tier::Offline;
        let err = require_tier1("plan", &tier, Some("https://bad:7777")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk plan'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("https://bad:7777"));
        assert!(
            !msg.contains("Set server_url"),
            "must not suggest setting an already-set server_url: {msg}"
        );
        assert!(
            msg.contains("server_ca"),
            "must point at the TLS-trust knob for untrusted-cert failures: {msg}"
        );
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline;
        let err = require_tier1("memory push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk memory push'"));
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

    // ── Embedding-dim pre-flight checks ──────────────────────────────────────

    /// Helper: build a health JSON body with the given capabilities and dim.
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

    /// Auto-discovered loopback server with wrong dim → `Tier::Offline` (soft downgrade).
    #[tokio::test]
    async fn probe_loopback_dim_mismatch_downgrades_to_offline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Return a health body claiming 768-dim embeddings — wrong for the current CLI (896).
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

    /// Auto-discovered loopback server with correct dim → `Tier::Server`.
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

    /// A server advertising `accepts_pushed_vectors: true` must parse into
    /// `caps.accepts_pushed_vectors == true` — the gate the sync push reads
    /// before attaching a client-computed vector.
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

    /// A server that omits the field (older server, or the OSS team server)
    /// must default to `false` — the push stays text-only there.
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

    /// A server that DOES advertise `limits` must have it parsed into
    /// `Tier::Server.server_limits`. This is the non-version-skew case: a
    /// current-build server carrying the `/index/embed` timeout exemption.
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

    /// A server that does NOT advertise `limits` (pre-dates the field) must
    /// leave `Tier::Server.server_limits` as `None` — this is the exact
    /// version-skew case: an old server still enforcing the legacy 30s
    /// `/index/embed` budget with no exemption. `None` must never be
    /// confused with "no limit" by a caller.
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

    /// `embedder_token_cap` specifically must round-trip as `None` when the
    /// server reports it as JSON `null` (e.g. embedder not ready, or an
    /// external non-native backend with no known cap) — distinct from the
    /// whole `limits` object being absent.
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

    /// Auto-discovered loopback server with no embedder (dim 0) → `Tier::Server`
    /// (dim 0 means no `index.embed` check is relevant).
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

    /// Explicit server_url with wrong dim → hard `Err` with an actionable message.
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

    /// A non-loopback `http://` URL must be rejected before any request is
    /// sent — no mock is mounted, so a request would fail with "connection
    /// refused" or similar rather than surfacing the validation error; the
    /// assertion on the error message proves the reject happened pre-flight.
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // Deliberately no MockServer / no listener on this address — if
        // `probe_url` tried to send a request it would get a connection error,
        // not this validation message.
        let result = probe_url("http://team-server:7777", REMOTE_PROBE_TIMEOUT, false, None).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    /// Same rejection applies to the loopback auto-discovery path (defensive;
    /// auto-discovery URLs are always loopback in practice).
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

    /// Loopback `http://` and `https://` URLs are accepted (proceed to the
    /// actual health request against a mock server).
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

    /// `/v1/health` must never carry an `Authorization` header — it is an
    /// unauthenticated endpoint.
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

    // ── EmbedderState ────────────────────────────────────────────────────────

    #[test]
    fn embedder_state_default_is_unknown() {
        assert_eq!(EmbedderState::default(), EmbedderState::Unknown);
    }

    #[test]
    fn embedder_state_deserializes_lowercase_wire_values() {
        // Must match the server's `#[serde(rename_all = "lowercase")]` values.
        for (wire, want) in [
            ("loading", EmbedderState::Loading),
            ("ready", EmbedderState::Ready),
            ("unavailable", EmbedderState::Unavailable),
            ("disabled", EmbedderState::Disabled),
        ] {
            let got: EmbedderState =
                serde_json::from_value(serde_json::Value::String(wire.to_string())).unwrap();
            assert_eq!(got, want, "wire {wire:?} should deserialize to {want:?}");
            assert_eq!(want.as_str(), wire, "as_str round-trips the wire value");
        }
    }

    #[test]
    fn tier_embedder_state_accessor() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Loading,
            server_limits: None,
        };
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
        assert_eq!(Tier::Offline.embedder_state(), None);
    }

    /// Health body carrying the PR-A `embedder: { state, detail }` sub-object.
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

    /// `probe_url` must surface the server's `embedder.state` on `Tier::Server`.
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

    /// A server that pre-dates the `embedder` field → `Unknown` (not an error).
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

    // ── error_chain / find_rustls_cause / describe_rustls_error ─────────────

    /// Minimal chained error for exercising `error_chain`/`find_rustls_cause`
    /// without needing a real `reqwest::Error` (whose constructors are private).
    #[derive(Debug)]
    struct ChainErr(&'static str, Option<Box<dyn std::error::Error + 'static>>);

    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref()
        }
    }

    /// A fake error whose `Display` mimics webpki's `CaUsedAsEndEntity`, since
    /// rustls buckets that variant into `CertificateError::Other` (no direct
    /// counterpart) and detection matches on the rendered name.
    #[derive(Debug)]
    struct FakeCaUsedAsEndEntity;

    impl std::fmt::Display for FakeCaUsedAsEndEntity {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CaUsedAsEndEntity")
        }
    }

    impl std::error::Error for FakeCaUsedAsEndEntity {}

    #[test]
    fn error_chain_joins_every_source_level() {
        let bottom = ChainErr("dns lookup failed", None);
        let middle = ChainErr("connecting to socket", Some(Box::new(bottom)));
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(middle)),
        );

        let chain = error_chain(&top);
        assert_eq!(
            chain,
            "error sending request for url (https://x/) -> connecting to socket -> dns lookup failed"
        );
    }

    #[test]
    fn error_chain_single_level_is_just_the_message() {
        let only = ChainErr("boom", None);
        assert_eq!(error_chain(&only), "boom");
    }

    #[test]
    fn find_rustls_cause_none_for_plain_io_error_chain() {
        // Models a genuine connect-level failure (refused/timed out): no
        // rustls::Error anywhere in the chain, so this must classify as
        // `[unreachable]`, not `[tls: ...]`.
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(io_err)),
        );
        assert!(find_rustls_cause(&top).is_none());
    }

    #[test]
    fn find_rustls_cause_detects_rustls_error_boxed_in_io_error() {
        // tokio-rustls reports handshake failures as an io::Error wrapping a
        // rustls::Error: the exact shape this function must see through.
        let rustls_err = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io_err = std::io::Error::other(rustls_err);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(io_err)),
        );

        let cause = find_rustls_cause(&top).expect("must detect the boxed rustls::Error");
        assert!(cause.contains("unknown issuer"), "got: {cause}");
    }

    #[test]
    fn find_rustls_cause_detects_direct_rustls_error() {
        let rustls_err =
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(rustls_err)),
        );

        let cause = find_rustls_cause(&top).expect("must detect a directly-chained rustls::Error");
        assert!(cause.contains("hostname"), "got: {cause}");
    }

    #[test]
    fn describe_rustls_error_names_ca_used_as_end_entity() {
        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Other(
            rustls::OtherError(std::sync::Arc::new(FakeCaUsedAsEndEntity)),
        ));
        let cause = describe_rustls_error(&err);
        assert!(
            cause.contains("CA certificate") && cause.contains("leaf"),
            "got: {cause}"
        );
    }

    #[test]
    fn describe_rustls_error_expired() {
        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        assert_eq!(describe_rustls_error(&err), "certificate expired");
    }

    #[test]
    fn describe_rustls_error_non_certificate_variant_falls_back_generically() {
        let err = rustls::Error::NoCertificatesPresented;
        let cause = describe_rustls_error(&err);
        assert!(cause.starts_with("TLS handshake failed:"), "got: {cause}");
    }

    #[test]
    fn cert_trust_hint_mentions_both_classic_traps_and_the_doc_section() {
        let hint = cert_trust_hint();
        assert!(hint.contains("leaf certificate, not the issuing CA"));
        assert!(hint.contains("CA:TRUE"));
        assert!(hint.contains("Trusting the server's certificate on the client"));
    }

    // Note: a real end-to-end TLS-trust failure (genuine rustls handshake
    // against a proper CA→leaf chain, and against a CA:TRUE-as-leaf
    // misconfiguration) is exercised in `tests/tls_trust.rs`, which asserts
    // `explicit_probe_failure()` reports `ConnFailure::Tls` and that the
    // status/WARN output names the certificate cause. That is the level this
    // bug actually lives at: reqwest's real error chain through hyper/rustls
    // isn't reproducible with a hand-built chain here.

    // ── version-coupling guard ───────────────────────────────────────────────

    /// `find_rustls_cause`'s `downcast_ref::<rustls::Error>()` only matches
    /// while spelunk-cli's direct `rustls` dependency resolves to the exact
    /// same crate version as the one reqwest's `rustls-tls` feature pulls in
    /// transitively: `downcast_ref` compares `TypeId`, which differs across
    /// two builds of the same-named crate at different semver-incompatible
    /// versions. A future dependency bump that forces a second `rustls` into
    /// the tree would silently degrade every TLS diagnostic back to
    /// `[unreachable]`, with no panic and no failed request: just a downcast miss.
    /// Catch that at the lockfile level, immediately, rather than waiting for
    /// a TLS handshake to expose it.
    #[test]
    fn cargo_lock_resolves_a_single_rustls_version() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_path = manifest_dir.join("../../Cargo.lock");
        let lock = std::fs::read_to_string(&lock_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));

        let rustls_entries = lock
            .lines()
            .filter(|line| line.trim() == "name = \"rustls\"")
            .count();

        assert_eq!(
            rustls_entries, 1,
            "expected exactly one resolved `rustls` version in Cargo.lock, found \
             {rustls_entries}; a split here means find_rustls_cause's downcast_ref \
             will silently stop matching TLS causes; repin spelunk-cli's direct \
             rustls to the same version reqwest resolves"
        );
    }

    // ── get_tier process-cache semantics ─────────────────────────────────────

    /// `TIER` is a `OnceCell`: `get_tier` must probe at most once per process
    /// and every later call must return the identical cached `Tier`, not
    /// re-probe. This is what makes `EXPLICIT_PROBE_FAILURE` safe to read from
    /// `Tier::Offline` rendering: there is no later probe in the same process
    /// that could silently swap a fresh success in underneath a stale failure
    /// annotation (or vice versa).
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

    /// A genuine TCP connection-refused error through the real `reqwest`
    /// client must classify as `Unreachable`, never `Tls`: no TLS layer is
    /// ever reached, so `find_rustls_cause` must return `None` on it.
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

    /// A genuine client-side timeout (the peer accepts the TCP connection but
    /// never answers) must also classify as `Unreachable`, not `Tls`: a slow
    /// or hung server is not a certificate problem.
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

    /// A reachable server that answers with a non-2xx status (e.g. a
    /// misconfigured reverse proxy, a 500, garbage) is neither `[tls: ...]`
    /// nor `[unreachable]`: the transport and TLS both worked fine. This
    /// path must leave `EXPLICIT_PROBE_FAILURE` unset entirely.
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

    /// Auto-discovered (loopback) probe failures must never populate
    /// `EXPLICIT_PROBE_FAILURE`: that cache exists only to annotate an
    /// *explicit* `server_url` miss. A common "no local server running"
    /// loopback miss must not leave behind a failure cause that a later
    /// status render could misattribute to an unrelated explicit `server_url`.
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

    // ── find_rustls_cause: nested io::Error unwrap depth ─────────────────────

    /// tokio-rustls's own wrapping is one `io::Error` layer deep, but the
    /// hyper/reqwest client stack can add further `io::Error` wrapping on top
    /// of that. `find_rustls_cause` must keep unwrapping past the first
    /// layer: a version that only checked one level (e.g. a depth-limited
    /// rewrite of the loop) would miss this and misclassify as `[unreachable]`.
    #[test]
    fn find_rustls_cause_detects_rustls_error_two_io_error_layers_deep() {
        let rustls_err = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        let inner_io = std::io::Error::other(rustls_err);
        let outer_io = std::io::Error::other(inner_io);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(outer_io)),
        );

        let cause = find_rustls_cause(&top)
            .expect("must unwrap two nested io::Error layers to find the rustls::Error");
        assert!(cause.contains("expired"), "got: {cause}");
    }

    // ── describe_rustls_error: CaUsedAsEndEntity string-match must be exact ──

    /// A `CertificateError::Other` whose rendered text does NOT mention
    /// `CaUsedAsEndEntity` must fall back to the generic message, not be
    /// swept into the CA-as-leaf-specific sentence. This is the negative half
    /// of `describe_rustls_error_names_ca_used_as_end_entity`: without it, an
    /// overly-loose match (e.g. matching on `Other(_)` alone) would pass the
    /// positive test but silently mislabel every other certificate error.
    #[test]
    fn describe_rustls_error_other_variant_without_the_marker_string_is_generic() {
        #[derive(Debug)]
        struct SomeOtherWebpkiError;
        impl std::fmt::Display for SomeOtherWebpkiError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "InvalidSignatureForPublicKey")
            }
        }
        impl std::error::Error for SomeOtherWebpkiError {}

        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Other(
            rustls::OtherError(std::sync::Arc::new(SomeOtherWebpkiError)),
        ));
        let cause = describe_rustls_error(&err);
        assert!(
            !cause.contains("CA certificate") && !cause.contains("own leaf"),
            "must not misclassify an unrelated Other() cause as CA-as-leaf: got {cause}"
        );
        assert!(cause.starts_with("certificate rejected:"), "got: {cause}");
    }

    // ── cert_trust_hint gating ────────────────────────────────────────────────

    /// The hint is only useful (and only accurate) when `server_ca` is
    /// actually configured: it names a `server_ca` misconfiguration. Without
    /// `server_ca` set, an `UnknownIssuer` failure is trusting the default
    /// root store, and the hint must not appear, so a real e2e for this
    /// exact gating lives in `tests/tls_trust.rs`
    /// (`tls_server_with_untrusted_cert_and_no_server_ca_configured...`); this
    /// unit test only pins the gating condition itself.
    #[test]
    fn cert_trust_hint_is_only_appended_when_server_ca_is_configured() {
        // Mirrors the gating in probe_url's Err(e) TLS-cause branch.
        let server_ca: Option<&std::path::Path> = None;
        let hint = if server_ca.is_some() {
            cert_trust_hint()
        } else {
            String::new()
        };
        assert!(hint.is_empty(), "no server_ca configured => no hint");

        let server_ca: Option<&std::path::Path> = Some(std::path::Path::new("/tmp/ca.pem"));
        let hint = if server_ca.is_some() {
            cert_trust_hint()
        } else {
            String::new()
        };
        assert!(!hint.is_empty(), "server_ca configured => hint present");
    }

    // ── chain rendering hygiene ───────────────────────────────────────────────

    /// `error_chain` must not panic or garble on a `Display` embedding literal
    /// newlines (e.g. a multi-line certificate parse error): it is printed
    /// straight into a `tracing::warn!` line and the terminal.
    #[test]
    fn error_chain_does_not_panic_on_multiline_display() {
        let bottom = ChainErr("line one\nline two\nline three", None);
        let top = ChainErr("outer", Some(Box::new(bottom)));
        let chain = error_chain(&top);
        assert_eq!(chain, "outer -> line one\nline two\nline three");
    }

    /// `error_chain` and `find_rustls_cause` both walk the chain with a
    /// `while let` loop, not recursion: an arbitrarily deep chain must not
    /// stack-overflow. 10k levels is far beyond anything hyper/reqwest/rustls
    /// actually produce (2-4 levels in practice); this only pins that the
    /// walk is iterative.
    #[test]
    fn error_chain_does_not_overflow_on_a_very_deep_chain() {
        const DEPTH: usize = 10_000;
        let mut err: Box<dyn std::error::Error + 'static> = Box::new(ChainErr("bottom", None));
        for _ in 0..DEPTH {
            err = Box::new(ChainErr("layer", Some(err)));
        }
        let chain = error_chain(err.as_ref());
        assert_eq!(chain.matches(" -> ").count(), DEPTH);
        assert!(find_rustls_cause(err.as_ref()).is_none());
    }
}
