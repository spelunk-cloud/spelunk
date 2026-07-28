//! Feature-gating guards: turn a `Tier` into either `Ok` or an actionable
//! "requires spelunk-server" error.

use super::tier::Tier;

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

/// Guard for a feature that moves memory to or from an explicitly-configured
/// server (`memory push`, `sync`, `memory pull`): a self-hosted team server or
/// Spelunk Cloud both work identically here. Distinct from features that
/// merely need *an* inference-capable server ([`require_tier1`]).
///
/// `require_tier1` alone can't distinguish an auto-discovered loopback
/// inference server (tier `Server`, `server_url` unset, never a memory store,
/// ADR-004) from an explicitly-configured team/cloud server, so callers check
/// `require_tier1` first, then this guard confirms the server was configured
/// explicitly rather than merely auto-discovered.
///
/// Explicit-config-only: reads `cfg.server_url` and nothing else, never
/// probing reachability. Every current caller has already established
/// reachability via `require_tier1`, so this only answers one question: was
/// the server set explicitly? Returns the configured `server_url` on success.
pub fn require_explicit_server_url(
    feature: &str,
    cfg: &crate::config::Config,
) -> anyhow::Result<String> {
    cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "'spelunk {feature}' requires a server. Set `server_url` in your spelunk config \
             (e.g. ~/.config/spelunk/config.toml or .spelunk/config.toml)."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::state::{Capabilities, EmbedderState};
    use super::*;
    use crate::config::Config;

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

    // ── require_explicit_server_url ──────────────────────────────────────────
    //
    // `memory push`, `sync`, and `memory pull` move memory to/from an
    // explicitly-configured team server; an auto-discovered loopback
    // inference server must never satisfy them (ADR-004). This guard checks
    // configuration presence only, never reachability, so it stays usable
    // before any network call is made.

    #[test]
    fn require_explicit_server_url_errs_when_unset() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let err = require_explicit_server_url("sync", &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_explicit_server_url_ok_regardless_of_reachability() {
        let cfg = Config {
            server_url: Some("https://unreachable.invalid:1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            require_explicit_server_url("sync", &cfg).unwrap(),
            "https://unreachable.invalid:1"
        );
    }

    /// `memory push` and `sync` must refuse with the exact same message
    /// shape (only the feature name differs), so they can't drift apart
    /// again.
    #[test]
    fn require_explicit_server_url_message_is_identical_in_shape_across_features() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let push_msg = require_explicit_server_url("memory push", &cfg)
            .unwrap_err()
            .to_string();
        let sync_msg = require_explicit_server_url("sync", &cfg)
            .unwrap_err()
            .to_string();
        assert_eq!(
            push_msg.replace("memory push", "sync"),
            sync_msg,
            "push and sync messages must differ only in the feature name: \
             push={push_msg:?} sync={sync_msg:?}"
        );
    }
}
