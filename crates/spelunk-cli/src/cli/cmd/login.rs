//! `spelunk login` — WorkOS device-authorization grant, cloud-api-proxied (ADR-045).
//!
//! Flow
//! ----
//! 1. POST /v1/auth/device → device_code (opaque), user_code, verification_uri.
//! 2. Print the verification URL and user code for the operator.
//! 3. Poll POST /v1/auth/device/token every `interval` seconds (RFC 8628):
//!    - single-org success            → persist tokens, done
//!    - organization_selection_required → pick via `--org` or prompt, then
//!      POST /v1/auth/device/select-org → persist tokens
//!    - authorization_pending          → keep polling
//!    - slow_down                      → increase interval by 5 s
//!    - expired_token / access_denied  → exit 1
//!    - MFA / step-up challenge        → print "complete in browser", keep polling
//!
//! When the operator is already logged in with a valid refresh token and passes
//! `--org <slug>`, login short-circuits to a silent org-switch (no device
//! re-entry) via `org switch`.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, Organization, PollOutcome, TokenSuccess};
use super::org::{persist_tokens, resolve_org_for_switch, switch_org};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud)
    #[arg(long, env = "SPELUNK_CLOUD_URL")]
    pub cloud_url: Option<String>,

    /// Organization to log into (slug). For multi-org accounts this selects the
    /// org non-interactively; when already logged in it silently re-scopes the
    /// session without a new device login.
    #[arg(long)]
    pub org: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/')
        .to_string();

    let client = auth_api::build_client()?;

    // Already logged in with a valid refresh token + `--org`: silent re-scope.
    if let Some(org_slug) = &args.org {
        let cfg = config::Config::load(None).context("loading config")?;
        if let Some(auth) = cfg.auth.as_ref() {
            let tokens = switch_org(&client, &cloud_url, auth, org_slug).await?;
            persist_tokens(&tokens)?;
            println!("Switched to organization '{org_slug}'.");
            return Ok(());
        }
    }

    // ── Step 1: initiate device authorization ─────────────────────────────────
    let device = auth_api::initiate_device(&client, &cloud_url).await?;

    // ── Step 2: prompt the user ───────────────────────────────────────────────
    println!();
    println!("Open the following URL in your browser:");
    println!();
    println!("  {}", device.verification_uri);
    println!();
    println!("Enter the code: {}", device.user_code);
    println!();

    if let Some(ref complete_url) = device.verification_uri_complete
        && complete_url != &device.verification_uri
    {
        println!("Or open this direct link (code pre-filled):\n  {complete_url}");
        println!();
    }

    println!(
        "Waiting for authorization (expires in {} s)...",
        device.expires_in
    );

    // ── Step 3: polling loop ──────────────────────────────────────────────────
    let mut interval_secs = device.interval.max(5);
    let mut consecutive_errors: u32 = 0;
    let mut challenge_announced = false;

    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match auth_api::poll_token(&client, &cloud_url, &device.device_code).await {
            PollOutcome::Success(token) => {
                finish_login(token.into_auth_tokens())?;
                return Ok(());
            }
            PollOutcome::SelectOrg {
                pending_token,
                organizations,
            } => {
                let token = complete_org_selection(
                    &client,
                    &cloud_url,
                    &pending_token,
                    &organizations,
                    args.org.as_deref(),
                )
                .await?;
                finish_login(token.into_auth_tokens())?;
                return Ok(());
            }
            PollOutcome::Pending => {
                print!(".");
                let _ = std::io::stdout().flush();
                consecutive_errors = 0;
            }
            PollOutcome::SlowDown => {
                interval_secs += 5;
                consecutive_errors = 0;
            }
            PollOutcome::RateLimit => {
                interval_secs *= 2;
                consecutive_errors = 0;
            }
            PollOutcome::Challenge(url) => {
                if !challenge_announced {
                    match url {
                        Some(u) => eprintln!(
                            "\nAdditional verification required — complete it in your browser:\n  {u}"
                        ),
                        None => eprintln!(
                            "\nAdditional verification required — complete it in your browser."
                        ),
                    }
                    challenge_announced = true;
                }
                consecutive_errors = 0;
            }
            PollOutcome::Expired => {
                eprintln!("\nLogin timed out. Run `spelunk login` again.");
                std::process::exit(1);
            }
            PollOutcome::Denied => {
                eprintln!("\nLogin was denied.");
                std::process::exit(1);
            }
            PollOutcome::InvalidGrant(msg) => {
                eprintln!("\nLogin failed: {msg}");
                std::process::exit(1);
            }
            PollOutcome::Error(err) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(err.context("polling for token failed 3 times in a row"));
                }
                tracing::warn!("token poll error (attempt {consecutive_errors}/3): {err:#}");
            }
        }
    }
}

/// Persist tokens and print the success message.
fn finish_login(tokens: AuthTokens) -> Result<()> {
    // Write before printing so a write error surfaces before the user believes
    // they are logged in.
    persist_tokens(&tokens)?;
    println!("\nLogin successful.");
    Ok(())
}

/// Resolve a multi-org device login: honour `--org <slug>` when given, otherwise
/// prompt the operator to pick from `organizations`, then call select-org.
async fn complete_org_selection(
    client: &reqwest::Client,
    cloud_url: &str,
    pending_token: &str,
    organizations: &[Organization],
    org_flag: Option<&str>,
) -> Result<TokenSuccess> {
    let org = resolve_org_for_switch(organizations, org_flag, prompt_org_choice)?;
    auth_api::select_org(client, cloud_url, pending_token, &org.id).await
}

/// Interactively prompt the operator to choose an organisation by number.
fn prompt_org_choice(organizations: &[Organization]) -> Result<Organization> {
    println!("\nYou belong to multiple organizations. Choose one:");
    for (i, org) in organizations.iter().enumerate() {
        println!("  {}) {} ({})", i + 1, org.name, org.slug);
    }
    loop {
        print!("Enter a number (1-{}): ", organizations.len());
        std::io::stdout().flush().ok();

        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading organisation choice")?;
        match line.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= organizations.len() => {
                return Ok(organizations[n - 1].clone());
            }
            _ => eprintln!("Invalid choice; try again."),
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use spelunk_core::config::{self, AuthTokens};
    use tempfile::TempDir;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    use crate::cli::cmd::auth_api;

    struct NonEmptyBody;
    impl Match for NonEmptyBody {
        fn matches(&self, request: &Request) -> bool {
            !request.body.is_empty()
        }
    }

    /// `initiate_device` sends a non-empty JSON body so the fronting proxy does
    /// not reject the request with 411 (GH #434).
    #[tokio::test]
    async fn initiate_device_sends_body() {
        let server = MockServer::start().await;
        let device_json = serde_json::json!({
            "device_code": "dc-123",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://example.test/device",
            "expires_in": 600,
            "interval": 5,
        });

        Mock::given(method("POST"))
            .and(path("/v1/auth/device"))
            .and(header_exists("content-type"))
            .and(NonEmptyBody)
            .respond_with(ResponseTemplate::new(200).set_body_json(&device_json))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let device = auth_api::initiate_device(&client, &server.uri())
            .await
            .expect("device init should succeed when a body is sent");
        assert_eq!(device.device_code, "dc-123");
        assert_eq!(device.user_code, "ABCD-EFGH");
    }

    /// A single-org poll success is parsed into the persisted token shape.
    #[tokio::test]
    async fn poll_token_single_org_success_persists_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/device/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token_type": "workos",
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "expires_at": 4_000_000_000_i64,
                "org_id": "org_solo",
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        match auth_api::poll_token(&client, &server.uri(), "dc-1").await {
            auth_api::PollOutcome::Success(t) => {
                let auth = t.into_auth_tokens();
                assert_eq!(auth.access_token, "at-1");
                assert_eq!(auth.refresh_token, "rt-1");
                assert_eq!(auth.expires_at, 4_000_000_000_i64);
                assert_eq!(auth.org_id, "org_solo");
            }
            _ => panic!("expected single-org success"),
        }
    }

    /// `authorization_pending` maps to Pending so the loop keeps polling.
    #[tokio::test]
    async fn poll_token_pending_maps_to_pending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/device/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "authorization_pending"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        assert!(matches!(
            auth_api::poll_token(&client, &server.uri(), "dc-1").await,
            auth_api::PollOutcome::Pending
        ));
    }

    /// A multi-org poll carries the pending_token + organizations list.
    #[tokio::test]
    async fn poll_token_multi_org_returns_select_org() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/device/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "organization_selection_required",
                "pending_token": "pt-xyz",
                "organizations": [
                    { "id": "org_a", "name": "Acme", "slug": "acme" },
                    { "id": "org_b", "name": "Beta", "slug": "beta" }
                ]
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        match auth_api::poll_token(&client, &server.uri(), "dc-1").await {
            auth_api::PollOutcome::SelectOrg {
                pending_token,
                organizations,
            } => {
                assert_eq!(pending_token, "pt-xyz");
                assert_eq!(organizations.len(), 2);
                assert_eq!(organizations[1].slug, "beta");
            }
            _ => panic!("expected SelectOrg"),
        }
    }

    /// select-org exchanges the pending token + org id for real tokens.
    #[tokio::test]
    async fn select_org_returns_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/device/select-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token_type": "workos",
                "access_token": "at-b",
                "refresh_token": "rt-b",
                "expires_at": 4_000_000_001_i64,
                "org_id": "org_b",
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let tokens = auth_api::select_org(&client, &server.uri(), "pt-xyz", "org_b")
            .await
            .expect("select-org should succeed");
        assert_eq!(tokens.org_id, "org_b");
        assert_eq!(tokens.access_token, "at-b");
    }

    /// A `403 org_not_member` from select-org maps to the clear membership error
    /// (not a raw status dump), so a multi-org login against a stale org choice
    /// fails legibly.
    #[tokio::test]
    async fn select_org_403_maps_to_membership_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/device/select-org"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "org_not_member"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = auth_api::select_org(&client, &server.uri(), "pt-xyz", "org_b")
            .await
            .expect_err("select-org should fail for a non-member org");
        assert!(
            err.to_string().contains("not a member"),
            "expected a clear membership error, got: {err}"
        );
    }

    /// The persisted `[auth]` table round-trips through Config::load and the
    /// access token becomes the effective server_key bearer.
    #[test]
    #[serial_test::serial]
    fn persisted_auth_tokens_resolve_to_server_key() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let tokens = AuthTokens {
            access_token: "at-persist".into(),
            refresh_token: "rt-persist".into(),
            expires_at: 4_000_000_000,
            org_id: "org_x".into(),
        };
        config::save_auth_tokens_to(&tokens, &path).unwrap();

        let cfg = config::Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some("at-persist"));
        assert_eq!(cfg.auth.as_ref().unwrap().refresh_token, "rt-persist");
    }
}
