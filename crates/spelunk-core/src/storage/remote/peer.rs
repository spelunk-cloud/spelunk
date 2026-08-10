//! Which memory dialect the configured `server_url` speaks.
//!
//! `cloud_first` routes memory CRUD to whatever server is configured, and that
//! is either a self-hosted OSS team server or the hosted cloud API. The two
//! expose different memory routes, so the dialect has to be settled once, at
//! backend-open time, rather than branched on inside every method.

use std::time::Duration;

use serde::Deserialize;

/// How long the probe waits before falling back.
///
/// Deliberately far below the 30s CRUD timeout: a server that cannot answer a
/// liveness question promptly is not one this client should stall on, and the
/// real request that follows carries its own, longer budget. An unreachable
/// server therefore costs this much before the command proceeds to fail on its
/// own terms, which is why it is kept small rather than merely "under 30s":
/// platforms where a connection to an unreachable host hangs rather than
/// refusing pay it in full, on every memory command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The capability that separates the two peers.
///
/// The OSS team server builds its `/v1/health` capability list from `memory`,
/// `index.embed`/`search.semantic` and `explore`/`llm.complete`, and never
/// advertises SSE streaming; the hosted API does. Keying on an already-shipped
/// capability avoids adding a field to either peer just to tell them apart.
const CLOUD_ONLY_CAPABILITY: &str = "memory.stream";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::storage) enum PeerDialect {
    /// `POST memory/search`, `POST memory/{id}/archive`, `GET stats`, and a
    /// `source_ref`-filtered `GET memory`.
    TeamServer,
    /// `GET memory?q=`, `DELETE memory/{id}`, batch edges for supersede.
    CloudApi,
}

#[derive(Deserialize)]
struct HealthBody {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Probe `{base_url}/v1/health` and report which dialect to speak.
///
/// Every uncertain answer resolves to [`PeerDialect::TeamServer`]: an
/// unreachable, slow, or pre-JSON-health server then takes exactly the code
/// path it took before this probe existed, so no self-hosted deployment gains
/// a new failure mode at open time. A genuinely unreachable cloud server still
/// fails, but on the CRUD request that follows, exactly as it does today.
///
/// Sent unauthenticated: `/v1/health` requires no auth on either peer, and a
/// bearer minted for one origin has no business being offered to a server
/// whose identity is still being established.
pub(in crate::storage) async fn detect_dialect(
    client: &reqwest::Client,
    base_url: &str,
) -> PeerDialect {
    let url = format!("{}/v1/health", base_url.trim_end_matches('/'));
    let Ok(resp) = client.get(&url).timeout(PROBE_TIMEOUT).send().await else {
        return PeerDialect::TeamServer;
    };
    if !resp.status().is_success() {
        return PeerDialect::TeamServer;
    }
    match resp.json::<HealthBody>().await {
        Ok(body) if body.capabilities.iter().any(|c| c == CLOUD_ONLY_CAPABILITY) => {
            PeerDialect::CloudApi
        }
        _ => PeerDialect::TeamServer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    async fn health_server(body: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(body)
            .mount(&server)
            .await;
        server
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn memory_stream_capability_selects_the_cloud_dialect() {
        let server = health_server(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic", "llm.complete", "memory.stream"],
        })))
        .await;
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::CloudApi
        );
    }

    #[tokio::test]
    async fn team_server_capabilities_select_the_team_dialect() {
        let server = health_server(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic"],
        })))
        .await;
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::TeamServer
        );
    }

    // TEST-NET-1 (RFC 5737) is reserved for documentation and never routes, so
    // this cannot be answered by whatever else the suite has bound. Reusing a
    // just-dropped mock server's port would race: another test can claim it.
    #[tokio::test]
    async fn an_unreachable_probe_falls_back_to_the_team_dialect() {
        let uri = "http://192.0.2.1:7777".to_string();
        assert_eq!(
            detect_dialect(&client(), &uri).await,
            PeerDialect::TeamServer
        );
    }

    #[tokio::test]
    async fn a_legacy_plain_text_health_body_falls_back_to_the_team_dialect() {
        let server = health_server(ResponseTemplate::new(200).set_body_string("ok")).await;
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::TeamServer
        );
    }

    #[tokio::test]
    async fn a_health_body_without_capabilities_falls_back_to_the_team_dialect() {
        let server = health_server(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "ok"})),
        )
        .await;
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::TeamServer
        );
    }

    #[tokio::test]
    async fn a_non_success_health_status_falls_back_to_the_team_dialect() {
        let server = health_server(ResponseTemplate::new(503)).await;
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::TeamServer
        );
    }

    #[tokio::test]
    async fn the_probe_never_sends_a_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(|req: &Request| {
                let leaked = req.headers.contains_key("authorization");
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "status": "ok",
                    "capabilities": if leaked { vec!["memory.stream"] } else { vec![] },
                }))
            })
            .mount(&server)
            .await;

        // The probe leaks a credential iff the mock saw an Authorization
        // header, in which case it answers with the cloud capability.
        assert_eq!(
            detect_dialect(&client(), &server.uri()).await,
            PeerDialect::TeamServer,
            "the peer probe must not offer a bearer to an unidentified server"
        );
    }
}
