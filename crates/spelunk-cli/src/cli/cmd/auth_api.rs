//! HTTP client for the cloud-api WorkOS-proxied auth endpoints (ADR-045).
//!
//! cloud-api holds the WorkOS secret; the CLI only ever talks to cloud-api.
//! This module owns the request/response shapes for the device-authorization
//! grant and the token/refresh/org-switch exchanges, plus the polling loop used
//! by `spelunk login`. Persisting the resulting tokens is the caller's job.
//!
//! Endpoints:
//!   POST /v1/auth/device              — start the device flow
//!   POST /v1/auth/device/token        — poll for the token (RFC 8628)
//!   POST /v1/auth/device/select-org   — finish a multi-org device login
//!   POST /v1/auth/token               — rotate / silently switch org
//!   GET  /v1/me                       — current identity + org memberships

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use spelunk_core::config::AuthTokens;

/// Default cloud API base URL.
pub const DEFAULT_CLOUD_URL: &str = "https://api.spelunk.cloud";

/// HTTP request timeout for non-polling auth calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared reqwest client used for all auth calls.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// `POST /v1/auth/device` response.
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    /// Opaque handle — never parsed by the CLI.
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

/// A WorkOS organisation the operator can select from.
///
/// `id` is the **local org UUID** (not the WorkOS `org_...` id); it is the value
/// to pass as `organization_id` / `org_id` to the auth endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub slug: String,
}

/// One `orgs[]` entry from `GET /v1/me`.
///
/// `id` is the **local org UUID**. Extra fields (e.g. `role`) are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct MeOrg {
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    pub slug: String,
}

/// `GET /v1/me` response (only the membership list is consumed by the CLI).
#[derive(Debug, Clone, Deserialize)]
pub struct MeResponse {
    #[serde(default)]
    pub orgs: Vec<MeOrg>,
}

/// Successful token body shared by `/device/token`, `/device/select-org`, and
/// `/auth/token`.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenSuccess {
    #[allow(dead_code)]
    pub token_type: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub org_id: String,
}

impl TokenSuccess {
    /// Convert a wire success body into the persisted [`AuthTokens`] shape.
    pub fn into_auth_tokens(self) -> AuthTokens {
        AuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            org_id: self.org_id,
        }
    }
}

/// RFC 8628 error body returned by a pending / failed poll.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
    /// MFA / step-up challenge URL (surfaced non-fatally when present).
    #[serde(default)]
    challenge_url: Option<String>,
    // Multi-org selection fields (present when error == organization_selection_required).
    #[serde(default)]
    pending_token: Option<String>,
    #[serde(default)]
    organizations: Vec<Organization>,
}

// ── Device flow ───────────────────────────────────────────────────────────────

/// `POST /v1/auth/device` to start the device-authorization grant.
///
/// A non-empty JSON body is always sent so the fronting proxy does not reject
/// the request with `411 Length Required` (see GH #434).
pub async fn initiate_device(
    client: &reqwest::Client,
    cloud_url: &str,
) -> Result<DeviceCodeResponse> {
    let resp = client
        .post(format!("{cloud_url}/v1/auth/device"))
        .json(&device_init_body())
        .send()
        .await
        .context("POST /v1/auth/device failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization request failed ({status}): {body}");
    }

    resp.json()
        .await
        .context("parsing device authorization response")
}

/// Build the JSON body for `POST /v1/auth/device`.
///
/// The machine hostname is sent as `client_hint` when it resolves; otherwise an
/// empty object is sent (still a non-empty payload so the proxy accepts it).
pub fn device_init_body() -> serde_json::Value {
    match gethostname::gethostname().into_string() {
        Ok(host) if !host.trim().is_empty() => serde_json::json!({ "client_hint": host }),
        _ => serde_json::json!({}),
    }
}

/// Outcome of a single poll of `/v1/auth/device/token`.
pub enum PollOutcome {
    /// Single-org success — tokens are ready to persist.
    Success(TokenSuccess),
    /// Multi-org: the operator must pick an organisation to continue.
    SelectOrg {
        pending_token: String,
        organizations: Vec<Organization>,
    },
    /// User has not yet approved.
    Pending,
    /// Server requests slower polling (RFC 8628 §3.5).
    SlowDown,
    /// HTTP 429 — back off.
    RateLimit,
    /// Device code expired.
    Expired,
    /// User explicitly denied.
    Denied,
    /// invalid_grant (e.g. code already used or revoked).
    InvalidGrant(String),
    /// A WorkOS MFA / step-up challenge — non-fatal; complete in the browser.
    Challenge(Option<String>),
    /// Transient network / parse error.
    Error(anyhow::Error),
}

/// `POST /v1/auth/device/token` once with the opaque `device_code`.
pub async fn poll_token(
    client: &reqwest::Client,
    cloud_url: &str,
    device_code: &str,
) -> PollOutcome {
    let body = serde_json::json!({ "device_code": device_code });

    let resp = match client
        .post(format!("{cloud_url}/v1/auth/device/token"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("network error")),
    };

    let status = resp.status();

    if status.is_success() {
        return match resp.json::<TokenSuccess>().await {
            Ok(t) => PollOutcome::Success(t),
            Err(e) => PollOutcome::Error(anyhow::anyhow!(e).context("parsing token response")),
        };
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PollOutcome::RateLimit;
    }

    let err: ErrorResponse = match resp.json().await {
        Ok(e) => e,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("parsing error response")),
    };

    match err.error.as_str() {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "expired_token" => PollOutcome::Expired,
        "access_denied" => PollOutcome::Denied,
        "organization_selection_required" => match err.pending_token {
            Some(pending_token) => PollOutcome::SelectOrg {
                pending_token,
                organizations: err.organizations,
            },
            None => PollOutcome::Error(anyhow::anyhow!(
                "organization_selection_required without a pending_token"
            )),
        },
        "mfa_required" | "mfa_challenge" | "challenge_required" => {
            PollOutcome::Challenge(err.challenge_url)
        }
        "invalid_grant" => {
            let msg = err
                .error_description
                .unwrap_or_else(|| "invalid_grant".to_string());
            PollOutcome::InvalidGrant(msg)
        }
        other => PollOutcome::Error(anyhow::anyhow!(
            "unexpected error from token endpoint: {other}"
        )),
    }
}

/// `POST /v1/auth/device/select-org` to finish a multi-org device login.
///
/// Returns the issued tokens, or a clear error on `403 org_not_member`.
pub async fn select_org(
    client: &reqwest::Client,
    cloud_url: &str,
    pending_token: &str,
    org_id: &str,
) -> Result<TokenSuccess> {
    let body = serde_json::json!({ "pending_token": pending_token, "org_id": org_id });
    let resp = client
        .post(format!("{cloud_url}/v1/auth/device/select-org"))
        .json(&body)
        .send()
        .await
        .context("POST /v1/auth/device/select-org failed")?;

    token_or_error(resp, "selecting organisation").await
}

/// `POST /v1/auth/token` to rotate tokens.
///
/// With `organization_id` set this is a silent org-switch; without it, a plain
/// refresh. A `403 org_not_member` is surfaced as a clear error.
pub async fn refresh_token(
    client: &reqwest::Client,
    cloud_url: &str,
    refresh_token: &str,
    organization_id: Option<&str>,
) -> Result<TokenSuccess> {
    let mut body = serde_json::json!({ "refresh_token": refresh_token });
    if let Some(org) = organization_id {
        body["organization_id"] = serde_json::Value::String(org.to_string());
    }
    let resp = client
        .post(format!("{cloud_url}/v1/auth/token"))
        .json(&body)
        .send()
        .await
        .context("POST /v1/auth/token failed")?;

    token_or_error(resp, "refreshing token").await
}

/// `GET /v1/me` — fetch the caller's identity and org memberships.
///
/// Authenticated with the stored access token as a bearer. Used to resolve an
/// org slug to its local org UUID before a silent switch.
pub async fn fetch_me(
    client: &reqwest::Client,
    cloud_url: &str,
    access_token: &str,
) -> Result<MeResponse> {
    let resp = client
        .get(format!("{cloud_url}/v1/me"))
        .bearer_auth(access_token)
        .send()
        .await
        .context("GET /v1/me failed")?;

    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<MeResponse>()
            .await
            .context("parsing /v1/me response");
    }

    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("GET /v1/me failed ({status}): {body}");
}

/// Parse a `TokenSuccess` from a 2xx response, mapping known error bodies
/// (notably `org_not_member`) to readable errors.
async fn token_or_error(resp: reqwest::Response, ctx: &str) -> Result<TokenSuccess> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<TokenSuccess>()
            .await
            .with_context(|| format!("parsing token response while {ctx}"));
    }

    let body = resp.text().await.unwrap_or_default();
    if let Ok(err) = serde_json::from_str::<ErrorResponse>(&body) {
        if err.error == "org_not_member" {
            anyhow::bail!("You are not a member of the requested organization.");
        }
        anyhow::bail!("{ctx} failed ({status}): {}", err.error);
    }
    anyhow::bail!("{ctx} failed ({status}): {body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_init_body_is_non_empty_object() {
        let body = device_init_body();
        assert!(body.is_object());
        assert!(serde_json::to_string(&body).unwrap().len() >= 2);
    }

    #[test]
    fn token_success_into_auth_tokens_maps_fields() {
        let t = TokenSuccess {
            token_type: Some("workos".into()),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1234,
            org_id: "org_1".into(),
        };
        let auth = t.into_auth_tokens();
        assert_eq!(auth.access_token, "at");
        assert_eq!(auth.refresh_token, "rt");
        assert_eq!(auth.expires_at, 1234);
        assert_eq!(auth.org_id, "org_1");
    }
}
