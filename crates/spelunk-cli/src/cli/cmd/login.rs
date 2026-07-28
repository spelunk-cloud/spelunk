//! `spelunk login` — WorkOS device-authorization grant, direct.
//!
//! Flow
//! ----
//! 1. POST WorkOS `/authorize/device` (client_id only) → device_code, user_code,
//!    verification_uri.
//! 2. Print the verification URL and user code for the operator.
//! 3. Poll POST WorkOS `/authenticate` (device-code grant) every `interval`
//!    seconds (RFC 8628):
//!    - success                         → persist tokens, done
//!    - authorization_pending           → keep polling
//!    - slow_down                       → increase interval by 5 s
//!    - expired_token / access_denied   → exit 1
//!    - MFA / step-up challenge         → print "complete in browser", keep polling
//!
//! Org selection happens browser-side on WorkOS's hosted approval page, so the
//! CLI never sees an org-selection step and the returned token is already
//! org-scoped.
//!
//! `--org <slug>` is login-then-switch: a device login always yields a token
//! first; if `--org` is given, the session is then silently re-scoped to that
//! org via the refresh grant. When the operator is already logged in with a
//! valid refresh token and passes `--org`, login short-circuits straight to the
//! silent org-switch (no device re-entry).
//!
//! No-org token, no `--org` (first-run UX)
//! ---------------------------------------
//! WorkOS does not auto-select an org even for single-org users, so a plain
//! `spelunk login` yields a token with an empty `org_id`. Rather than leave the
//! operator with a session that can't do anything until they run
//! `spelunk org switch`, the `None` arm resolves an org itself:
//!   - one org   → auto-select it silently;
//!   - many orgs → an interactive selector on a TTY (require `--org` otherwise);
//!   - zero orgs → a clear onboarding message, and no dangling session written.

use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, MeOrg, PollOutcome};
use super::org::{persist_tokens, switch_org};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud).
    /// Also selects the WorkOS environment (prod host → prod client_id; any
    /// other host → dev client_id) unless `SPELUNK_WORKOS_CLIENT_ID` is set.
    #[arg(long, env = "SPELUNK_CLOUD_URL")]
    pub cloud_url: Option<String>,

    /// Organization to log into (slug). After the device login yields a token,
    /// the session is silently re-scoped to this org; when already logged in it
    /// re-scopes without a new device login.
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
    let workos_url = auth_api::workos_url();
    let client_id = auth_api::workos_client_id(&cloud_url);

    let client = auth_api::build_client()?;

    // Already logged in with a valid refresh token + `--org`: silent re-scope.
    if let Some(org_slug) = &args.org {
        let cfg = config::Config::load(None).context("loading config")?;
        if let Some(auth) = cfg.auth.as_ref() {
            let tokens =
                switch_org(&client, &workos_url, &cloud_url, &client_id, auth, org_slug).await?;
            return finish_login(&cloud_url, tokens, Some(org_slug)).await;
        }
    }

    // ── Step 1: initiate device authorization ─────────────────────────────────
    let device = auth_api::initiate_device(&client, &workos_url, &client_id).await?;

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

    let tokens = loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match auth_api::poll_token(&client, &workos_url, &client_id, &device.device_code).await {
            PollOutcome::Success(token) => break token.into_auth_tokens(),
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
    };

    // A device login always yields a token first; honour `--org` as a
    // login-then-switch by re-scoping the freshly-issued session.
    match &args.org {
        Some(org_slug) => {
            let switched = switch_org(
                &client,
                &workos_url,
                &cloud_url,
                &client_id,
                &tokens,
                org_slug,
            )
            .await?;
            finish_login(&cloud_url, switched, Some(org_slug.as_str())).await
        }
        // No `--org`: if WorkOS already scoped the token to an org (browser-side
        // pick), keep it. Otherwise resolve one ourselves so the first run does
        // not leave a session that needs a follow-up `org switch`.
        None if !tokens.org_id.is_empty() => finish_login(&cloud_url, tokens, None).await,
        None => resolve_org_after_login(&client, &workos_url, &cloud_url, &client_id, tokens).await,
    }
}

/// Resolve an org for a freshly-issued **no-org** token (no `--org` given).
///
/// Fetches the caller's memberships via `GET /v1/me` and:
/// - **1 org** → silently `switch_org` to it (auto-select);
/// - **N orgs** → interactive selector on a TTY; a non-TTY/agent shell errors
///   with an actionable "pass `--org`" message and a non-zero exit;
/// - **0 orgs** → a clear onboarding message and a non-zero exit, leaving no
///   dangling no-org session persisted.
async fn resolve_org_after_login(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    tokens: AuthTokens,
) -> Result<()> {
    let me = auth_api::fetch_me(client, cloud_url, &tokens.access_token)
        .await
        .context("fetching your organizations after login")?;

    let interactive = std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && !spelunk_core::utils::is_agent_mode();

    match choose_org(&me.orgs, interactive)? {
        OrgChoice::Switch(org) => {
            // Prefer the WorkOS org id (skips a redundant /v1/me in switch_org);
            // fall back to the slug when the membership lacks a workos_org_id.
            let target = org
                .workos_org_id
                .clone()
                .unwrap_or_else(|| org.slug.clone());
            let switched =
                switch_org(client, workos_url, cloud_url, client_id, &tokens, &target).await?;
            // We already know the human name; pass the slug as the display hint.
            finish_login(cloud_url, switched, Some(&org.slug)).await
        }
    }
}

/// The outcome of resolving an org for a no-org login.
#[derive(Debug)]
enum OrgChoice {
    /// Switch the session to this membership.
    Switch(MeOrg),
}

/// Decide which org a no-org login should scope to, given the caller's
/// memberships and whether we may prompt interactively.
///
/// Pure and side-effect-free apart from the interactive prompt (which only runs
/// when `interactive` is true), so every branch is unit-testable:
/// - 0 orgs → onboarding error;
/// - 1 org → auto-select;
/// - N + TTY → prompt;
/// - N + non-TTY → actionable "pass `--org`" error.
fn choose_org(orgs: &[MeOrg], interactive: bool) -> Result<OrgChoice> {
    match orgs.len() {
        0 => anyhow::bail!(
            "Your account is not a member of any organization yet.\n\
             Create one at https://spelunk.cloud/onboarding, then run `spelunk login` again."
        ),
        1 => Ok(OrgChoice::Switch(orgs[0].clone())),
        _ if interactive => {
            let idx = prompt_org_selection(orgs)?;
            Ok(OrgChoice::Switch(orgs[idx].clone()))
        }
        _ => {
            let slugs = orgs
                .iter()
                .map(|o| o.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "You are a member of multiple organizations; a non-interactive shell \
                 cannot prompt.\n\
                 Re-run with `spelunk login --org <slug>` (one of: {slugs})."
            )
        }
    }
}

/// Prompt the operator to pick one of `orgs` (guaranteed len >= 2) on a TTY.
///
/// Lists each org as `name (slug)` and reads a 1-based index from stdin,
/// re-prompting on invalid input. Returns the chosen 0-based index.
fn prompt_org_selection(orgs: &[MeOrg]) -> Result<usize> {
    eprintln!();
    eprintln!("You are a member of multiple organizations. Select one:");
    for (i, o) in orgs.iter().enumerate() {
        eprintln!("  {}. {} ({})", i + 1, o.name, o.slug);
    }
    eprintln!();

    let stdin = std::io::stdin();
    loop {
        eprint!("Enter a number [1-{}]: ", orgs.len());
        std::io::stderr().flush().ok();

        let mut line = String::new();
        let n = stdin
            .read_line(&mut line)
            .context("reading org selection")?;
        if n == 0 {
            // EOF (e.g. stdin closed mid-prompt) — don't loop forever.
            anyhow::bail!("no organization selected (input closed)");
        }
        match line.trim().parse::<usize>() {
            Ok(choice) if (1..=orgs.len()).contains(&choice) => return Ok(choice - 1),
            _ => eprintln!("Please enter a number between 1 and {}.", orgs.len()),
        }
    }
}

/// Persist tokens and print the success message naming the org entered.
///
/// Attempts a best-effort `GET /v1/me` lookup to resolve the WorkOS org id to
/// a human-readable `"<name> (<slug>)"` string. The lookup is never fatal: any
/// error, timeout, or missing entry falls back to the `--org` slug hint (when
/// provided) or the raw `org_id` from the token.
async fn finish_login(
    cloud_url: &str,
    tokens: AuthTokens,
    org_slug_hint: Option<&str>,
) -> Result<()> {
    // Write before printing so a write error surfaces before the user believes
    // they are logged in.
    persist_tokens(&tokens)?;
    println!();
    // Best-effort: resolve the WorkOS org id to a display name.
    let display =
        auth_api::lookup_org_display_name(cloud_url, &tokens.access_token, &tokens.org_id).await;
    // Fall back chain: resolved name → slug hint → raw org_id.
    let label = display
        .as_deref()
        .or(org_slug_hint)
        .unwrap_or(&tokens.org_id);
    print_logged_in(label);
    Ok(())
}

/// Print the "logged in to <org>" confirmation with a switch hint.
fn print_logged_in(org: &str) {
    println!("Logged in to {org}. Use `spelunk org switch` to change.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(id: &str, name: &str, slug: &str, workos: Option<&str>) -> MeOrg {
        MeOrg {
            id: id.into(),
            name: name.into(),
            slug: slug.into(),
            workos_org_id: workos.map(str::to_string),
        }
    }

    fn one_org() -> Vec<MeOrg> {
        vec![org(
            "11111111-1111-1111-1111-111111111111",
            "Acme",
            "acme",
            Some("org_acme"),
        )]
    }

    fn two_orgs() -> Vec<MeOrg> {
        vec![
            org(
                "11111111-1111-1111-1111-111111111111",
                "Acme",
                "acme",
                Some("org_acme"),
            ),
            org(
                "22222222-2222-2222-2222-222222222222",
                "Beta Corp",
                "beta",
                Some("org_beta"),
            ),
        ]
    }

    /// Exactly one org → auto-select it (no prompt), even when non-interactive.
    #[test]
    fn choose_org_single_org_auto_selects() {
        let OrgChoice::Switch(picked) = choose_org(&one_org(), false).unwrap();
        assert_eq!(picked.slug, "acme");
        // Interactivity is irrelevant for a single org — same result on a TTY.
        let OrgChoice::Switch(picked) = choose_org(&one_org(), true).unwrap();
        assert_eq!(picked.slug, "acme");
    }

    /// Zero orgs → a clear onboarding error, never a dangling session.
    #[test]
    fn choose_org_zero_orgs_points_at_onboarding() {
        let err = choose_org(&[], true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("onboarding"),
            "0-org error should point at onboarding, got: {msg}"
        );
        assert!(
            msg.contains("not a member of any organization"),
            "0-org error should explain the cause, got: {msg}"
        );
    }

    /// Multiple orgs on a non-interactive shell → actionable `--org` error naming
    /// the available slugs, with no prompt (and thus a non-zero exit upstream).
    #[test]
    fn choose_org_multi_non_interactive_requires_org_flag() {
        let err = choose_org(&two_orgs(), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--org"),
            "multi/non-TTY error should tell the user to pass --org, got: {msg}"
        );
        assert!(
            msg.contains("acme") && msg.contains("beta"),
            "error should list the candidate slugs, got: {msg}"
        );
    }

    /// The scripted path is `--org`, resolved by `switch_org`; `choose_org` is only
    /// reached when `--org` is absent, so a multi-org non-interactive caller must
    /// be told to use it rather than hang on a prompt.
    #[test]
    fn choose_org_multi_non_interactive_does_not_prompt() {
        // `interactive = false` must return Err synchronously (no stdin read),
        // proving the non-TTY guard prevents a blocking prompt.
        assert!(choose_org(&two_orgs(), false).is_err());
    }
}
