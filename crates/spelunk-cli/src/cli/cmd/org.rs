//! `spelunk org switch <org>` — silently re-scope the session to another
//! organisation without a new device login.
//!
//! Resolves the org argument to its **WorkOS org id** (`org_…`) — that is what
//! WorkOS's `/authenticate` refresh grant expects in `organization_id`, NOT the
//! cloud-api local org UUID. Resolution rules:
//!
//! - a raw `org_…` value is already a WorkOS org id and is used directly;
//! - a slug or a local org UUID is mapped to its `workos_org_id` via cloud-api
//!   `GET /v1/me`.
//!
//! The resolved WorkOS org id is then sent to WorkOS `/authenticate` (refresh
//! grant) using the stored refresh token, and the rotated tokens are persisted.
//! Shared org-resolution and token-persistence helpers also back
//! `spelunk login --org`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

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
    /// Organization to switch to. Accepts a WorkOS org id (`org_…`), an org
    /// slug, or a local org UUID. A slug or local UUID is resolved to its
    /// WorkOS org id via `GET /v1/me` before switching.
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
/// `org` may be a WorkOS org id (`org_…`), an org slug, or a local org UUID.
/// A slug or local UUID is resolved to its **WorkOS org id** via cloud-api
/// `GET /v1/me`; that WorkOS org id is then sent as `organization_id` to WorkOS
/// `/authenticate` (refresh grant), which rotates the tokens scoped to that
/// organisation (or returns an `organization_not_found` membership error).
///
/// The access token used for the `GET /v1/me` lookup is refreshed first if it
/// has expired (WorkOS access tokens are ~5-minute-lived), so a paused session
/// does not fail the lookup with a `401`.
pub async fn switch_org(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    org: &str,
) -> Result<AuthTokens> {
    let workos_org_id =
        resolve_workos_org_id(client, workos_url, cloud_url, client_id, auth, org).await?;
    let success = auth_api::refresh_token(
        client,
        workos_url,
        client_id,
        &auth.refresh_token,
        Some(&workos_org_id),
    )
    .await?;
    Ok(success.into_auth_tokens())
}

/// Resolve `arg` to a **WorkOS org id** (`org_…`).
///
/// - A value already shaped like a WorkOS org id (`org_…`) is used directly,
///   with no `GET /v1/me` lookup.
/// - Any other value (a slug or a local org UUID) is matched against the
///   caller's `orgs[]` from `GET /v1/me` and mapped to the matching entry's
///   `workos_org_id`. No match — or a matched entry whose `workos_org_id` is
///   absent — is a clear error.
///
/// The `/v1/me` call refreshes an expired access token first (persisting the
/// rotated tokens) so a paused session does not 401.
async fn resolve_workos_org_id(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    arg: &str,
) -> Result<String> {
    if is_workos_org_id(arg) {
        return Ok(arg.to_string());
    }

    let fresh =
        auth_api::ensure_fresh_token(client, workos_url, client_id, auth, persist_tokens).await?;
    let me = auth_api::fetch_me(client, cloud_url, &fresh.access_token).await?;
    resolve_arg_to_workos_org_id(&me.orgs, arg)
}

/// Whether `arg` is already a WorkOS org id (`org_…`), in which case it is the
/// value WorkOS expects and needs no `/v1/me` lookup.
fn is_workos_org_id(arg: &str) -> bool {
    arg.starts_with("org_")
}

/// Find the `orgs[]` entry matching `arg` (by slug or local UUID) and return its
/// WorkOS org id (`workos_org_id`).
///
/// WorkOS's refresh grant keys `organization_id` on the provider org id, not the
/// cloud-api local UUID, so this maps either human identifier onto the right one.
fn resolve_arg_to_workos_org_id(orgs: &[MeOrg], arg: &str) -> Result<String> {
    let entry = orgs
        .iter()
        .find(|o| o.slug == arg || o.id == arg)
        .ok_or_else(|| anyhow::anyhow!("not a member of org '{arg}', or unknown org"))?;
    entry.workos_org_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "org '{arg}' has no WorkOS org id on record; cannot switch \
             (re-run `spelunk login` or contact support)"
        )
    })
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
                workos_org_id: Some("org_acme".into()),
            },
            MeOrg {
                id: "22222222-2222-2222-2222-222222222222".into(),
                name: "Beta".into(),
                slug: "beta".into(),
                workos_org_id: Some("org_beta".into()),
            },
        ]
    }

    /// `org_…` is already a WorkOS org id — no lookup, used verbatim.
    #[test]
    fn is_workos_org_id_recognises_provider_ids() {
        assert!(is_workos_org_id("org_01ABCDEF"));
        assert!(!is_workos_org_id("beta"));
        assert!(!is_workos_org_id("22222222-2222-2222-2222-222222222222"));
    }

    /// A slug resolves to the matching entry's WorkOS org id (NOT its local UUID).
    #[test]
    fn resolve_arg_to_workos_org_id_matches_slug() {
        let id = resolve_arg_to_workos_org_id(&me_orgs(), "beta").unwrap();
        assert_eq!(id, "org_beta");
    }

    /// A local org UUID resolves to that entry's WorkOS org id.
    #[test]
    fn resolve_arg_to_workos_org_id_matches_local_uuid() {
        let id = resolve_arg_to_workos_org_id(&me_orgs(), "11111111-1111-1111-1111-111111111111")
            .unwrap();
        assert_eq!(id, "org_acme");
    }

    /// An unknown arg is a clear membership error.
    #[test]
    fn resolve_arg_to_workos_org_id_unknown_errors() {
        let err = resolve_arg_to_workos_org_id(&me_orgs(), "gamma").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "error should name the arg: {msg}");
    }

    /// A matched entry whose `workos_org_id` is absent is a clear error, never a
    /// silent fall-through to the local UUID.
    #[test]
    fn resolve_arg_to_workos_org_id_missing_workos_id_errors() {
        let orgs = vec![MeOrg {
            id: "33333333-3333-3333-3333-333333333333".into(),
            name: "Gamma".into(),
            slug: "gamma".into(),
            workos_org_id: None,
        }];
        let err = resolve_arg_to_workos_org_id(&orgs, "gamma").unwrap_err();
        assert!(
            err.to_string().contains("WorkOS org id"),
            "error should explain the missing WorkOS org id: {err}"
        );
    }

    // ── switch_org wire-level tests (WorkOS-direct) ────────────────────────────
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
        /// WorkOS provider org id for Beta — this is what must reach WorkOS as
        /// `organization_id`, NOT the local `BETA_UUID`.
        const BETA_WORKOS: &str = "org_beta01";
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
        /// (the resolved WorkOS org id, `org_…`) and the refresh grant fields.
        struct AssertForm(&'static str);
        impl Respond for AssertForm {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    body.contains(&format!("organization_id={}", self.0)),
                    "organization_id must be the WorkOS org id, got body: {body}"
                );
                assert!(
                    body.contains("grant_type=refresh_token"),
                    "must use the refresh-token grant, got body: {body}"
                );
                ResponseTemplate::new(200).set_body_json(workos_success(self.0))
            }
        }

        /// `switch_org(<slug>)` calls GET /v1/me, resolves the slug to its WorkOS
        /// org id, and POSTs that WorkOS org id (NOT the local UUID) as
        /// `organization_id` to WorkOS.
        #[tokio::test]
        async fn slug_resolves_via_me_then_switches() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": "11111111-1111-1111-1111-111111111111",
                          "name": "Acme", "slug": "acme",
                          "workos_org_id": "org_acme01", "role": "member" },
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
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
            // Rotated session org_id comes from the JWT/echo = the WorkOS org id.
            assert_eq!(tokens.org_id, BETA_WORKOS);
        }

        /// A local org UUID arg is resolved via GET /v1/me to its WorkOS org id,
        /// which is what reaches WorkOS as `organization_id`.
        #[tokio::test]
        async fn local_uuid_resolves_to_workos_org_id() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
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
            .expect("local-uuid switch should succeed");
            assert_eq!(tokens.org_id, BETA_WORKOS);
        }

        /// An `org_…` arg is already a WorkOS org id and is passed straight
        /// through as `organization_id` with no GET /v1/me lookup.
        #[tokio::test]
        async fn workos_org_id_passed_directly_skips_me() {
            let server = MockServer::start().await;

            // No /v1/me mock mounted: if it is hit, the request 404s.
            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
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
                BETA_WORKOS,
            )
            .await
            .expect("workos-org-id switch should succeed");
            assert_eq!(tokens.org_id, BETA_WORKOS);
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
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(200).set_body_json(workos_success(BETA_WORKOS)))
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
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
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
