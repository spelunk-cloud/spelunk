//! LLM routing: which server, if any, serves `POST /llm/complete` for this
//! project.
//!
//! Deliberately separate from embedding. `Config::resolve_inference_url` is
//! the embed rule and is not consulted here; embedding keeps routing to the
//! local tier exactly as it did before this module existed.
//!
//! The rule, in order:
//!
//! 1. Explicit offline: nothing runs, and nothing is probed.
//! 2. The local inference tier advertises `llm.complete`: use it.
//! 3. `llm_url` is set but the local server does not serve an LLM: **stop**.
//!    The user asked for a local LLM; quietly sending their code to a remote
//!    one instead is a privacy surprise, not a fallback.
//! 4. An explicit `server_url` advertises `llm.complete`: use it.
//! 5. Otherwise no LLM is available.
//!
//! Step 2 keys on what the reachable server actually advertises rather than on
//! a config field, because a field cannot tell you whether the running daemon
//! ever picked the value up.

use std::path::Path;

use crate::config::Config;
use crate::server_client::ServerInferenceClient;

use super::llm_message::NoLlmReason;
use super::probe::{get_inference_tier, get_tier};
use super::tier::Tier;

/// Where LLM inference goes for this invocation.
#[derive(Debug, Clone)]
pub enum LlmRoute {
    /// The local inference tier serves the LLM. Carries the effective config
    /// whose inference target is that tier's URL.
    Local(Config),
    /// An explicitly configured `server_url` serves the LLM. Carries the
    /// effective config whose inference target is that URL.
    Remote(Config),
    /// No LLM to route to, and why.
    Unavailable(NoLlmReason),
}

impl LlmRoute {
    /// The reason no LLM is available, or `None` when one is.
    pub fn reason(&self) -> Option<NoLlmReason> {
        match self {
            LlmRoute::Unavailable(reason) => Some(*reason),
            _ => None,
        }
    }

    /// The URL LLM requests will be sent to, or `None` when unavailable.
    #[cfg(test)]
    pub fn target_url(&self) -> Option<&str> {
        match self {
            LlmRoute::Local(cfg) | LlmRoute::Remote(cfg) => cfg.resolve_inference_url(),
            LlmRoute::Unavailable(_) => None,
        }
    }

    /// Build the LLM client for this route.
    ///
    /// The `Remote` arm cannot use plain `from_config`: that re-derives
    /// "reached via an explicit remote" from the inference target being unset,
    /// which this route has just set. Losing the flag would make a failure on
    /// the remote tell the user to read `spelunk server logs`, which only ever
    /// reads the local daemon's log.
    pub fn client(&self) -> Option<ServerInferenceClient> {
        match self {
            LlmRoute::Local(cfg) => ServerInferenceClient::from_config(cfg),
            LlmRoute::Remote(cfg) => ServerInferenceClient::from_config_explicit_remote(cfg),
            LlmRoute::Unavailable(_) => None,
        }
    }
}

/// Resolve where this project's LLM calls should go. See the module docs for
/// the rule.
///
/// The remote is probed only on the arm that can actually use it, so steps 1
/// to 3 reach no network at all.
pub async fn resolve_llm_route(cfg: &Config, project_root: &Path) -> LlmRoute {
    // Probing at all would defeat the point of an explicitly offline run.
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    if explicit_offline {
        return LlmRoute::Unavailable(NoLlmReason::Offline);
    }

    let inference_tier = get_inference_tier(cfg).await;
    if let Some(route) = local_route(cfg, project_root, &inference_tier) {
        return route;
    }

    if cfg.server_url.is_none() {
        return LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere);
    }
    remote_route(cfg, project_root, get_tier(cfg).await)
}

/// Steps 2 and 3: decide from the local inference tier alone.
///
/// `None` means neither step applies and the caller may go on to probe an
/// explicit `server_url`.
fn local_route(cfg: &Config, project_root: &Path, inference_tier: &Tier) -> Option<LlmRoute> {
    if inference_tier.caps().is_some_and(|c| c.llm_complete) {
        return Some(LlmRoute::Local(
            inference_tier.effective_config(cfg, project_root),
        ));
    }
    // The privacy guard: an explicitly configured local endpoint means the
    // remote is not an acceptable substitute, only a stale daemon to restart.
    if cfg.llm_url.is_some() {
        return Some(LlmRoute::Unavailable(
            NoLlmReason::LocalConfiguredButNotServed,
        ));
    }
    None
}

/// Step 4: decide from the tier reached by probing `server_url`.
fn remote_route(cfg: &Config, project_root: &Path, remote_tier: &Tier) -> LlmRoute {
    if remote_tier.caps().is_some_and(|c| c.llm_complete)
        && let Some(url) = remote_tier.server_url()
    {
        let mut out = cfg.clone();
        out.inference_url = Some(url.to_string());
        if out.project_id.is_none() {
            out.project_id = Some(cfg.resolve_project_id(project_root));
        }
        return LlmRoute::Remote(out);
    }
    LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere)
}

#[cfg(test)]
mod tests {
    use super::super::state::{Capabilities, EmbedderState};
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ROOT: &str = "/tmp/spelunk-llm-route-fixture";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    fn caps_with_llm(llm_complete: bool) -> Capabilities {
        let mut caps = Capabilities::all();
        caps.llm_complete = llm_complete;
        caps
    }

    fn server_tier(url: &str, llm_complete: bool, auto_discovered: bool) -> Tier {
        Tier::Server {
            url: url.to_string(),
            caps: caps_with_llm(llm_complete),
            auto_discovered,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        }
    }

    // Health body advertising `llm.complete` alongside the usual set.
    fn health_with_llm() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore", "llm.complete"],
            "embedding_dim": spelunk_core::embeddings::EMBEDDING_DIM,
        })
    }

    // Health body from a server with an embedder but no LLM: `explore` is
    // present, which is exactly the version-skew trap keying on it would hit.
    fn health_without_llm() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore"],
            "embedding_dim": spelunk_core::embeddings::EMBEDDING_DIM,
        })
    }

    async fn mock_server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn port_of(uri: &str) -> u16 {
        uri.rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric")
    }

    // Point loopback auto-discovery at `uri` for the duration of the returned
    // guard, then restore whatever was there.
    struct StateDirGuard {
        _tmp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirGuard {
        fn pointing_at(uri: &str) -> Self {
            let tmp = tempfile::TempDir::new().expect("temp state dir");
            let state_dir = tmp.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("create state dir");
            std::fs::write(state_dir.join("server.port"), format!("{}\n", port_of(uri)))
                .expect("write server.port");
            let previous = std::env::var_os("SPELUNK_STATE_DIR");
            unsafe { std::env::set_var("SPELUNK_STATE_DIR", &state_dir) };
            Self {
                _tmp: tmp,
                previous,
            }
        }

        // An empty state dir: loopback auto-discovery finds nothing, and the
        // machine's real daemon on the default port cannot be mistaken for the
        // fixture because nothing is mounted for it either.
        fn empty() -> Self {
            let tmp = tempfile::TempDir::new().expect("temp state dir");
            let state_dir = tmp.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("create state dir");
            let previous = std::env::var_os("SPELUNK_STATE_DIR");
            unsafe { std::env::set_var("SPELUNK_STATE_DIR", &state_dir) };
            Self {
                _tmp: tmp,
                previous,
            }
        }
    }

    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
                }
            }
        }
    }

    // ── explicit offline: no probe at all ────────────────────────────────────

    // `mode = "offline"` must short-circuit before any probe. `server_url` and
    // the loopback both point at mock servers with `expect(0)` health mocks, so
    // a probe that does happen fails the test rather than passing quietly.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn offline_mode_routes_nowhere_and_probes_nothing() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_with_llm()))
            .expect(0)
            .mount(&loopback)
            .await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(spelunk_core::config::SyncMode::Offline),
            ..Default::default()
        };

        let route = resolve_llm_route(&cfg, root()).await;
        assert_eq!(route.reason(), Some(NoLlmReason::Offline), "got {route:?}");
        assert_eq!(
            loopback.received_requests().await.expect("recorded").len(),
            0,
            "explicit offline must not probe anything"
        );
    }

    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn no_server_env_kill_switch_routes_nowhere_and_probes_nothing() {
        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_with_llm()))
            .mount(&loopback)
            .await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
        let route = resolve_llm_route(&Config::default(), root()).await;
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        assert_eq!(route.reason(), Some(NoLlmReason::Offline), "got {route:?}");
        assert_eq!(
            loopback.received_requests().await.expect("recorded").len(),
            0,
            "the kill switch must not probe anything"
        );
    }

    // ── local branch ─────────────────────────────────────────────────────────

    // The founder's own setup with no team server: a loopback daemon serving an
    // LLM must be used, with no `server_url` involved anywhere.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn loopback_with_an_llm_and_no_server_url_routes_local() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
        let loopback = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let route = resolve_llm_route(&Config::default(), root()).await;
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some(loopback.uri().as_str()));
    }

    // The founder's reported scenario: `server_url` set, loopback serving an
    // LLM. Local must win, and the remote must never be probed. The remote is
    // deliberately unroutable, so any attempt surfaces as a real failure.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn loopback_with_an_llm_wins_over_an_llm_capable_server_url() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
        let loopback = mock_server(health_with_llm()).await;
        let remote = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some(remote.uri()),
            project_id: Some("team/proj".to_string()),
            mode: None, // local_first, because server_url is set
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            spelunk_core::config::SyncMode::LocalFirst
        );

        let route = resolve_llm_route(&cfg, root()).await;
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some(loopback.uri().as_str()));
        assert_eq!(
            remote.received_requests().await.expect("recorded").len(),
            0,
            "a usable local LLM must not cause the remote to be contacted at all"
        );
    }

    // The privacy guard. `llm_url` is configured, so the user asked for a local
    // LLM; the running daemon has not picked it up. Falling through to the
    // LLM-capable remote would ship their code somewhere they did not choose,
    // so this must stop, and the remote must receive nothing.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn configured_local_llm_not_served_stops_and_never_reaches_the_remote() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
        let loopback = mock_server(health_without_llm()).await;
        let remote = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some(remote.uri()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };

        let route = resolve_llm_route(&cfg, root()).await;
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed),
            "got {route:?}"
        );
        assert_eq!(
            remote.received_requests().await.expect("recorded").len(),
            0,
            "the remote must not even be probed once a local LLM was configured"
        );
    }

    // No loopback, no `server_url`: nothing to route to, and the reason must be
    // the actionable one rather than the offline one.
    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn nothing_configured_anywhere_reports_no_llm_not_offline() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
        let _state = StateDirGuard::empty();

        let route = resolve_llm_route(&Config::default(), root()).await;
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::NoLlmAnywhere),
            "got {route:?}"
        );
    }

    // ── the two decision steps, exhaustively ─────────────────────────────────
    //
    // `get_tier` caches its probe in a process-wide cell with no reset hook, so
    // the remote arm cannot be driven end to end from a unit test without
    // making the result depend on test ordering. These cover the decision
    // directly; `tests/index_llm_routing.rs` drives the same arms through a
    // real `spelunk` process.

    #[test]
    fn local_route_takes_the_local_llm_when_the_tier_advertises_one() {
        let tier = server_tier("http://127.0.0.1:7777", true, true);
        let route = local_route(&Config::default(), root(), &tier).expect("local arm applies");
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("http://127.0.0.1:7777"));
    }

    // The version-skew guard at the decision layer: a tier advertising
    // `explore` but not `llm.complete` has no LLM route behind it.
    #[test]
    fn local_route_declines_a_tier_that_advertises_explore_but_no_llm() {
        let mut caps = Capabilities::all();
        caps.llm_complete = false;
        assert!(caps.explore);
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps,
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(
            local_route(&Config::default(), root(), &tier).is_none(),
            "explore must never stand in for an LLM"
        );
    }

    #[test]
    fn local_route_stops_when_llm_url_is_set_and_the_tier_serves_no_llm() {
        let cfg = Config {
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("http://127.0.0.1:7777", false, true);
        let route = local_route(&cfg, root(), &tier).expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    // No local server at all plus a configured `llm_url` is still the stale or
    // stopped daemon case, not "no LLM anywhere": the fix is still a restart.
    #[test]
    fn local_route_stops_when_llm_url_is_set_and_no_local_server_is_up() {
        let cfg = Config {
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };
        let route = local_route(&cfg, root(), &Tier::Offline).expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    // The boundary of the privacy guard, pinned so a change to it is visible.
    //
    // In `cloud_first` the inference tier IS the configured `server_url`, so
    // step 2 matches on the remote and step 3 never runs: a set `llm_url` does
    // not stop an LLM call from going to the remote. That is consistent rather
    // than an escape hatch, because `cloud_first` already routes embedding to
    // the same remote, so chunk text leaves the machine there either way. In
    // `local_first`, which is the default whenever `server_url` is set, the
    // guard does apply.
    #[test]
    fn cloud_first_routes_to_the_remote_even_with_llm_url_set() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            mode: Some(spelunk_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:7777", true, false);
        let route = local_route(&cfg, root(), &tier).expect("step 2 matches on the remote");
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("https://team.example:7777"));
    }

    // The same config in `local_first`, for contrast: there the guard stops the
    // call rather than sending it to the remote.
    #[test]
    fn local_first_with_the_same_config_stops_instead_of_using_the_remote() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            spelunk_core::config::SyncMode::LocalFirst
        );
        let tier = server_tier("http://127.0.0.1:7777", false, true);
        let route = local_route(&cfg, root(), &tier).expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    #[test]
    fn local_route_defers_to_the_remote_when_no_local_llm_was_asked_for() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert!(local_route(&cfg, root(), &Tier::Offline).is_none());
        let no_llm = server_tier("http://127.0.0.1:7777", false, true);
        assert!(local_route(&cfg, root(), &no_llm).is_none());
    }

    #[test]
    fn remote_route_targets_the_server_url_when_it_advertises_an_llm() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:7777", true, false);
        let route = remote_route(&cfg, root(), &tier);
        assert!(matches!(route, LlmRoute::Remote(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("https://team.example:7777"));
    }

    // The remote arm must resolve its bearer against the remote's own origin.
    // Pointing the inference target at `server_url` is what makes
    // `bearer_for` (per-origin) ask about the right server rather than the
    // loopback the embed path is using.
    #[test]
    fn remote_route_config_names_the_remote_origin_for_credential_resolution() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:7777", true, false);
        match remote_route(&cfg, root(), &tier) {
            LlmRoute::Remote(eff) => assert_eq!(
                eff.resolve_inference_url(),
                Some("https://team.example:7777"),
                "the remote's own origin is what the bearer must be resolved for"
            ),
            other => panic!("expected the remote arm, got {other:?}"),
        }
    }

    #[test]
    fn remote_route_derives_a_project_id_when_the_config_has_none() {
        let cfg = Config {
            server_url: Some("http://127.0.0.1:7777".to_string()),
            ..Default::default()
        };
        let tier = server_tier("http://127.0.0.1:7777", true, false);
        match remote_route(&cfg, root(), &tier) {
            LlmRoute::Remote(eff) => assert!(
                eff.project_id.is_some(),
                "the client cannot address a project without one"
            ),
            other => panic!("expected the remote arm, got {other:?}"),
        }
    }

    #[test]
    fn remote_route_reports_no_llm_when_the_server_url_has_none() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:7777", false, false);
        assert_eq!(
            remote_route(&cfg, root(), &tier).reason(),
            Some(NoLlmReason::NoLlmAnywhere)
        );
    }

    #[test]
    fn remote_route_reports_no_llm_when_the_server_url_is_unreachable() {
        let cfg = Config {
            server_url: Some("https://team.example:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert_eq!(
            remote_route(&cfg, root(), &Tier::Offline).reason(),
            Some(NoLlmReason::NoLlmAnywhere)
        );
    }

    // ── the route carries the explicit-remote distinction into the client ────

    #[test]
    fn unavailable_route_builds_no_client() {
        assert!(
            LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere)
                .client()
                .is_none()
        );
    }
}
