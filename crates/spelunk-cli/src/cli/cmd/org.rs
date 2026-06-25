//! `spelunk org switch <org>` — silently re-scope the session to another
//! organisation without a new device login (ADR-045).
//!
//! Uses the stored refresh token against `POST /v1/auth/token` with
//! `organization_id`, then persists the rotated tokens. Shared org-resolution
//! and token-persistence helpers also back `spelunk login --org`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, Organization};

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
    /// Organization to switch to. Accepts a WorkOS organization id
    /// (`org_...`); cloud-api resolves it against your memberships.
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
/// `org` is sent as `organization_id` to `POST /v1/auth/token`; cloud-api
/// rotates the tokens scoped to that organisation (or returns `org_not_member`).
pub async fn switch_org(
    client: &reqwest::Client,
    cloud_url: &str,
    auth: &AuthTokens,
    org: &str,
) -> Result<AuthTokens> {
    let success =
        auth_api::refresh_token(client, cloud_url, &auth.refresh_token, Some(org)).await?;
    Ok(success.into_auth_tokens())
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

    /// `--org` also accepts a raw WorkOS org id.
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
}
