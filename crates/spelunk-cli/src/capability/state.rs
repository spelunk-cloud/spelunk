//! Server-capability data types: embedder readiness, index/embed limits,
//! and the feature-availability set parsed from `/v1/health`.

use serde::{Deserialize, Serialize};

/// Server-side embedder readiness, mirrored from the `/v1/health` `embedder.state`
/// field. The CLI uses this to distinguish, when semantic search is unavailable,
/// between "no server reachable", "server up but the model is still warming up",
/// and "the model failed to load": so it can print an actionable one-line notice
/// rather than silently degrading.
///
/// Serialized lowercase to match the server's health body and to feed
/// `spelunk status --format json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderState {
    /// Native embedder build/download in progress: not ready yet, keep polling.
    Loading,
    /// Model loaded; embed endpoints will serve.
    Ready,
    /// Background load failed (download error, OOM, …). Terminal for that process.
    Unavailable,
    /// Server built without the native embedder (`embed-native` feature): no
    /// in-process model to load, ever. Embed endpoints return a permanent 400.
    Disabled,
    /// Field absent from the health body (server pre-dates it), or set to a
    /// state this build does not know. Unknown state.
    ///
    // `other` matters more than it looks. Without it an unrecognised state
    // string fails to deserialize, and because this enum sits inside the
    // health body, that failure takes the *whole* body down with it: the CLI
    // falls back to the legacy plain-text branch and silently discards
    // `limits`, `embedding_dim`, and every capability the server advertised.
    // A newer server adding a state value is explicitly allowed by the
    // additive-only rule in docs/stability.md, so it must cost nothing.
    #[default]
    #[serde(other)]
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
/// server pre-dates this field: the embed phase treats that as "assume the
/// legacy 30s / no-embed-exemption profile", which is exactly the
/// version-skew case a newer CLI can hit talking to an older, long-running
/// server (see `embed_phase.rs`'s calibration-vs-server-budget clamping).
///
/// Every member is independently optional because a peer advertises them
/// independently: `None` on a member means "this peer did not advertise it, or
/// advertised it in a shape this build cannot read", and each consumer applies
/// its own legacy fallback. Reading the object all-or-nothing instead would
/// discard a member that parsed fine because a sibling did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    /// Wall-clock budget (seconds) the server allows a single `/index/embed`
    /// request before returning `408`.
    pub embed_request_timeout_secs: Option<u64>,
    /// Max chunks accepted in a single `/index/embed` request (`413` above this).
    pub max_batch_chunks: Option<usize>,
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
    /// The server serves `POST /llm/complete`, advertised as `llm.complete`.
    ///
    /// The only trustworthy "this server has an LLM" signal. `explore` cannot
    /// stand in for it: `explore` predates both this capability and the
    /// `/llm/complete` route, so a server old enough to advertise `explore`
    /// alone has no LLM route at all.
    ///
    /// Kept out of `spelunk status --format json` (which serializes this
    /// struct wholesale) so that payload's shape is unchanged.
    #[serde(skip_serializing)]
    pub llm_complete: bool,
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
    pub(crate) fn from_server_caps(caps: &[&str]) -> Self {
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
            llm_complete: has("llm.complete"),
            plan: has("plan"),
            // Not derivable from the `capabilities` array: it is a separate
            // top-level bool set by `parse_health` from the health body.
            accepts_pushed_vectors: false,
        }
    }

    /// Conservative set assumed when talking to a legacy server that returns
    /// plain-text health ("ok") instead of JSON.
    pub(crate) fn legacy_memory_only() -> Self {
        Self {
            search_semantic: false,
            index_embed: false,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: false,
            explore: false,
            llm_complete: false,
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
            llm_complete: true,
            plan: true,
            accepts_pushed_vectors: true,
        }
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

    // ── llm.complete ─────────────────────────────────────────────────────────

    #[test]
    fn from_server_caps_llm_complete_sets_the_flag() {
        let caps = Capabilities::from_server_caps(&["memory", "explore", "llm.complete"]);
        assert!(caps.llm_complete);
    }

    // Version skew: `llm.complete` and the `/llm/complete` route landed
    // together, while `explore` predates both. A server advertising only
    // `explore` has no LLM route at all, so `explore` must never stand in for
    // the LLM signal.
    #[test]
    fn from_server_caps_explore_without_llm_complete_is_not_llm_capable() {
        let caps = Capabilities::from_server_caps(&["memory", "index.embed", "explore", "plan"]);
        assert!(caps.explore);
        assert!(!caps.llm_complete);
    }

    #[test]
    fn from_server_caps_without_explore_or_llm_complete_is_not_llm_capable() {
        let caps = Capabilities::from_server_caps(&["memory", "index.embed"]);
        assert!(!caps.explore);
        assert!(!caps.llm_complete);
    }

    #[test]
    fn legacy_memory_only_is_not_llm_capable() {
        assert!(!Capabilities::legacy_memory_only().llm_complete);
    }

    #[test]
    fn all_is_llm_capable() {
        assert!(Capabilities::all().llm_complete);
    }

    // `spelunk status --format json` serializes `Capabilities` wholesale
    // (`status.rs`), so a newly-parsed field must stay out of that object or
    // the documented status payload changes shape.
    #[test]
    fn llm_complete_is_not_serialized_into_status_json() {
        let value = serde_json::to_value(Capabilities::all()).expect("serialize capabilities");
        let object = value
            .as_object()
            .expect("capabilities serialize as an object");
        assert!(
            !object.contains_key("llm_complete"),
            "llm_complete must not reach `spelunk status --format json`: {object:?}"
        );
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
}
