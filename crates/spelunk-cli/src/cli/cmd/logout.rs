//! `spelunk logout` — remove the stored `server_key` from the global config.

use anyhow::{Context as _, Result};

use spelunk_core::config;

pub async fn logout() -> Result<()> {
    config::remove_server_key()
        .context("removing server_key from ~/.config/spelunk/config.toml")?;
    println!("Logged out. Your server_key has been removed from ~/.config/spelunk/config.toml");
    Ok(())
}
