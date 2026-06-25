//! `spelunk org switch <org>` — silently re-scope the session to another
//! organisation without a new device login (ADR-045).
//!
//! Uses the stored refresh token against `POST /v1/auth/token` with
//! `organization_id`, then persists the rotated tokens. Shared org-resolution
//! and token-persistence helpers also back `spelunk login --org`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use uuid::Uuid;

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, MeOrg, Organization};

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

    match args.command {
        OrgCommand::Switch(switch_args) => org_switch(&cloud_url, &switch_args.org).await,
    }
}

async fn org_switch(cloud_url: &str, org: &str) -> Result<()> {
    let cfg = config::Config::load(None).context("loading config")?;
    let auth = cfg.auth.as_ref().ok_or_else(|| {
        anyhow::anyhow!("Not logged in. Run `spelunk login` before switching organizations.")
    })?;

    let client = auth_api::build_client()?;
    let tokens = switch_org(&client, cloud_url, auth, org).await?;
    persist_tokens(&tokens)?;
    println!("Switched to organization '{org}'.");
    Ok(())
}

/// Silently switch the session to `org` using the stored refresh token.
///
/// `org` may be a local org UUID or a slug. A slug is resolved to its local org
/// UUID via `GET /v1/me`; the UUID is then sent as `organization_id` to
/// `POST /v1/auth/token`, which rotates the tokens scoped to that organisation
/// (or returns `org_not_member`).
pub async fn switch_org(
    client: &reqwest::Client,
    cloud_url: &str,
    auth: &AuthTokens,
    org: &str,
) -> Result<AuthTokens> {
    let org_uuid = resolve_org_uuid(client, cloud_url, auth, org).await?;
    let success =
        auth_api::refresh_token(client, cloud_url, &auth.refresh_token, Some(&org_uuid)).await?;
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

/// Resolve which organisation to act on during a multi-org device login.
///
/// When `org_flag` (a slug) is given, it must match an entry in `organizations`
/// (by slug, or by id as a fallback); otherwise `prompt` is invoked so the
/// operator can choose interactively.
pub fn resolve_org_for_switch(
    organizations: &[Organization],
    org_flag: Option<&str>,
    prompt: impl FnOnce(&[Organization]) -> Result<Organization>,
) -> Result<Organization> {
    match org_flag {
        Some(slug) => organizations
            .iter()
            .find(|o| o.slug == slug || o.id == slug)
            .cloned()
            .ok_or_else(|| {
                let available = organizations
                    .iter()
                    .map(|o| o.slug.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::anyhow!(
                    "Organization '{slug}' is not one of your memberships. Available: {available}"
                )
            }),
        None => prompt(organizations),
    }
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

    fn orgs() -> Vec<Organization> {
        vec![
            Organization {
                id: "org_a".into(),
                name: "Acme".into(),
                slug: "acme".into(),
            },
            Organization {
                id: "org_b".into(),
                name: "Beta".into(),
                slug: "beta".into(),
            },
        ]
    }

    fn unreachable_prompt(_: &[Organization]) -> Result<Organization> {
        panic!("prompt must not be called when --org matches");
    }

    /// `--org <slug>` selects the matching org without prompting.
    #[test]
    fn resolve_matches_org_flag_by_slug() {
        let org = resolve_org_for_switch(&orgs(), Some("beta"), unreachable_prompt).unwrap();
        assert_eq!(org.id, "org_b");
    }

    /// `--org` also accepts an org's local UUID directly (id fallback).
    #[test]
    fn resolve_matches_org_flag_by_id() {
        let org = resolve_org_for_switch(&orgs(), Some("org_a"), unreachable_prompt).unwrap();
        assert_eq!(org.slug, "acme");
    }

    /// A `--org` that is not a membership is a clear error, not a prompt.
    #[test]
    fn resolve_unknown_org_flag_errors() {
        let err = resolve_org_for_switch(&orgs(), Some("gamma"), unreachable_prompt).unwrap_err();
        assert!(err.to_string().contains("gamma"));
    }

    /// With no `--org`, the prompt is used.
    #[test]
    fn resolve_without_flag_prompts() {
        let chosen = resolve_org_for_switch(&orgs(), None, |o| Ok(o[0].clone())).unwrap();
        assert_eq!(chosen.id, "org_a");
    }

    // ── switch_org wire-level tests ───────────────────────────────────────────
    mod switch_org_wire {
        use serde_json::Value;
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;
        use spelunk_core::config::AuthTokens;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";

        fn auth() -> AuthTokens {
            AuthTokens {
                access_token: "at-current".into(),
                refresh_token: "rt-current".into(),
                expires_at: 4_000_000_000,
                org_id: "00000000-0000-0000-0000-000000000000".into(),
            }
        }

        fn token_success(org_id: &str) -> Value {
            serde_json::json!({
                "token_type": "workos",
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_at": 4_000_000_100_i64,
                "org_id": org_id,
            })
        }

        /// Asserts the `/v1/auth/token` body carries the expected
        /// `organization_id` (the resolved local UUID).
        struct AssertOrgId(&'static str);
        impl Respond for AssertOrgId {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["organization_id"].as_str(),
                    Some(self.0),
                    "organization_id must be the local org UUID"
                );
                ResponseTemplate::new(200).set_body_json(token_success(self.0))
            }
        }

        /// `switch_org(<slug>)` calls GET /v1/me, resolves the slug to its local
        /// UUID, and POSTs that UUID as `organization_id`.
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
                .and(path("/v1/auth/token"))
                .respond_with(AssertOrgId(BETA_UUID))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(&client, &server.uri(), &auth(), "beta")
                .await
                .expect("slug switch should succeed");
            assert_eq!(tokens.access_token, "at-new");
            assert_eq!(tokens.org_id, BETA_UUID);
        }

        /// A UUID arg is passed straight through as `organization_id` with no
        /// GET /v1/me lookup.
        #[tokio::test]
        async fn uuid_passed_directly_skips_me() {
            let server = MockServer::start().await;

            // No /v1/me mock mounted: if it is hit, the request 404s and the
            // body assertion below would never run with the right value.
            Mock::given(method("POST"))
                .and(path("/v1/auth/token"))
                .and(body_partial_json(
                    serde_json::json!({ "organization_id": BETA_UUID }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(token_success(BETA_UUID)))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(&client, &server.uri(), &auth(), BETA_UUID)
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
                .and(path("/v1/auth/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(token_success(BETA_UUID)))
                .expect(0)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(&client, &server.uri(), &auth(), "gamma")
                .await
                .expect_err("unknown slug should error");
            assert!(
                err.to_string().contains("gamma"),
                "error should name the slug: {err}"
            );
        }

        /// A slug that resolves to a UUID but is rejected by the token endpoint
        /// with `403 org_not_member` surfaces the clear membership error — the
        /// server is the authority on membership even after a local slug match.
        #[tokio::test]
        async fn token_403_org_not_member_maps_to_membership_error() {
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
                .and(path("/v1/auth/token"))
                .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": "org_not_member"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(&client, &server.uri(), &auth(), "beta")
                .await
                .expect_err("a 403 org_not_member must error");
            assert!(
                err.to_string().contains("not a member"),
                "expected a clear membership error, got: {err}"
            );
        }
    }
}
