//! `spelunk auth set-key` / `spelunk auth list-servers`: manage the
//! per-server bearer credentials a self-hosted `server_url` resolves through
//! (ADR-071 D1/D3).
//!
//! These are the credential's front door: the key is read from stdin or an
//! interactive prompt, never from argv (a positional or flag-valued secret
//! lands in shell history and `ps` output). `set-key` stores it in the
//! per-origin map (`spelunk_core::config::server_keys`); `list-servers`
//! prints only origins, never key material.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::io::{IsTerminal, Write};

use spelunk_core::config::server_keys;

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Store a bearer key for a self-hosted spelunk-server, read from stdin/prompt
    SetKey(AuthSetKeyArgs),
    /// List servers with a stored key (origins only, never prints key material)
    ListServers,
}

#[derive(Args, Debug)]
pub struct AuthSetKeyArgs {
    /// Server URL this key belongs to (normalized to its origin before storage)
    #[arg(long)]
    pub server: String,
}

pub async fn auth(args: AuthArgs) -> Result<()> {
    match args.command {
        AuthCommand::SetKey(set_key_args) => set_key(&set_key_args.server),
        AuthCommand::ListServers => list_servers(),
    }
}

fn set_key(server: &str) -> Result<()> {
    let key = read_secret_from_stdin_or_prompt()?;
    let store = spelunk_core::config::default_secret_store()?;
    let origin = server_keys::set_key_for_origin(server, &key, store.as_ref())
        .context("storing the server key")?;
    println!("Stored a server key for {origin}.");
    Ok(())
}

fn list_servers() -> Result<()> {
    let store = spelunk_core::config::default_secret_store()?;
    let (origins, legacy) = server_keys::list_origins(store.as_ref())?;
    if origins.is_empty() && !legacy {
        println!("No server keys stored.");
        return Ok(());
    }
    for origin in &origins {
        println!("{origin}");
    }
    if legacy {
        println!(
            "(a legacy server key is also stored; it migrates automatically the next \
             time it resolves for a server)"
        );
    }
    Ok(())
}

/// Read a secret from stdin: piped input if present, else an interactive
/// prompt. Never accepted via a CLI flag/argv (D3).
fn read_secret_from_stdin_or_prompt() -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("Server key: ");
        std::io::stderr().flush().ok();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading server key from stdin")?;
    let key = line.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no server key provided (empty input)");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::config::secret_store::MemoryStore;

    #[test]
    fn set_key_then_list_servers_round_trip_via_store() {
        let store = MemoryStore::default();
        let origin =
            server_keys::set_key_for_origin("https://team.example:7777/ignored", "sk-1", &store)
                .unwrap();
        assert_eq!(origin, "https://team.example:7777");

        let (origins, legacy) = server_keys::list_origins(&store).unwrap();
        assert_eq!(origins, vec!["https://team.example:7777".to_string()]);
        assert!(!legacy);
    }
}
