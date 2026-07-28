//! HTTP client for WorkOS-direct device-flow auth.
//!
//! Supersedes the cloud-api `/v1/auth/*` proxy. A live multi-org
//! device login proved every token leg the CLI needs is a WorkOS PUBLIC-CLIENT
//! exchange — `client_id` only, no secret — so the CLI talks to WorkOS directly
//! and no longer routes auth through cloud-api.
//!
//! WorkOS endpoints (all under `https://api.workos.com/user_management`):
//!   POST /authorize/device   — start the device-authorization grant
//!   POST /authenticate       — exchange a device code, refresh, or switch org
//!
//! Org selection happens browser-side on WorkOS's hosted approval page, so the
//! CLI never sees `organization_selection_required` and there is no
//! pending-token / select-org step.
//!
//! cloud-api is still used for ONE thing — `GET /v1/me` resolves an org slug to
//! its local org UUID before a switch (see [`fetch_me`]). That call carries the
//! WorkOS access token as a bearer; cloud-api validates it.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use spelunk_core::config::{AuthTokens, Config};

/// Default cloud API base URL (used for `GET /v1/me`, and as the cloud-vs-
/// self-hosted origin boundary for bearer resolution, ADR-071 D2). Single
/// source of truth lives in `spelunk_core::config::server_keys`, which also
/// reads it (and its `SPELUNK_CLOUD_URL` override) when deciding credential
/// kind; re-exported here so every existing `auth_api::DEFAULT_CLOUD_URL`
/// call site keeps working unchanged.
pub use spelunk_core::config::server_keys::DEFAULT_CLOUD_URL;

/// Default WorkOS User Management API base URL.
pub const DEFAULT_WORKOS_URL: &str = "https://api.workos.com";

/// Embedded PUBLIC-CLIENT `client_id` for the **production** WorkOS environment.
pub const WORKOS_CLIENT_ID_PROD: &str = "client_01KTY5G1EK45Y93B0RB9Q1D2Q2";

/// Embedded PUBLIC-CLIENT `client_id` for the **dev / staging** WorkOS environment.
pub const WORKOS_CLIENT_ID_DEV: &str = "client_01KTY5JEJSZQD6R3QBXZS19WVF";

/// RFC 8628 device-code grant type.
const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// OAuth refresh-token grant type (also used for silent org-switch).
const GRANT_REFRESH_TOKEN: &str = "refresh_token";

/// HTTP request timeout for non-polling auth calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared reqwest client used for all auth calls.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")
}

/// Resolve the WorkOS base URL.
///
/// `SPELUNK_WORKOS_URL` overrides the default (used by tests to point at a mock
/// server). Trailing slashes are trimmed.
pub fn workos_url() -> String {
    std::env::var("SPELUNK_WORKOS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WORKOS_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Resolve the embedded WorkOS `client_id` for the active environment.
///
/// Selection mirrors the cloud-url default-vs-override pattern already used for
/// the rest of the CLI's environment config:
///   1. `SPELUNK_WORKOS_CLIENT_ID` — explicit override (tests / bespoke envs).
///   2. Otherwise derived from `cloud_url`: the production cloud host
///      (`api.spelunk.cloud`) selects the prod client_id; anything else (a dev
///      override, localhost, a staging host) selects the dev client_id.
pub fn workos_client_id(cloud_url: &str) -> String {
    if let Ok(v) = std::env::var("SPELUNK_WORKOS_CLIENT_ID")
        && !v.trim().is_empty()
    {
        return v;
    }
    if is_prod_cloud_url(cloud_url) {
        WORKOS_CLIENT_ID_PROD.to_string()
    } else {
        WORKOS_CLIENT_ID_DEV.to_string()
    }
}

/// Whether `cloud_url` targets the production spelunk.cloud API host.
///
/// Only the canonical production host counts as prod; every other host (dev
/// overrides, staging, localhost) falls through to the dev environment.
fn is_prod_cloud_url(cloud_url: &str) -> bool {
    let host = cloud_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split(['/', ':']).next().unwrap_or(host);
    host.eq_ignore_ascii_case("api.spelunk.cloud")
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// `POST /authorize/device` response (WorkOS shape).
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

/// One `orgs[]` entry from `GET /v1/me`.
///
/// `id` is the **local org UUID**; `workos_org_id` is the provider-assigned id
/// (e.g. `org_01KV…`) carried in the access-token JWT. Extra fields (e.g.
/// `role`) are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct MeOrg {
    pub id: String,
    pub name: String,
    pub slug: String,
    /// Provider-assigned org id — matches the `org_id` claim in the access-token JWT.
    #[serde(default)]
    pub workos_org_id: Option<String>,
}

/// `GET /v1/me` response (only the membership list is consumed by the CLI).
#[derive(Debug, Clone, Deserialize)]
pub struct MeResponse {
    #[serde(default)]
    pub orgs: Vec<MeOrg>,
}

/// Raw WorkOS `/authenticate` success body.
///
/// WorkOS returns the rotated `access_token` (a JWT) and `refresh_token`. The
/// expiry and organisation are carried in the access-token JWT claims (`exp`,
/// `org_id`); `organization_id` is also echoed at the top level on org-scoped
/// authentications, which we prefer when present.
#[derive(Debug, Clone, Deserialize)]
struct WorkosAuthResponse {
    access_token: String,
    refresh_token: String,
    /// Echoed for org-scoped sessions; falls back to the JWT `org_id` claim.
    #[serde(default)]
    organization_id: Option<String>,
}

/// A successful token exchange, normalised into the persisted [`AuthTokens`].
///
/// Built from a [`WorkosAuthResponse`] by decoding the access-token JWT for its
/// `exp` (→ `expires_at`) and `org_id` claims.
#[derive(Debug, Clone)]
pub struct TokenSuccess {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub org_id: String,
}

impl TokenSuccess {
    /// Convert a normalised success body into the persisted [`AuthTokens`] shape.
    pub fn into_auth_tokens(self) -> AuthTokens {
        AuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            org_id: self.org_id,
        }
    }
}

impl WorkosAuthResponse {
    /// Normalise the WorkOS body into a [`TokenSuccess`], deriving `expires_at`
    /// and `org_id` from the access-token JWT claims.
    fn into_success(self) -> TokenSuccess {
        let claims = decode_jwt_claims(&self.access_token).unwrap_or_default();
        let expires_at = claims.exp.unwrap_or(0);
        let org_id = self.organization_id.or(claims.org_id).unwrap_or_default();
        TokenSuccess {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
            org_id,
        }
    }
}

/// The subset of JWT claims the CLI reads from a WorkOS access token.
#[derive(Debug, Default, Deserialize)]
struct JwtClaims {
    /// Absolute expiry (Unix seconds).
    exp: Option<i64>,
    /// WorkOS organisation id the token is scoped to.
    org_id: Option<String>,
}

/// Decode the claims (second segment) of a JWT without verifying the signature.
///
/// The CLI does not validate the token — WorkOS issued it over TLS and the
/// server re-validates on every request. We only need the `exp` and `org_id`
/// claims to populate local state, so a base64url-decode of the payload is
/// sufficient. Returns `None` if the token is malformed.
fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload_b64)?;
    serde_json::from_slice(&bytes).ok()
}

/// Decode unpadded base64url (RFC 4648 §5) into bytes. Returns `None` on any
/// invalid character. A small standalone decoder so the CLI needs no base64 dep.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }

    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// WorkOS `/authenticate` 4xx error body (RFC 8628 + WorkOS error codes).
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

// ── Device flow ───────────────────────────────────────────────────────────────

/// `POST /user_management/authorize/device` to start the device grant.
///
/// Sends only `client_id` (public-client init); WorkOS returns the device code,
/// user code, verification URLs, expiry, and poll interval.
pub async fn initiate_device(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
) -> Result<DeviceCodeResponse> {
    let resp = client
        .post(format!("{workos_url}/user_management/authorize/device"))
        .form(&[("client_id", client_id)])
        .send()
        .await
        .context("POST /user_management/authorize/device failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization request failed ({status}): {body}");
    }

    resp.json()
        .await
        .context("parsing device authorization response")
}

/// Outcome of a single poll of `/authenticate` (device-code grant).
pub enum PollOutcome {
    /// Success — tokens are ready to persist.
    Success(TokenSuccess),
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

/// Poll `POST /user_management/authenticate` once with the device-code grant.
pub async fn poll_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    device_code: &str,
) -> PollOutcome {
    let resp = match client
        .post(format!("{workos_url}/user_management/authenticate"))
        .form(&[
            ("client_id", client_id),
            ("grant_type", GRANT_DEVICE_CODE),
            ("device_code", device_code),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("network error")),
    };

    let status = resp.status();

    if status.is_success() {
        return match resp.json::<WorkosAuthResponse>().await {
            Ok(t) => PollOutcome::Success(t.into_success()),
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
        "mfa_required" | "mfa_challenge" | "mfa_enrollment" | "challenge_required" => {
            // WorkOS step-up is completed browser-side; surface it non-fatally.
            PollOutcome::Challenge(None)
        }
        "invalid_grant" => {
            let msg = err
                .error_description
                .unwrap_or_else(|| "invalid_grant".to_string());
            PollOutcome::InvalidGrant(msg)
        }
        other => PollOutcome::Error(anyhow::anyhow!(
            "unexpected error from authenticate endpoint: {other}"
        )),
    }
}

/// `POST /user_management/authenticate` with the refresh-token grant.
///
/// With `organization_id` set this is a silent org-switch; without it, a plain
/// refresh reverts to the account's default org. A non-member / unknown
/// organisation surfaces as a clear error.
pub async fn refresh_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    refresh_token: &str,
    organization_id: Option<&str>,
) -> Result<TokenSuccess> {
    let mut form: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("grant_type", GRANT_REFRESH_TOKEN),
        ("refresh_token", refresh_token),
    ];
    if let Some(org) = organization_id {
        form.push(("organization_id", org));
    }

    let resp = client
        .post(format!("{workos_url}/user_management/authenticate"))
        .form(&form)
        .send()
        .await
        .context("POST /user_management/authenticate (refresh) failed")?;

    token_or_error(resp, "refreshing token").await
}

/// Ensure the access token in `auth` is fresh before a cloud-api call.
///
/// WorkOS access tokens are short-lived (~5 min), so a token read straight from
/// the stored `[auth]` table is frequently already expired by the time the CLI
/// runs — every cloud-api call would then `401`. This guard refreshes proactively
/// when the token is at/past expiry (with a small skew window, see
/// [`AuthTokens::is_expired`]) using the stored refresh token directly against
/// WorkOS (refresh grant), persists the rotated tokens to `[auth]`, and
/// returns the tokens to use.
///
/// The refresh re-sends `auth.org_id` as `organization_id` so a prior `org
/// switch` survives rotation — WorkOS's refresh grant otherwise reverts to the
/// account's default org when `organization_id` is omitted, silently undoing
/// the switch on every expiry (see [`refresh_token`]).
///
/// When the token is still valid, `auth` is returned unchanged (no network
/// call). A refresh failure (revoked/expired refresh token) surfaces a clear
/// "run `spelunk login`" error rather than a raw 401 from the downstream call.
///
/// `persist` is invoked with the rotated tokens so the caller controls *where*
/// they are written (the global config in production, a temp path in tests).
pub async fn ensure_fresh_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    persist: impl FnOnce(&AuthTokens) -> Result<()>,
) -> Result<AuthTokens> {
    if !auth.is_expired() {
        return Ok(auth.clone());
    }

    let rotated = refresh_token(
        client,
        workos_url,
        client_id,
        &auth.refresh_token,
        org_id_for_refresh(&auth.org_id),
    )
    .await
    .map_err(|e| e.context("session expired and token refresh failed — run `spelunk login`"))?
    .into_auth_tokens();
    persist(&rotated)?;
    Ok(rotated)
}

/// The `organization_id` to re-request on a plain refresh: `auth.org_id` when
/// set, `None` when empty (an orgless account, or tokens predating org
/// tracking) so the refresh grant falls back to its default-org behaviour
/// instead of sending an empty `organization_id` form field.
pub(crate) fn org_id_for_refresh(org_id: &str) -> Option<&str> {
    (!org_id.is_empty()).then_some(org_id)
}

/// Resolve the bearer token to send to `server_url`, refreshing the stored
/// WorkOS access token first if it has expired.
///
/// Resolution goes through [`Config::bearer_for`] (ADR-071 D2): only a
/// cloud-origin `server_url` can ever resolve to the `[auth]` access token,
/// so a self-hosted `server_url` never mistakes an unrelated cloud login for
/// its own credential. Because access tokens are short-lived, commands that
/// hit cloud-api directly (e.g. `spelunk sync`, `spelunk memory pull`) must
/// guard against using an already-expired token, which would 401.
///
/// Behaviour:
///   - When `[auth]` tokens are present AND the resolved bearer was derived
///     from them (i.e. `server_url` is the cloud origin) AND they are
///     expired, refresh directly against WorkOS, persist the rotated tokens,
///     and return the fresh access token.
///   - Otherwise return the resolved bearer unchanged: a self-hosted
///     server-key or an env override is not refreshable here, and a
///     still-valid token needs no network round-trip.
///
/// A refresh failure surfaces a clear "run `spelunk login`" error.
pub async fn ensure_fresh_server_key(cfg: &Config, server_url: &str) -> Result<Option<String>> {
    let resolved = cfg.bearer_for(server_url)?;

    // Only the WorkOS-login bearer (the cloud kind, resolved from
    // [auth].access_token) is refreshable; a self-hosted server-key is
    // returned as-is.
    let Some(auth) = cfg
        .auth
        .as_ref()
        .filter(|a| Some(a.access_token.as_str()) == resolved.as_deref())
    else {
        return Ok(resolved);
    };

    if !auth.is_expired() {
        return Ok(resolved);
    }

    let client = build_client()?;
    let client_id = workos_client_id(DEFAULT_CLOUD_URL);
    let fresh = ensure_fresh_token(
        &client,
        &workos_url(),
        &client_id,
        auth,
        spelunk_core::config::save_auth_tokens,
    )
    .await?;
    Ok(Some(fresh.access_token))
}

/// `GET /v1/me` (cloud-api) — fetch the caller's identity and org memberships.
///
/// Authenticated with the stored WorkOS access token as a bearer. Used to
/// resolve an org slug to its local org UUID before a silent switch.
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

/// Best-effort display name for the org identified by `workos_org_id`.
///
/// Calls `GET /v1/me` with a short (5 s) timeout independent of the caller's
/// client, finds the entry whose `workos_org_id` matches, and returns
/// `"<name> (<slug>)"`. Returns `None` on any error (network, timeout, missing
/// field, org not found) so the caller can fall back to the raw org id without
/// failing.
pub async fn lookup_org_display_name(
    cloud_url: &str,
    access_token: &str,
    workos_org_id: &str,
) -> Option<String> {
    // Build a client with a tight timeout so a slow /v1/me never delays login.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let me = fetch_me(&client, cloud_url, access_token).await.ok()?;
    me.orgs
        .into_iter()
        .find(|o| o.workos_org_id.as_deref() == Some(workos_org_id))
        .map(|o| format!("{} ({})", o.name, o.slug))
}

/// Parse a `TokenSuccess` from a 2xx WorkOS response, mapping known error bodies
/// (notably a non-member organisation) to readable errors.
async fn token_or_error(resp: reqwest::Response, ctx: &str) -> Result<TokenSuccess> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<WorkosAuthResponse>()
            .await
            .map(WorkosAuthResponse::into_success)
            .with_context(|| format!("parsing token response while {ctx}"));
    }

    let body = resp.text().await.unwrap_or_default();
    if let Ok(err) = serde_json::from_str::<ErrorResponse>(&body) {
        if err.error == "organization_not_found" || err.error == "org_not_member" {
            anyhow::bail!("You are not a member of the requested organization.");
        }
        anyhow::bail!("{ctx} failed ({status}): {}", err.error);
    }
    anyhow::bail!("{ctx} failed ({status}): {body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal unsigned JWT with the given claims payload (header and
    /// signature are placeholders — the CLI never verifies them).
    fn fake_jwt(claims: &serde_json::Value) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(ALPHABET[(n & 0x3f) as usize] as char);
                }
            }
            out
        }
        let header = b64url(br#"{"alg":"none"}"#);
        let payload = b64url(serde_json::to_string(claims).unwrap().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn base64url_decode_round_trips_jwt_payload() {
        let jwt = fake_jwt(&serde_json::json!({ "exp": 123, "org_id": "org_abc" }));
        let claims = decode_jwt_claims(&jwt).expect("claims decode");
        assert_eq!(claims.exp, Some(123));
        assert_eq!(claims.org_id.as_deref(), Some("org_abc"));
    }

    #[test]
    fn decode_jwt_claims_malformed_returns_none() {
        assert!(decode_jwt_claims("not-a-jwt").is_none());
        assert!(decode_jwt_claims("a.!!!.c").is_none());
    }

    #[test]
    fn into_success_prefers_top_level_org_then_falls_back_to_claim() {
        // Top-level organization_id wins.
        let resp = WorkosAuthResponse {
            access_token: fake_jwt(&serde_json::json!({ "exp": 999, "org_id": "org_claim" })),
            refresh_token: "rt".into(),
            organization_id: Some("org_top".into()),
        };
        let s = resp.into_success();
        assert_eq!(s.org_id, "org_top");
        assert_eq!(s.expires_at, 999);

        // Falls back to the JWT claim when absent.
        let resp = WorkosAuthResponse {
            access_token: fake_jwt(&serde_json::json!({ "exp": 1000, "org_id": "org_claim" })),
            refresh_token: "rt".into(),
            organization_id: None,
        };
        let s = resp.into_success();
        assert_eq!(s.org_id, "org_claim");
        assert_eq!(s.expires_at, 1000);
    }

    #[test]
    fn workos_client_id_prod_for_canonical_host() {
        // No override env set in this case path.
        let prev = std::env::var("SPELUNK_WORKOS_CLIENT_ID").ok();
        unsafe { std::env::remove_var("SPELUNK_WORKOS_CLIENT_ID") };
        assert_eq!(
            workos_client_id("https://api.spelunk.cloud"),
            WORKOS_CLIENT_ID_PROD
        );
        assert_eq!(
            workos_client_id("https://dev.spelunk.cloud"),
            WORKOS_CLIENT_ID_DEV
        );
        assert_eq!(
            workos_client_id("http://localhost:8080"),
            WORKOS_CLIENT_ID_DEV
        );
        if let Some(v) = prev {
            unsafe { std::env::set_var("SPELUNK_WORKOS_CLIENT_ID", v) };
        }
    }

    /// Guard the embedded prod `client_id` against an accidental revert to a
    /// stale WorkOS environment (prod rotated on 2026-06-30).
    #[test]
    fn prod_client_id_is_current_rotated_value() {
        assert_eq!(
            WORKOS_CLIENT_ID_PROD, "client_01KTY5G1EK45Y93B0RB9Q1D2Q2",
            "prod client_id must track the rotated WorkOS prod environment"
        );
    }

    #[test]
    fn is_prod_cloud_url_only_matches_canonical_host() {
        assert!(is_prod_cloud_url("https://api.spelunk.cloud"));
        assert!(is_prod_cloud_url("https://api.spelunk.cloud/"));
        assert!(is_prod_cloud_url("https://API.SPELUNK.CLOUD"));
        assert!(!is_prod_cloud_url("https://staging.spelunk.cloud"));
        assert!(!is_prod_cloud_url("http://127.0.0.1:8080"));
    }

    // ── ensure_fresh_token (expiry guard) ────────────────────────────────────────

    /// A still-valid access token is returned unchanged with NO network call and
    /// NO persist call (the persist closure would panic if invoked).
    #[tokio::test]
    async fn ensure_fresh_token_noop_when_valid() {
        let auth = AuthTokens {
            access_token: "at-valid".into(),
            refresh_token: "rt".into(),
            // Far in the future ⇒ not expired.
            expires_at: 5_000_000_000,
            org_id: "org_1".into(),
        };
        let client = build_client().unwrap();
        // workos_url points nowhere reachable; it must never be hit.
        let out = ensure_fresh_token(&client, "http://127.0.0.1:1", "client_test", &auth, |_| {
            panic!("persist must not be called when the token is still valid");
        })
        .await
        .expect("a valid token needs no refresh");
        assert_eq!(out, auth);
    }

    /// An expired access token is refreshed via WorkOS, the rotated tokens are
    /// handed to `persist`, and the fresh tokens are returned.
    #[tokio::test]
    async fn ensure_fresh_token_refreshes_and_persists_when_expired() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh_jwt =
            fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_1" }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": fresh_jwt,
                "refresh_token": "rt-rotated",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0, // definitely past expiry
            org_id: "org_1".into(),
        };

        let persisted: Arc<Mutex<Option<AuthTokens>>> = Arc::new(Mutex::new(None));
        let sink = persisted.clone();
        let client = build_client().unwrap();
        let out = ensure_fresh_token(&client, &server.uri(), "client_test", &expired, |t| {
            *sink.lock().unwrap() = Some(t.clone());
            Ok(())
        })
        .await
        .expect("an expired token should refresh");

        assert_eq!(out.refresh_token, "rt-rotated");
        assert_eq!(out.access_token, fresh_jwt);
        assert_eq!(
            persisted.lock().unwrap().as_ref().unwrap().refresh_token,
            "rt-rotated",
            "rotated tokens must be persisted"
        );
    }

    /// Regression test for the org-switch-doesn't-stick bug: a refresh of an
    /// expired token scoped to a switched-to org (`auth.org_id`) must re-send
    /// that same org as `organization_id`, so the session stays scoped to it
    /// instead of silently reverting to the account's default org.
    #[tokio::test]
    async fn ensure_fresh_token_preserves_switched_org_across_refresh() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh_jwt =
            fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_switched" }));
        // The mock only answers a request whose form body names the switched
        // org; a plain refresh (no organization_id) would 404 here instead.
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_switched"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": fresh_jwt,
                "refresh_token": "rt-rotated",
                "organization_id": "org_switched",
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Simulates state right after `org switch org_switched`: the access
        // token has since expired, but auth.org_id still names the switched org.
        let expired_after_switch = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: "org_switched".into(),
        };

        let client = build_client().unwrap();
        let out = ensure_fresh_token(
            &client,
            &server.uri(),
            "client_test",
            &expired_after_switch,
            |_| Ok(()),
        )
        .await
        .expect("refresh scoped to the switched org should succeed");

        assert_eq!(
            out.org_id, "org_switched",
            "the rotated token must stay scoped to the switched org, not revert to the default"
        );
    }

    /// An account with no active org (`org_id` empty — e.g. tokens from before
    /// `org switch` was ever used) falls back to a plain refresh: no
    /// `organization_id` field is sent at all, matching prior behaviour.
    #[tokio::test]
    async fn ensure_fresh_token_empty_org_id_sends_plain_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        struct AssertNoOrgId;
        impl Respond for AssertNoOrgId {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    !body.contains("organization_id"),
                    "an empty org_id must not send organization_id at all, got body: {body}"
                );
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64 })),
                    "refresh_token": "rt-rotated",
                }))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(AssertNoOrgId)
            .expect(1)
            .mount(&server)
            .await;

        let expired_no_org = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: String::new(),
        };

        let client = build_client().unwrap();
        ensure_fresh_token(
            &client,
            &server.uri(),
            "client_test",
            &expired_no_org,
            |_| Ok(()),
        )
        .await
        .expect("plain refresh with no org should still succeed");
    }

    /// When the refresh grant is rejected (revoked/expired refresh token), the
    /// error carries a clear "run `spelunk login`" hint instead of a raw 401.
    #[tokio::test]
    async fn ensure_fresh_token_refresh_failure_says_run_login() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-revoked".into(),
            expires_at: 0,
            org_id: "org_1".into(),
        };
        let client = build_client().unwrap();
        let err = ensure_fresh_token(&client, &server.uri(), "client_test", &expired, |_| Ok(()))
            .await
            .expect_err("a revoked refresh token must error");
        assert!(
            err.to_string().contains("spelunk login"),
            "error should tell the user to re-login, got: {err}"
        );
    }

    // ── Wire-level tests for initiate_device ─────────────────────────────────────

    /// `initiate_device` sends a form-encoded `client_id` and parses the
    /// WorkOS device-code response shape correctly.
    #[tokio::test]
    async fn initiate_device_sends_client_id_and_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        struct AssertClientId;
        impl Respond for AssertClientId {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    body.contains("client_id=client_test"),
                    "initiate_device must send client_id in form body, got: {body}"
                );
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_code": "dc-abc",
                    "user_code": "USER-CODE",
                    "verification_uri": "https://workos.example.com/activate",
                    "verification_uri_complete": "https://workos.example.com/activate?code=USER-CODE",
                    "expires_in": 900,
                    "interval": 5
                }))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authorize/device"))
            .respond_with(AssertClientId)
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let resp = initiate_device(&client, &server.uri(), "client_test")
            .await
            .expect("initiate_device should succeed");

        assert_eq!(resp.device_code, "dc-abc");
        assert_eq!(resp.user_code, "USER-CODE");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://workos.example.com/activate?code=USER-CODE")
        );
    }

    /// A non-2xx from `initiate_device` is surfaced as a clear error (not a
    /// panic or an opaque parse failure).
    #[tokio::test]
    async fn initiate_device_error_response_surfaces_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authorize/device"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad client"))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let err = initiate_device(&client, &server.uri(), "bad_client_id")
            .await
            .expect_err("a 400 from WorkOS must surface as an error");
        assert!(
            err.to_string().contains("400"),
            "error should mention the HTTP status, got: {err}"
        );
    }

    // ── Wire-level tests for poll_token ────────────────────────────────────────

    /// A WorkOS MFA/step-up response (`mfa_required`) must map to
    /// `PollOutcome::Challenge` — it is non-fatal so the polling loop can
    /// continue and the operator completes the challenge in the browser.
    #[tokio::test]
    async fn poll_token_mfa_required_is_non_fatal_challenge() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "mfa_required"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        assert!(
            matches!(outcome, PollOutcome::Challenge(_)),
            "mfa_required must yield PollOutcome::Challenge (non-fatal)"
        );
    }

    /// All four WorkOS step-up/MFA error codes must map to `PollOutcome::Challenge`
    /// so the operator can complete the challenge in the browser rather than having
    /// the CLI exit with an error.
    #[tokio::test]
    async fn poll_token_all_mfa_codes_are_non_fatal() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for code in &["mfa_challenge", "mfa_enrollment", "challenge_required"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": code
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = build_client().unwrap();
            let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
            assert!(
                matches!(outcome, PollOutcome::Challenge(_)),
                "error code '{code}' must yield PollOutcome::Challenge (non-fatal)"
            );
        }
    }

    /// `authorization_pending` must yield `PollOutcome::Pending` so the loop
    /// keeps polling without surfacing an error.
    #[tokio::test]
    async fn poll_token_authorization_pending_is_pending() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "authorization_pending"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        assert!(
            matches!(outcome, PollOutcome::Pending),
            "authorization_pending must yield PollOutcome::Pending"
        );
    }

    /// A successful device-code exchange returns `PollOutcome::Success` with the
    /// correct tokens, decoding `exp` and `org_id` from the JWT payload.
    #[tokio::test]
    async fn poll_token_success_decodes_jwt_claims() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let jwt = fake_jwt(&serde_json::json!({ "exp": 4_000_000_000_i64, "org_id": "org_abc" }));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": jwt,
                "refresh_token": "rt-device",
                "organization_id": "org_abc",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        let PollOutcome::Success(tok) = outcome else {
            panic!("expected Success, got non-Success outcome");
        };
        assert_eq!(tok.refresh_token, "rt-device");
        assert_eq!(tok.org_id, "org_abc");
        assert_eq!(tok.expires_at, 4_000_000_000_i64);
    }

    // ── --org login-then-switch integration ───────────────────────────────────
    //
    // The `--org` flow in `login.rs` is:
    //   1. Device-code login yields initial tokens (org chosen browser-side).
    //   2. `switch_org` is called with those initial tokens + the `--org` slug.
    //   3. `switch_org` calls GET /v1/me (slug → local UUID) then WorkOS refresh
    //      with `organization_id` (the local UUID).
    //
    // The full `login()` function sleeps (device polling) and calls
    // `process::exit`, so we test the critical switch leg directly through the
    // public `switch_org` helper that `login` calls, verifying the end-to-end
    // wire contract of the login-then-switch path.

    /// After a device login yields an initial token for org A, passing `--org
    /// <beta-slug>` re-scopes via `switch_org`: GET /v1/me resolves the slug to
    /// its WorkOS org id and POST /authenticate (refresh grant + organization_id)
    /// returns tokens scoped to the target org.
    #[tokio::test]
    async fn login_then_switch_org_resolves_slug_and_refreshes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BETA_WORKOS: &str = "org_beta01";

        let server = MockServer::start().await;

        // GET /v1/me — returns two orgs; beta is the one the user wants to switch to.
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    { "id": "11111111-1111-1111-1111-111111111111",
                      "name": "Acme", "slug": "acme",
                      "workos_org_id": "org_acme01", "role": "admin" },
                    { "id": BETA_UUID, "name": "Beta Corp", "slug": "beta",
                      "workos_org_id": BETA_WORKOS, "role": "member" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        // POST /authenticate (refresh grant) — must carry the resolved WorkOS org id.
        let beta_jwt = fake_jwt(&serde_json::json!({
            "exp": 5_000_000_000_i64,
            "org_id": BETA_WORKOS
        }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": beta_jwt,
                "refresh_token": "rt-beta",
                "organization_id": BETA_WORKOS,
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Simulate the initial tokens that a device login would have yielded
        // (scoped to acme, the org picked browser-side).
        let initial_tokens = spelunk_core::config::AuthTokens {
            access_token: "at-initial-acme".into(),
            refresh_token: "rt-initial-acme".into(),
            expires_at: 5_000_000_000,
            org_id: "11111111-1111-1111-1111-111111111111".into(),
        };

        let client = build_client().unwrap();
        let switched = switch_org(
            &client,
            &server.uri(), // workos_url
            &server.uri(), // cloud_url (for /v1/me)
            "client_test",
            &initial_tokens,
            "beta",
        )
        .await
        .expect("login-then-switch should succeed");

        // Rotated session org_id is the WorkOS org id echoed by /authenticate.
        assert_eq!(switched.org_id, BETA_WORKOS);
        assert_eq!(switched.refresh_token, "rt-beta");
        assert_eq!(switched.expires_at, 5_000_000_000_i64);
    }

    /// When `--org` targets an org the user is NOT a member of, WorkOS returns
    /// `organization_not_found`, which must surface as a clear membership error
    /// — NOT a panic or an opaque HTTP error. This guards the login-then-switch
    /// path specifically (the slug resolves via /v1/me but WorkOS rejects the
    /// refresh grant).
    #[tokio::test]
    async fn login_then_switch_org_not_member_surfaces_clear_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    { "id": BETA_UUID, "name": "Beta Corp", "slug": "beta",
                      "workos_org_id": "org_beta01", "role": "member" }
                ]
            })))
            .mount(&server)
            .await;

        // WorkOS rejects the refresh because the user is not actually a member
        // (e.g. membership in the local DB is stale).
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "organization_not_found"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let initial_tokens = spelunk_core::config::AuthTokens {
            access_token: "at-initial".into(),
            refresh_token: "rt-initial".into(),
            expires_at: 5_000_000_000,
            org_id: "11111111-1111-1111-1111-111111111111".into(),
        };

        let client = build_client().unwrap();
        let err = switch_org(
            &client,
            &server.uri(),
            &server.uri(),
            "client_test",
            &initial_tokens,
            "beta",
        )
        .await
        .expect_err("non-member org should error");

        assert!(
            err.to_string().contains("not a member"),
            "expected a clear membership error, got: {err}"
        );
    }

    // ── lookup_org_display_name ───────────────────────────────────────────────

    /// When `/v1/me` returns an org whose `workos_org_id` matches the token's
    /// org id, `lookup_org_display_name` resolves it to `"<name> (<slug>)"`.
    #[tokio::test]
    async fn lookup_org_display_name_resolves_name_and_slug() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    {
                        "id": "11111111-1111-1111-1111-111111111111",
                        "name": "Acme Corp",
                        "slug": "acme",
                        "workos_org_id": "org_01KVGBR276C2PN9MCH6WZ5HJ1Y",
                        "role": "admin"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert_eq!(
            display.as_deref(),
            Some("Acme Corp (acme)"),
            "expected resolved name+slug, got: {display:?}"
        );
    }

    /// When the `workos_org_id` field is absent from all `orgs[]` entries,
    /// `lookup_org_display_name` returns `None` (best-effort fallback).
    #[tokio::test]
    async fn lookup_org_display_name_returns_none_when_workos_org_id_missing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    // No workos_org_id field — older server or different shape.
                    { "id": "11111111-1111-1111-1111-111111111111",
                      "name": "Acme Corp", "slug": "acme", "role": "admin" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert!(
            display.is_none(),
            "expected None when workos_org_id not found, got: {display:?}"
        );
    }

    /// When `/v1/me` returns a non-2xx, `lookup_org_display_name` returns `None`
    /// instead of propagating an error (best-effort, never makes login fail).
    #[tokio::test]
    async fn lookup_org_display_name_returns_none_on_server_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert!(
            display.is_none(),
            "a /v1/me error must not propagate — expected None, got: {display:?}"
        );
    }

    /// When there are multiple orgs, the correct one is matched by `workos_org_id`.
    #[tokio::test]
    async fn lookup_org_display_name_matches_correct_org() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    {
                        "id": "11111111-1111-1111-1111-111111111111",
                        "name": "Acme Corp",
                        "slug": "acme",
                        "workos_org_id": "org_AAAA",
                        "role": "admin"
                    },
                    {
                        "id": "22222222-2222-2222-2222-222222222222",
                        "name": "Beta Inc",
                        "slug": "beta",
                        "workos_org_id": "org_BBBB",
                        "role": "member"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Token is scoped to the second org.
        let display = lookup_org_display_name(&server.uri(), "at-test", "org_BBBB").await;
        assert_eq!(
            display.as_deref(),
            Some("Beta Inc (beta)"),
            "should resolve the org matching the workos_org_id, got: {display:?}"
        );
    }
}
