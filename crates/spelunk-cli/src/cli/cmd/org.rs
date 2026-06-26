//! `spelunk org switch <org>` — silently re-scope the session to another
//! organisation without a new device login (ADR-047).
//!
//! Resolves the org slug to its local UUID via cloud-api `GET /v1/me`, then
//! uses the stored refresh token directly against WorkOS `/authenticate`
//! (refresh grant) with `organization_id`, and persists the rotated tokens.
//! Shared org-resolution and token-persistence helpers also back
//! `spelunk login --org`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use uuid::Uuid;

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, MeOrg};

#[derive(Args, Debug)]
pub struct OrgArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud)
    #[arg(long, env = "SPELUNK_CLOUD_URL", global = true)]
    pub cloud_url: Option<String>,

    #[command(subcommand)]
    pub command: OrgCommand,
}

#[derive(Subcommand, Debug)]
pub enum OrgCommand {
    /// Switch the active organization (silently, using the stored refresh token)
    Switch(OrgSwitchArgs),
}

#[derive(Args, Debug)]
pub struct OrgSwitchArgs {
    /// Organization to switch to. Accepts a local org UUID or an org slug; a
    /// slug is resolved to its UUID via `GET /v1/me` before switching.
    pub org: String,
}

pub async fn org(args: OrgArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/')
        .to_string();

    let workos_url = auth_api::workos_url();
    let client_id = auth_api::workos_client_id(&cloud_url);
    match args.command {
        OrgCommand::Switch(switch_args) => {
            org_switch(&workos_url, &cloud_url, &client_id, &switch_args.org).await
        }
    }
}

async fn org_switch(workos_url: &str, cloud_url: &str, client_id: &str, org: &str) -> Result<()> {
    let cfg = config::Config::load(None).context("loading config")?;
    let auth = cfg.auth.as_ref().ok_or_else(|| {
        anyhow::anyhow!("Not logged in. Run `spelunk login` before switching organizations.")
    })?;

    let client = auth_api::build_client()?;
    let tokens = switch_org(&client, workos_url, cloud_url, client_id, auth, org).await?;
    persist_tokens(&tokens)?;
    println!("Switched to organization '{org}'.");
    Ok(())
}

/// Silently switch the session to `org` using the stored refresh token.
///
/// `org` may be a local org UUID or a slug. A slug is resolved to its local org
/// UUID via cloud-api `GET /v1/me`; the UUID is then sent as `organization_id`
/// to WorkOS `/authenticate` (refresh grant), which rotates the tokens scoped
/// to that organisation (or returns an `organization_not_found` membership
/// error).
pub async fn switch_org(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    org: &str,
) -> Result<AuthTokens> {
    let org_uuid = resolve_org_uuid(client, cloud_url, auth, org).await?;
    let success = auth_api::refresh_token(
        client,
        workos_url,
        client_id,
        &auth.refresh_token,
        Some(&org_uuid),
    )
    .await?;
    Ok(success.into_auth_tokens())
}

/// Resolve `arg` to a local org UUID.
///
/// A value that already parses as a UUID is used directly. Otherwise `arg` is
/// treated as a slug and matched against the caller's `orgs[]` from `GET /v1/me`,
/// returning the matching entry's `id` (the local org UUID). No match is a clear
/// error.
async fn resolve_org_uuid(
    client: &reqwest::Client,
    cloud_url: &str,
    auth: &AuthTokens,
    arg: &str,
) -> Result<String> {
    if Uuid::parse_str(arg).is_ok() {
        return Ok(arg.to_string());
    }

    let me = auth_api::fetch_me(client, cloud_url, &auth.access_token).await?;
    resolve_slug_to_uuid(&me.orgs, arg)
}

/// Find the `orgs[]` entry whose slug equals `slug` and return its local UUID.
fn resolve_slug_to_uuid(orgs: &[MeOrg], slug: &str) -> Result<String> {
    orgs.iter()
        .find(|o| o.slug == slug)
        .map(|o| o.id.clone())
        .ok_or_else(|| anyhow::anyhow!("not a member of org '{slug}', or unknown org"))
}

/// Persist rotated/issued tokens to the `[auth]` table (written `0600`).
pub fn persist_tokens(tokens: &AuthTokens) -> Result<()> {
    config::save_auth_tokens(tokens).context("saving auth tokens to ~/.config/spelunk/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me_orgs() -> Vec<MeOrg> {
        vec![
            MeOrg {
                id: "11111111-1111-1111-1111-111111111111".into(),
                name: "Acme".into(),
                slug: "acme".into(),
            },
            MeOrg {
                id: "22222222-2222-2222-2222-222222222222".into(),
                name: "Beta".into(),
                slug: "beta".into(),
            },
        ]
    }

    /// A slug resolves to the matching entry's local org UUID.
    #[test]
    fn resolve_slug_to_uuid_matches_slug() {
        let uuid = resolve_slug_to_uuid(&me_orgs(), "beta").unwrap();
        assert_eq!(uuid, "22222222-2222-2222-2222-222222222222");
    }

    /// An unknown slug is a clear membership error.
    #[test]
    fn resolve_slug_to_uuid_unknown_errors() {
        let err = resolve_slug_to_uuid(&me_orgs(), "gamma").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "error should name the slug: {msg}");
    }

    // ── switch_org wire-level tests (WorkOS-direct, ADR-047) ──────────────────
    //
    // `switch_org` makes two calls: cloud-api `GET /v1/me` (slug → UUID) and
    // WorkOS `POST /user_management/authenticate` (refresh grant). Both are
    // mounted on one mock server here, used as both `cloud_url` and `workos_url`.
    mod switch_org_wire {
        use serde_json::Value;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;
        use spelunk_core::config::AuthTokens;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";
        const CLIENT_ID: &str = "client_test";

        fn auth() -> AuthTokens {
            AuthTokens {
                access_token: "at-current".into(),
                refresh_token: "rt-current".into(),
                expires_at: 4_000_000_000,
                org_id: "00000000-0000-0000-0000-000000000000".into(),
            }
        }

        /// Build an unsigned JWT whose payload carries `exp` and `org_id`, so the
        /// CLI's claim decode resolves the rotated session's org and expiry.
        fn jwt(org_id: &str, exp: i64) -> String {
            fn b64url(bytes: &[u8]) -> String {
                const A: &[u8] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
                let mut out = String::new();
                for chunk in bytes.chunks(3) {
                    let b = [
                        chunk[0],
                        *chunk.get(1).unwrap_or(&0),
                        *chunk.get(2).unwrap_or(&0),
                    ];
                    let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                    out.push(A[((n >> 18) & 0x3f) as usize] as char);
                    out.push(A[((n >> 12) & 0x3f) as usize] as char);
                    if chunk.len() > 1 {
                        out.push(A[((n >> 6) & 0x3f) as usize] as char);
                    }
                    if chunk.len() > 2 {
                        out.push(A[(n & 0x3f) as usize] as char);
                    }
                }
                out
            }
            let payload = serde_json::json!({ "exp": exp, "org_id": org_id }).to_string();
            format!("{}.{}.sig", b64url(b"{}"), b64url(payload.as_bytes()))
        }

        /// WorkOS `/authenticate` success body (access_token is a JWT).
        fn workos_success(org_id: &str) -> Value {
            serde_json::json!({
                "access_token": jwt(org_id, 4_000_000_100),
                "refresh_token": "rt-new",
                "organization_id": org_id,
            })
        }

        /// Asserts the WorkOS form body carries the expected `organization_id`
        /// (the resolved local UUID) and the refresh grant fields.
        struct AssertForm(&'static str);
        impl Respond for AssertForm {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    body.contains(&format!("organization_id={}", self.0)),
                    "organization_id must be the local org UUID, got body: {body}"
                );
                assert!(
                    body.contains("grant_type=refresh_token"),
                    "must use the refresh-token grant, got body: {body}"
                );
                ResponseTemplate::new(200).set_body_json(workos_success(self.0))
            }
        }

        /// `switch_org(<slug>)` calls GET /v1/me, resolves the slug to its local
        /// UUID, and POSTs that UUID as `organization_id` to WorkOS.
        #[tokio::test]
        async fn slug_resolves_via_me_then_switches() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": "11111111-1111-1111-1111-111111111111",
                          "name": "Acme", "slug": "acme", "role": "member" },
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta", "role": "admin" }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_UUID))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "beta",
            )
            .await
            .expect("slug switch should succeed");
            assert_eq!(tokens.refresh_token, "rt-new");
            assert_eq!(tokens.org_id, BETA_UUID);
        }

        /// A UUID arg is passed straight through as `organization_id` with no
        /// GET /v1/me lookup.
        #[tokio::test]
        async fn uuid_passed_directly_skips_me() {
            let server = MockServer::start().await;

            // No /v1/me mock mounted: if it is hit, the request 404s.
            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_UUID))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                BETA_UUID,
            )
            .await
            .expect("uuid switch should succeed");
            assert_eq!(tokens.org_id, BETA_UUID);
        }

        /// A slug not present in `orgs[]` is a clear membership error and no
        /// token call is made.
        #[tokio::test]
        async fn unknown_slug_errors_before_token_call() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta", "role": "admin" }
                    ]
                })))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(200).set_body_json(workos_success(BETA_UUID)))
                .expect(0)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "gamma",
            )
            .await
            .expect_err("unknown slug should error");
            assert!(
                err.to_string().contains("gamma"),
                "error should name the slug: {err}"
            );
        }

        /// A slug that resolves to a UUID but is rejected by WorkOS with
        /// `organization_not_found` surfaces the clear membership error — WorkOS
        /// is the authority on membership even after a local slug match.
        #[tokio::test]
        async fn organization_not_found_maps_to_membership_error() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta", "role": "admin" }
                    ]
                })))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "organization_not_found"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "beta",
            )
            .await
            .expect_err("an organization_not_found must error");
            assert!(
                err.to_string().contains("not a member"),
                "expected a clear membership error, got: {err}"
            );
        }
    }
}
