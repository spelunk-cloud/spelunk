use anyhow::{Context, Result};
use std::path::Path;

use super::AuthTokens;
use super::paths::spelunk_config_dir;
use super::secret_store::{self, KEY_SERVER_KEY, SecretStore};

/// Persist a bearer into the **legacy flat** secret-store entry
/// (`KEY_SERVER_KEY`, superseded by the per-origin map in
/// [`super::server_keys`], ADR-071 D1).
///
/// `spelunk login` does NOT call this: it writes the `[auth]` table via
/// [`save_auth_tokens`], and `cfg.server_key` derives from that at load time.
/// The only production caller left is [`super::Config::load_with_store`]'s
/// plaintext-file migration (moving a bare `server_key` out of
/// `config.toml`); `spelunk auth set-key` writes the per-origin map instead
/// (see [`super::server_keys::set_key_for_origin`]), never this flat entry. It
/// is stored in the OS keychain (macOS Keychain / Linux Secret Service /
/// Windows Credential Manager) by default, falling back to an owner-only file
/// when no keychain backend is available (CI / headless). The credential is
/// **never** written to `config.toml`.
pub fn save_server_key(key: &str) -> Result<()> {
    let store = secret_store::default_store(&spelunk_config_dir())?;
    save_server_key_with(key, store.as_ref())
}

/// Same as [`save_server_key`] but with an injected [`SecretStore`] (tests).
pub fn save_server_key_with(key: &str, store: &dyn SecretStore) -> Result<()> {
    store
        .set(KEY_SERVER_KEY, key)
        .context("persisting server_key to the secret store")
}

/// Remove the CLI bearer credential everywhere it might live.
///
/// What `spelunk logout --servers` clears (ADR-071 D3): the secret-store
/// entry **and** any legacy plaintext `server_key` left in
/// `~/.config/spelunk/config.toml`. Bare `spelunk logout` no longer calls
/// this; it only clears the `[auth]` cloud token pair (see
/// [`remove_auth_tokens`]). No-op for any location where the credential is
/// absent.
pub fn remove_server_key() -> Result<()> {
    let store = secret_store::default_store(&spelunk_config_dir())?;
    remove_server_key_with(store.as_ref(), &spelunk_config_dir().join("config.toml"))
}

/// Same as [`remove_server_key`] but with an injected [`SecretStore`] and an
/// explicit legacy config path (tests).
pub fn remove_server_key_with(store: &dyn SecretStore, config_path: &Path) -> Result<()> {
    store
        .delete(KEY_SERVER_KEY)
        .context("removing server_key from the secret store")?;
    // Belt-and-braces: also strip any legacy plaintext key still in the file.
    remove_server_key_from(config_path)
}

/// Same as [`remove_server_key`] but operates on an explicit path (useful in tests).
pub fn remove_server_key_from(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;

    let updated: String = existing
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Keep every line that is NOT a `server_key = …` assignment.
            // strip_prefix avoids a raw byte-index slice and handles the
            // "server_key" prefix unambiguously.
            let after_key = trimmed.strip_prefix("server_key");
            !matches!(after_key, Some(rest) if rest.trim_start().starts_with('='))
        })
        .map(|line| format!("{line}\n"))
        .collect();

    std::fs::write(config_path, updated)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// `[auth]` table persistence (WorkOS device-flow tokens)
// ───────────────────────────────────────────────────────────────────────────

/// Persist WorkOS tokens to the `[auth]` table of `~/.config/spelunk/config.toml`.
///
/// Replaces any existing `[auth]` table; all other top-level keys and tables
/// are preserved. The file is written with `0600` permissions so the refresh
/// token is not world-readable.
pub fn save_auth_tokens(tokens: &AuthTokens) -> Result<()> {
    save_auth_tokens_to(tokens, &spelunk_config_dir().join("config.toml"))
}

/// Same as [`save_auth_tokens`] but writes to an explicit path (useful in tests).
pub fn save_auth_tokens_to(tokens: &AuthTokens, config_path: &Path) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }

    let mut doc = read_config_table(config_path)?;
    let auth_value = toml::Value::try_from(tokens).context("serialising auth tokens")?;
    doc.insert("auth".to_string(), auth_value);

    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    write_config_secure(config_path, &serialised)
}

/// Remove the `[auth]` table from `~/.config/spelunk/config.toml`.
///
/// What bare `spelunk logout` clears (ADR-071 D3): only the `[auth]` cloud
/// token pair. It no longer touches self-hosted server keys as a side
/// effect; clearing those requires the explicit `--servers` or
/// `--server <url>` flag (see [`remove_server_key`]). No-op if the file or
/// the table is absent. Other keys are preserved.
pub fn remove_auth_tokens() -> Result<()> {
    remove_auth_tokens_from(&spelunk_config_dir().join("config.toml"))
}

/// Same as [`remove_auth_tokens`] but operates on an explicit path (tests).
pub fn remove_auth_tokens_from(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let mut doc = read_config_table(config_path)?;
    if doc.remove("auth").is_none() {
        return Ok(());
    }
    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    write_config_secure(config_path, &serialised)
}

/// Write `slug` as `project_id` to a project-level `.spelunk/config.toml`,
/// creating the file (and parent dir) if absent. Other keys are preserved.
///
/// Returns `(effective_slug, wrote)`: if the file already sets `project_id`,
/// it is left untouched — `wrote` is `false` and the existing value is returned
/// (no retroactive rename). This is a committed, shared file, so it is written
/// with normal permissions (unlike the secret-bearing personal config).
pub fn write_project_slug(config_path: &Path, slug: &str) -> Result<(String, bool)> {
    let mut doc = read_config_table(config_path)?;
    if let Some(existing) = doc.get("project_id").and_then(|v| v.as_str()) {
        return Ok((existing.to_string(), false));
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    doc.insert(
        "project_id".to_string(),
        toml::Value::String(slug.to_string()),
    );
    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    std::fs::write(config_path, serialised)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok((slug.to_string(), true))
}

/// Parse the config file into a `toml::Table`, returning an empty table when the
/// file does not exist.
fn read_config_table(config_path: &Path) -> Result<toml::Table> {
    if !config_path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    raw.parse::<toml::Table>()
        .with_context(|| format!("parsing {}", config_path.display()))
}

/// Write `contents` to `config_path` and tighten permissions to `0600` on Unix
/// so secrets in the file are owner-only.
fn write_config_secure(config_path: &Path, contents: &str) -> Result<()> {
    std::fs::write(config_path, contents)
        .with_context(|| format!("writing {}", config_path.display()))?;
    set_owner_only_permissions(config_path)?;
    Ok(())
}

/// Set `0600` permissions on Unix; a no-op on other platforms.
#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_project_slug_creates_file_and_reports_written() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join(".spelunk").join("config.toml");

        let (slug, wrote) = write_project_slug(&cfg, "github.com/acme/app").unwrap();
        assert_eq!(slug, "github.com/acme/app");
        assert!(wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            raw.contains("project_id = \"github.com/acme/app\""),
            "{raw}"
        );
    }

    #[test]
    fn write_project_slug_preserves_existing_and_does_not_rename() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "server_url = \"http://team:7777\"\nproject_id = \"old/slug\"\n",
        )
        .unwrap();

        let (slug, wrote) = write_project_slug(&cfg, "local/deadbeef").unwrap();
        assert_eq!(slug, "old/slug");
        assert!(!wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(raw.contains("project_id = \"old/slug\""), "{raw}");
        assert!(raw.contains("server_url"), "other keys preserved: {raw}");
    }

    #[test]
    fn write_project_slug_adds_key_preserving_other_keys() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(&cfg, "server_url = \"http://team:7777\"\n").unwrap();

        let (_, wrote) = write_project_slug(&cfg, "github.com/acme/app").unwrap();
        assert!(wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            raw.contains("project_id = \"github.com/acme/app\""),
            "{raw}"
        );
        assert!(raw.contains("server_url"), "existing key preserved: {raw}");
    }

    /// `remove_auth_tokens_from` is a no-op when the file is missing.
    #[test]
    fn remove_auth_tokens_no_op_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        remove_auth_tokens_from(&path).unwrap();
    }
}
