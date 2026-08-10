//! Client-side resolution of the credential sent to the configured LLM
//! endpoint.
//!
//! Deliberately not a field on [`Config`](super::Config) and never resolved by
//! `Config::load`: only the daemon-spawn path needs this value, and every
//! other command would otherwise pay a secret-store read (on macOS, a
//! keychain authorization) for a value it never uses.
//!
//! The detached `spelunk-server` must never touch the keychain itself, so the
//! CLI resolves the credential here, in the user's own session, and hands it
//! to the child out of band.

use anyhow::Result;

use super::secret_store::{KEY_LLM_KEY, SecretStore};

/// Environment variable holding the LLM credential, for CI and containers
/// that have no keychain.
pub const ENV_LLM_KEY: &str = "SPELUNK_LLM_KEY";

/// Environment variable overriding [`Config::llm_url`](super::Config::llm_url).
///
/// Named here rather than spelled inline so the loader that reads it and the
/// spawn path that pins it on the child cannot drift apart.
pub const ENV_LLM_URL: &str = "SPELUNK_LLM_URL";

/// Environment variable overriding [`Config::llm_model`](super::Config::llm_model).
pub const ENV_LLM_MODEL: &str = "SPELUNK_LLM_MODEL";

/// Trim `raw` and treat a blank result as "no key".
///
/// A set-but-empty value is what `${SPELUNK_LLM_KEY:-}` expands to in a
/// docker-compose file with the variable unset; that must read as
/// unauthenticated rather than as a broken empty-string credential.
fn normalize(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Resolve the LLM credential: `SPELUNK_LLM_KEY` first, then `store`.
///
/// Takes the store by reference so tests can inject one and never reach the
/// host keychain.
pub fn resolve_with_store(store: &dyn SecretStore) -> Result<Option<String>> {
    if let Some(k) = normalize(std::env::var(ENV_LLM_KEY).ok()) {
        return Ok(Some(k));
    }
    Ok(normalize(store.get(KEY_LLM_KEY)?))
}

/// Store `key` as the LLM credential, rejecting a blank value.
pub fn set_with_store(key: &str, store: &dyn SecretStore) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        anyhow::bail!("refusing to store a blank LLM key");
    }
    store.set(KEY_LLM_KEY, trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret_store::MemoryStore;

    fn clear_env() {
        unsafe { std::env::remove_var(ENV_LLM_KEY) };
    }

    #[test]
    #[serial_test::serial]
    fn resolves_the_stored_key_when_env_is_unset() {
        clear_env();
        let store = MemoryStore::default();
        set_with_store("sk-from-store", &store).unwrap();

        assert_eq!(
            resolve_with_store(&store).unwrap().as_deref(),
            Some("sk-from-store")
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_wins_over_a_different_stored_key() {
        clear_env();
        let store = MemoryStore::default();
        set_with_store("sk-from-store", &store).unwrap();

        unsafe { std::env::set_var(ENV_LLM_KEY, "sk-from-env") };
        let resolved = resolve_with_store(&store);
        clear_env();

        assert_eq!(resolved.unwrap().as_deref(), Some("sk-from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn blank_env_falls_through_to_the_stored_key() {
        clear_env();
        let store = MemoryStore::default();
        set_with_store("sk-from-store", &store).unwrap();

        unsafe { std::env::set_var(ENV_LLM_KEY, "   ") };
        let resolved = resolve_with_store(&store);
        clear_env();

        assert_eq!(resolved.unwrap().as_deref(), Some("sk-from-store"));
    }

    #[test]
    #[serial_test::serial]
    fn resolves_to_none_with_neither_env_nor_store() {
        clear_env();
        let store = MemoryStore::default();

        assert_eq!(resolve_with_store(&store).unwrap(), None);
    }

    #[test]
    #[serial_test::serial]
    fn set_rejects_a_blank_key() {
        clear_env();
        let store = MemoryStore::default();

        assert!(set_with_store("   ", &store).is_err());
        assert_eq!(resolve_with_store(&store).unwrap(), None);
    }
}
