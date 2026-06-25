//! `spelunk logout` — clear stored spelunk.cloud credentials.
//!
//! Removes both the WorkOS `[auth]` tokens written by `spelunk login` and the
//! legacy bare `server_key`, so a logout fully de-authenticates regardless of
//! which credential the user last logged in with.

use anyhow::{Context as _, Result};

use spelunk_core::config;

pub async fn logout() -> Result<()> {
    config::remove_auth_tokens()
        .context("removing [auth] tokens from ~/.config/spelunk/config.toml")?;
    config::remove_server_key()
        .context("removing server_key from ~/.config/spelunk/config.toml")?;
    println!("Logged out. Stored credentials have been removed from ~/.config/spelunk/config.toml");
    Ok(())
}
