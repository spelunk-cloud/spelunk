//! `spelunk login` — OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! Flow
//! ----
//! 1. POST /v1/auth/device → `{ device_code, user_code, verification_uri,
//!    verification_uri_complete?, expires_in, interval }`
//! 2. Print the verification URL and user code for the operator.
//! 3. Poll POST /v1/auth/device/token every `interval` seconds until:
//!    - 200 OK  →  parse { api_key }  →  save as `server_key`  →  print "Login successful."
//!    - 400 authorization_pending  →  keep polling (show progress dot)
//!    - 400 slow_down              →  increase interval by 5 s (RFC 8628 §3.5)
//!    - 400 expired_token          →  exit 1 with timeout message
//!    - 400 access_denied          →  exit 1
//!    - 400 invalid_grant          →  exit 1 with error body
//!    - 429                        →  double interval, retry
//!    - network/parse error        →  retry up to 3 times then exit 1

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;

use spelunk_core::config;

// ── Default cloud API base URL ────────────────────────────────────────────────

const DEFAULT_CLOUD_URL: &str = "https://api.spelunk.cloud";

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud)
    #[arg(long, env = "SPELUNK_CLOUD_URL")]
    pub cloud_url: Option<String>,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    api_key: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/');

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    // ── Step 1: initiate device authorization ─────────────────────────────────
    let device = initiate_device(&client, cloud_url).await?;

    // ── Step 2: prompt the user ───────────────────────────────────────────────
    println!();
    println!("Open the following URL in your browser:");
    println!();
    println!("  {}", device.verification_uri);
    println!();
    println!("Enter the code: {}", device.user_code);
    println!();

    // If the server provides a one-click URL, print it as a convenience.
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

    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        let result = poll_token(&client, cloud_url, &device.device_code).await;

        match result {
            PollOutcome::Success(token) => {
                // Write to config before printing the success message so that a
                // write error surfaces before the user thinks they are logged in.
                config::save_server_key(&token)
                    .context("saving server_key to ~/.config/spelunk/config.toml")?;
                println!("\nLogin successful.");
                return Ok(());
            }
            PollOutcome::Pending => {
                // Print a progress dot without a newline so the user sees activity.
                print!(".");
                let _ = std::io::stdout().flush();
                consecutive_errors = 0;
            }
            PollOutcome::SlowDown => {
                // RFC 8628 §3.5: increase interval by 5 s on slow_down.
                interval_secs += 5;
                consecutive_errors = 0;
            }
            PollOutcome::RateLimit => {
                // 429: double the interval as a back-off.
                interval_secs *= 2;
                consecutive_errors = 0;
            }
            PollOutcome::Expired => {
                eprintln!("\nLogin timed out. Run `spelunk login` again.");
                eprintln!(
                    "Hint: your account may not be part of an organization yet — \
                     contact your admin."
                );
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

// ── Step 1: device authorization initiation ───────────────────────────────────

/// `POST /v1/auth/device` to start the device-authorization grant.
///
/// The request **must** carry a JSON body so reqwest sets `Content-Length` +
/// `Content-Type`: a bodyless POST is rejected with `411 Length Required` by
/// Google Front End (Cloud Run's fronting proxy) before it ever reaches
/// cloud-api. The server treats the body — and `client_hint` within it — as
/// optional, but uses the hint to label the approval page and name the issued
/// key, so we send the machine hostname when we can resolve it.
async fn initiate_device(client: &reqwest::Client, cloud_url: &str) -> Result<DeviceCodeResponse> {
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

/// Build the JSON body for `POST /v1/auth/device` (see [`initiate_device`]).
///
/// When the machine hostname resolves it is passed as `client_hint`; otherwise
/// an empty object is sent, which the server accepts. Either way a non-empty
/// body is produced so the fronting proxy does not reject the request with
/// `411`.
fn device_init_body() -> serde_json::Value {
    match gethostname::gethostname().into_string() {
        Ok(host) if !host.trim().is_empty() => serde_json::json!({ "client_hint": host }),
        _ => serde_json::json!({}),
    }
}

// ── Poll outcome ──────────────────────────────────────────────────────────────

enum PollOutcome {
    /// Token issued — contains the api_key.
    Success(String),
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
    /// Transient network / parse error.
    Error(anyhow::Error),
}

async fn poll_token(client: &reqwest::Client, cloud_url: &str, device_code: &str) -> PollOutcome {
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
        match resp.json::<TokenResponse>().await {
            Ok(t) => return PollOutcome::Success(t.api_key),
            Err(e) => {
                return PollOutcome::Error(anyhow::anyhow!(e).context("parsing token response"));
            }
        }
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PollOutcome::RateLimit;
    }

    // Parse the error body.
    let err: ErrorResponse = match resp.json().await {
        Ok(e) => e,
        Err(e) => {
            return PollOutcome::Error(anyhow::anyhow!(e).context("parsing error response"));
        }
    };

    match err.error.as_str() {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "expired_token" => PollOutcome::Expired,
        "access_denied" => PollOutcome::Denied,
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    use spelunk_core::config;

    /// Matches a request whose body is non-empty (i.e. carries Content-Length).
    struct NonEmptyBody;
    impl Match for NonEmptyBody {
        fn matches(&self, request: &Request) -> bool {
            !request.body.is_empty()
        }
    }

    /// The device-init body is always a JSON object, so the request carries a
    /// Content-Length — guarding against the `411 Length Required` regression
    /// (GH #434) where a bodyless POST was rejected by Google Front End.
    #[test]
    fn device_init_body_is_non_empty_object() {
        let body = super::device_init_body();
        assert!(body.is_object(), "body must be a JSON object");
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            serialized.len() >= 2,
            "body must serialise to a non-empty payload, got {serialized:?}"
        );
    }

    /// `initiate_device` sends a non-empty JSON body with a Content-Type so the
    /// fronting proxy accepts the request. If the request were bodyless (the
    /// #434 bug) the mock would not match and this call would fail.
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
        let device = super::initiate_device(&client, &server.uri())
            .await
            .expect("device init should succeed when a body is sent");

        assert_eq!(device.device_code, "dc-123");
        assert_eq!(device.user_code, "ABCD-EFGH");
    }

    /// save_server_key_to creates the file and writes the key.
    #[test]
    fn save_server_key_creates_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        config::save_server_key_to("sk-sp-test", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("server_key"), "should contain server_key");
        assert!(
            contents.contains("sk-sp-test"),
            "should contain the key value"
        );
    }

    /// save_server_key_to preserves existing keys.
    #[test]
    fn save_server_key_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_url = \"http://localhost:7777\"\n").unwrap();
        config::save_server_key_to("sk-sp-test", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("server_url"),
            "should still contain server_url"
        );
        assert!(
            contents.contains("sk-sp-test"),
            "should contain the new key"
        );
    }

    /// save_server_key_to replaces an existing server_key entry.
    #[test]
    fn save_server_key_replaces_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-sp-old\"\n").unwrap();
        config::save_server_key_to("sk-sp-new", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("sk-sp-old"), "old key should be gone");
        assert!(contents.contains("sk-sp-new"), "new key should be present");
    }

    /// remove_server_key_from removes the server_key line.
    #[test]
    fn remove_server_key_removes_line() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://localhost:7777\"\nserver_key = \"sk-sp-test\"\n",
        )
        .unwrap();
        config::remove_server_key_from(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("server_key"),
            "server_key should be removed"
        );
        assert!(
            contents.contains("server_url"),
            "server_url should still be present"
        );
    }

    /// remove_server_key_from is a no-op when the file does not exist.
    #[test]
    fn remove_server_key_no_op_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // Must not error even when file is absent.
        config::remove_server_key_from(&path).unwrap();
    }
}
