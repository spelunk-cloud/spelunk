//! Credential-format-agnostic secret storage for the CLI.
//!
//! The CLI's bearer credential used to live as plaintext in
//! `~/.config/spelunk/config.toml` (`server_key = "sk-sp-…"`). Any process
//! running as the user could read it, and the common real-world leak is users
//! syncing `~/.config` into a dotfiles git repo or a backup. This module moves
//! the secret into the OS secret store:
//!
//! * macOS  — Keychain
//! * Linux  — Secret Service (libsecret / `org.freedesktop.secrets`)
//! * Windows— Credential Manager
//!
//! all via the [`keyring`] crate.
//!
//! ## Format-agnostic
//!
//! The store holds **opaque string secrets keyed by name** — it does not know
//! or care whether a value is today's `sk-sp-…` bearer key or a future WorkOS
//! access/refresh token. Callers pick the key name; this module
//! just persists and retrieves the bytes. That keeps the storage layer reusable
//! when the credential format changes.
//!
//! ## Headless / CI fallback
//!
//! There is no keychain in CI, containers, or a headless Linux box without a
//! running Secret Service daemon. This module **never hard-fails** in those
//! environments:
//!
//! * `SPELUNK_SERVER_KEY` remains the non-interactive escape hatch and is read
//!   directly by [`crate::config::Config::load`] — it bypasses this store
//!   entirely.
//! * When no keychain backend is available, [`SecretStore::default_store`]
//!   transparently falls back to an **opt-out file store** that keeps the
//!   pre-existing `config.toml` behaviour (a clear, non-fatal degradation rather
//!   than an error). Set `SPELUNK_SECRET_STORE=file` to force the file backend,
//!   or `SPELUNK_SECRET_STORE=keychain` to require the keychain (erroring if it
//!   is unavailable instead of falling back).
//!
//! Secrets are **never logged** — only key names and backend kinds appear in
//! any diagnostic output.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The keyring "service" name under which all spelunk secrets are grouped.
///
/// Shared by every entry so a user can find/audit spelunk credentials in their
/// OS keychain UI under a single service.
pub const KEYRING_SERVICE: &str = "spelunk";

/// Key name for the CLI bearer credential (today's `server_key`).
///
/// A stable, format-agnostic name: whether the value is an `sk-sp-…` key or a
/// future WorkOS token, it is the single `Authorization: Bearer` credential.
pub const KEY_SERVER_KEY: &str = "server_key";

/// Key name for the credential sent to the configured LLM endpoint.
///
/// A single flat entry rather than a per-origin map: there is one LLM
/// endpoint, not a set of them. Read only on the daemon-spawn path, never by
/// [`crate::config::Config::load`].
pub const KEY_LLM_KEY: &str = "llm_key";

/// Environment variable that pins which backend the secret store uses.
///
/// * unset / `auto` — prefer the keychain, fall back to the file store when no
///   keychain backend is available (the default, graceful headless behaviour).
/// * `keychain`     — require the OS keychain; error if it is unavailable.
/// * `file`         — always use the plaintext file store (opt-in, e.g. for a
///   container that mounts `~/.config` from a secret manager).
pub const ENV_SECRET_STORE: &str = "SPELUNK_SECRET_STORE";

/// A pluggable secret backend: opaque string secrets keyed by name.
///
/// Implementations must never log secret values. The trait is object-safe so a
/// resolved backend can be carried as `Box<dyn SecretStore>`.
pub trait SecretStore: Send + Sync {
    /// Fetch the secret stored under `key`, or `None` if absent.
    fn get(&self, key: &str) -> Result<Option<String>>;

    /// Store (or replace) the secret under `key`.
    fn set(&self, key: &str, value: &str) -> Result<()>;

    /// Delete the secret under `key`. A missing entry is **not** an error
    /// (delete is idempotent), so `logout` can clear unconditionally.
    fn delete(&self, key: &str) -> Result<()>;

    /// Human-readable backend name for diagnostics (never includes secrets).
    fn kind(&self) -> &'static str;
}

// ───────────────────────────────────────────────────────────────────────────
// Keyring-backed store (OS keychain)
// ───────────────────────────────────────────────────────────────────────────

/// Process-wide cache of keychain reads, keyed by entry name.
///
/// One CLI invocation can end up calling [`Config::load`](crate::config::Config::load)
/// (or otherwise resolving the secret store) more than once, and each
/// *uncached* keychain read is a separate OS authorization, not a free
/// in-memory lookup: on macOS a per-item ACL that hasn't been granted yet
/// prompts again on every read. Caching here bounds a given entry to at most
/// one real keychain access per process no matter how many call sites ask
/// for it.
static READ_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

fn read_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    READ_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// OS-keychain-backed [`SecretStore`] using the [`keyring`] crate.
///
/// Each secret is a keyring entry `(service = "spelunk", user = <key>)`.
pub struct KeyringStore;

impl KeyringStore {
    /// Build a keyring entry for `key` under the shared spelunk service.
    fn entry(key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, key)
            .with_context(|| format!("opening keychain entry for {KEYRING_SERVICE}/{key}"))
    }

    /// Probe whether a usable keychain backend exists on this host.
    ///
    /// Constructing an `Entry` is cheap and, with the default platform backend,
    /// fails fast when no Secret Service / Keychain is available (e.g. headless
    /// Linux with no daemon). We do not read or write a secret here — only
    /// confirm the backend can be addressed.
    pub fn is_available() -> bool {
        keyring::Entry::new(KEYRING_SERVICE, "__spelunk_probe__").is_ok()
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        if let Some(cached) = read_cache().lock().unwrap().get(key) {
            return Ok(cached.clone());
        }
        let value = match Self::entry(key)?.get_password() {
            Ok(v) => Some(v),
            Err(keyring::Error::NoEntry) => None,
            Err(e) => return Err(e).with_context(|| format!("reading keychain entry {key}")),
        };
        read_cache()
            .lock()
            .unwrap()
            .insert(key.to_string(), value.clone());
        Ok(value)
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        Self::entry(key)?
            .set_password(value)
            .with_context(|| format!("writing keychain entry {key}"))?;
        read_cache()
            .lock()
            .unwrap()
            .insert(key.to_string(), Some(value.to_string()));
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e).with_context(|| format!("deleting keychain entry {key}")),
        }
        read_cache().lock().unwrap().insert(key.to_string(), None);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "keychain"
    }
}

// ───────────────────────────────────────────────────────────────────────────
// File-backed store (headless / CI fallback)
// ───────────────────────────────────────────────────────────────────────────

/// Plaintext-file [`SecretStore`] — the graceful fallback when no keychain is
/// available, and the opt-in target for `SPELUNK_SECRET_STORE=file`.
///
/// Secrets live as top-level TOML keys in `<dir>/secrets.toml`, written `0600`
/// on Unix. This is the same trust level as the pre-existing plaintext
/// `config.toml` behaviour, so falling back here never *reduces* security
/// relative to before — it just no longer commingles the secret with the
/// shareable config (which users sync to dotfiles repos).
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// Build a file store writing to `path` (`<dir>/secrets.toml`).
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read_table(&self) -> Result<toml::Table> {
        if !self.path.exists() {
            return Ok(toml::Table::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        raw.parse::<toml::Table>()
            .with_context(|| format!("parsing {}", self.path.display()))
    }

    fn write_table(&self, table: &toml::Table) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating secret dir {}", parent.display()))?;
        }
        let serialised = toml::to_string_pretty(table).context("serialising secrets.toml")?;
        std::fs::write(&self.path, serialised)
            .with_context(|| format!("writing {}", self.path.display()))?;
        set_owner_only(&self.path)
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .read_table()?
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut table = self.read_table()?;
        table.insert(key.to_string(), toml::Value::String(value.to_string()));
        self.write_table(&table)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let mut table = self.read_table()?;
        if table.remove(key).is_none() {
            return Ok(());
        }
        // If the file is now empty, remove it entirely so we don't leave an
        // empty secrets.toml lying around.
        if table.is_empty() {
            if self.path.exists() {
                std::fs::remove_file(&self.path)
                    .with_context(|| format!("removing empty {}", self.path.display()))?;
            }
            return Ok(());
        }
        self.write_table(&table)
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

/// Set `0600` permissions on Unix; a no-op on other platforms.
#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// In-memory store (tests)
// ───────────────────────────────────────────────────────────────────────────

/// In-memory [`SecretStore`] for tests — no daemon, no filesystem, no keychain.
///
/// Avoids depending on the `keyring` crate's `mock` feature (which sets a
/// process-global backend) so individual tests stay isolated.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryStore {
    inner: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(any(test, feature = "test-support"))]
impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Backend resolution
// ───────────────────────────────────────────────────────────────────────────

/// Resolve the default [`SecretStore`] for this host, honouring
/// [`ENV_SECRET_STORE`].
///
/// * `keychain` — require the OS keychain; error if unavailable.
/// * `file`     — force the plaintext file store at `<config_dir>/secrets.toml`.
/// * unset / `auto` / anything else — prefer the keychain, fall back to the
///   file store when no keychain backend is present (logging at `info`).
///
/// `config_dir` is where the file-store fallback writes `secrets.toml`
/// (typically `~/.config/spelunk`).
pub fn default_store(config_dir: &Path) -> Result<Box<dyn SecretStore>> {
    let file_path = config_dir.join("secrets.toml");
    match std::env::var(ENV_SECRET_STORE).ok().as_deref() {
        Some("file") => {
            tracing::debug!("secret store: file (forced via {ENV_SECRET_STORE}=file)");
            Ok(Box::new(FileStore::new(file_path)))
        }
        Some("keychain") => {
            if KeyringStore::is_available() {
                tracing::debug!("secret store: keychain (forced via {ENV_SECRET_STORE}=keychain)");
                Ok(Box::new(KeyringStore))
            } else {
                anyhow::bail!(
                    "{ENV_SECRET_STORE}=keychain but no OS keychain backend is available \
                     (no Keychain / Secret Service). Unset {ENV_SECRET_STORE} to fall back to \
                     file storage, or set {ENV_SECRET_STORE}=file, or pass the credential via \
                     SPELUNK_SERVER_KEY."
                );
            }
        }
        _ => {
            if KeyringStore::is_available() {
                tracing::debug!("secret store: keychain (auto)");
                Ok(Box::new(KeyringStore))
            } else {
                tracing::info!(
                    "no OS keychain backend available — falling back to file storage at {}. \
                     Set {ENV_SECRET_STORE}=keychain to require the keychain, or pass the \
                     credential via SPELUNK_SERVER_KEY in CI/headless environments.",
                    file_path.display()
                );
                Ok(Box::new(FileStore::new(file_path)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── FileStore round-trip ────────────────────────────────────────────────

    #[test]
    fn file_store_set_get_delete_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path().join("secrets.toml"));

        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
        store.set(KEY_SERVER_KEY, "sk-sp-secret").unwrap();
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-sp-secret")
        );

        // Overwrite.
        store.set(KEY_SERVER_KEY, "sk-sp-rotated").unwrap();
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-sp-rotated")
        );

        store.delete(KEY_SERVER_KEY).unwrap();
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
    }

    #[test]
    fn file_store_delete_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path().join("secrets.toml"));
        // Idempotent delete: no error even when nothing is stored.
        store.delete(KEY_SERVER_KEY).unwrap();
    }

    #[test]
    fn file_store_is_format_agnostic_multiple_keys() {
        // The store holds opaque values under arbitrary names, so a future
        // access/refresh-token migration can reuse it with no changes here.
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path().join("secrets.toml"));
        store.set("access_token", "at-123").unwrap();
        store.set("refresh_token", "rt-456").unwrap();
        assert_eq!(
            store.get("access_token").unwrap().as_deref(),
            Some("at-123")
        );
        assert_eq!(
            store.get("refresh_token").unwrap().as_deref(),
            Some("rt-456")
        );
        // Deleting one leaves the other intact.
        store.delete("access_token").unwrap();
        assert_eq!(store.get("access_token").unwrap(), None);
        assert_eq!(
            store.get("refresh_token").unwrap().as_deref(),
            Some("rt-456")
        );
    }

    #[test]
    fn file_store_removes_file_when_last_key_deleted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secrets.toml");
        let store = FileStore::new(path.clone());
        store.set(KEY_SERVER_KEY, "x").unwrap();
        assert!(path.exists());
        store.delete(KEY_SERVER_KEY).unwrap();
        assert!(!path.exists(), "empty secrets.toml should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secrets.toml");
        let store = FileStore::new(path.clone());
        store.set(KEY_SERVER_KEY, "x").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secret file must be owner-only");
    }

    // ── MemoryStore ─────────────────────────────────────────────────────────

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::default();
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
        store.set(KEY_SERVER_KEY, "tok").unwrap();
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap().as_deref(), Some("tok"));
        store.delete(KEY_SERVER_KEY).unwrap();
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
        // Idempotent delete.
        store.delete(KEY_SERVER_KEY).unwrap();
    }

    // ── default_store backend selection ──────────────────────────────────────

    #[test]
    #[serial_test::serial(spelunk_secret_store_env)]
    fn default_store_file_forced_via_env() {
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var(ENV_SECRET_STORE, "file") };
        let store = default_store(tmp.path()).unwrap();
        assert_eq!(store.kind(), "file");
        unsafe { std::env::remove_var(ENV_SECRET_STORE) };
    }

    #[test]
    #[serial_test::serial(spelunk_secret_store_env)]
    fn default_store_keychain_forced_errors_when_unavailable() {
        // We can't guarantee a keychain in CI, so this only asserts the error
        // path when the keychain is genuinely unavailable. When a keychain IS
        // present (e.g. a dev macOS box), the call succeeds — both outcomes are
        // acceptable, so we just assert it does not fall back to file.
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var(ENV_SECRET_STORE, "keychain") };
        match default_store(tmp.path()) {
            Ok(store) => assert_eq!(
                store.kind(),
                "keychain",
                "forced keychain must never resolve to the file backend"
            ),
            Err(e) => assert!(
                e.to_string().contains("keychain"),
                "error should explain the keychain is unavailable: {e}"
            ),
        }
        unsafe { std::env::remove_var(ENV_SECRET_STORE) };
    }

    #[test]
    #[serial_test::serial(spelunk_secret_store_env)]
    fn default_store_auto_resolves_to_a_backend() {
        // Auto mode must always yield *some* backend (never hard-fail), whether
        // a keychain is present (→ keychain) or not (→ file fallback).
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::remove_var(ENV_SECRET_STORE) };
        let store = default_store(tmp.path()).unwrap();
        assert!(matches!(store.kind(), "keychain" | "file"));
    }

    // ── KeyringStore process-wide read cache ─────────────────────────────────
    //
    // `KeyringStore::get` hardcodes the real `keyring` crate backend with no
    // way to inject a fake, so it cannot be exercised in a test at all without
    // risking a real OS keychain access (and, on macOS, a real authorization
    // dialog) — something this project's test suite must never do. What *is*
    // testable in isolation is the cache primitive `KeyringStore::get`
    // delegates to: `read_cache()`, a process-wide `key -> Option<value>` map
    // that a value is written into once and served from on every subsequent
    // lookup. This test exercises that shared cache directly (the same
    // static `KeyringStore::get` reads and writes) to confirm a key already
    // present is served without a second backend fetch, which is the
    // invariant the fix relies on to bound keychain reads to at most one per
    // process per key.
    #[test]
    #[serial_test::serial(spelunk_keyring_read_cache)]
    fn read_cache_serves_a_cached_key_without_a_second_fetch() {
        let key = "test_only_read_cache_probe_key";
        // Isolate from any other run's leftovers (the map is process-global).
        read_cache().lock().unwrap().remove(key);

        let fetch_count = std::sync::atomic::AtomicUsize::new(0);
        // Mirrors KeyringStore::get's own check-cache-then-fetch-then-insert
        // flow, but with a counted stand-in for the real keychain read.
        let lookup = |k: &str| -> Option<String> {
            if let Some(cached) = read_cache().lock().unwrap().get(k) {
                return cached.clone();
            }
            fetch_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let value = Some(format!("value-for-{k}"));
            read_cache()
                .lock()
                .unwrap()
                .insert(k.to_string(), value.clone());
            value
        };

        assert_eq!(
            lookup(key).as_deref(),
            Some("value-for-test_only_read_cache_probe_key")
        );
        assert_eq!(
            lookup(key).as_deref(),
            Some("value-for-test_only_read_cache_probe_key")
        );
        assert_eq!(
            lookup(key).as_deref(),
            Some("value-for-test_only_read_cache_probe_key")
        );
        assert_eq!(
            fetch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cached key must be served from the cache, not re-fetched, on every \
             lookup after the first"
        );

        read_cache().lock().unwrap().remove(key);
    }

    /// A `None` result (key absent from the backend) is cached too, so a
    /// repeated lookup for a key that does not exist also avoids a second
    /// fetch — not just the `Some` case.
    #[test]
    #[serial_test::serial(spelunk_keyring_read_cache)]
    fn read_cache_caches_a_negative_result_too() {
        let key = "test_only_read_cache_probe_key_absent";
        read_cache().lock().unwrap().remove(key);

        let fetch_count = std::sync::atomic::AtomicUsize::new(0);
        let lookup = |k: &str| -> Option<String> {
            if let Some(cached) = read_cache().lock().unwrap().get(k) {
                return cached.clone();
            }
            fetch_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let value: Option<String> = None;
            read_cache()
                .lock()
                .unwrap()
                .insert(k.to_string(), value.clone());
            value
        };

        assert_eq!(lookup(key), None);
        assert_eq!(lookup(key), None);
        assert_eq!(
            fetch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cached miss (no such entry) must also be served from the cache"
        );

        read_cache().lock().unwrap().remove(key);
    }
}
