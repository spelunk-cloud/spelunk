//! `spelunk logout`: clear stored spelunk.cloud credentials.
//!
//! Bare `spelunk logout` clears **only** the `[auth]` WorkOS token pair
//! written by `spelunk login`: the credential logout exists to undo. It no
//! longer touches self-hosted server keys as a side effect (ADR-071 D3,
//! founder-review correction): a developer recovering from a broken cloud
//! login should not silently lose the server key(s) they use on other
//! projects. Clearing those is an explicit, separate action via `--servers`
//! (all of them) or `--server <url>` (just one).

use anyhow::{Context as _, Result};
use clap::Args;

use spelunk_core::config::{self, server_keys};

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Clear every stored self-hosted server key (the per-origin map and any
    /// legacy entry) and nothing else; the cloud token pair is left intact.
    #[arg(long, conflicts_with = "server")]
    pub servers: bool,

    /// Clear the stored server key for this one server origin and nothing
    /// else; the cloud token pair is left intact.
    #[arg(long)]
    pub server: Option<String>,
}

pub async fn logout(args: LogoutArgs) -> Result<()> {
    let store = config::default_secret_store()?;

    // Each form clears exactly one credential store and leaves the other
    // intact (see the module doc). Only the bare, no-flag form touches the
    // cloud `[auth]` pair; `--server`/`--servers` are server-key-only.
    if args.servers {
        server_keys::clear_all(store.as_ref()).context("clearing the server_keys map")?;
        // Belt-and-braces: also clear the legacy flat entry and any plaintext
        // remnant still in config.toml (config::remove_server_key's job).
        config::remove_server_key().context("clearing the legacy server_key entry")?;
        println!("Cleared all stored server keys.");
    } else if let Some(url) = &args.server {
        let origin = server_keys::clear_origin(url, store.as_ref())
            .context("clearing the stored server key")?;
        println!("Cleared the stored server key for {origin}.");
    } else {
        config::remove_auth_tokens()
            .context("removing [auth] tokens from ~/.config/spelunk/config.toml")?;
        println!("Logged out. Stored spelunk.cloud credentials have been removed.");

        let n = server_keys::count(store.as_ref())?;
        if n > 0 {
            println!(
                "{n} server key(s) are still stored (unaffected by this logout). \
                 Run `spelunk logout --servers` to remove them all, or \
                 `spelunk logout --server <url>` for just one."
            );
        }
    }

    Ok(())
}
