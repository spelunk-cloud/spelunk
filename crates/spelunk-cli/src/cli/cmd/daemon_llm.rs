//! LLM configuration handed to the auto-spawned `spelunk-server` daemon.
//!
//! Resolution happens here, in the user's own session, and splits into two
//! channels:
//!
//! * **argv** carries the endpoint URL and model. Neither is secret, and `ps`
//!   showing which endpoint a daemon serves is a diagnostic feature.
//! * **the child environment** carries the credential, and nothing else does.
//!
//! The daemon is detached and long-lived, so it must never open the OS
//! keychain itself: on macOS a keychain read from a background process with no
//! session is an authorization prompt the user cannot see or answer. The CLI
//! reads the credential once, here, and passes it out of band. `--llm-key` is
//! therefore never emitted into the child's argv for any input, since argv is
//! world-readable through the process table.

use anyhow::Result;
use std::ffi::OsString;

use spelunk_core::config::{Config, llm_key, secret_store::SecretStore};

/// The LLM values a spawned daemon is configured with.
///
/// `key` is `Some` only when a credential actually resolved; it is never
/// rendered into [`LlmSpawn::args`].
#[derive(Default)]
pub(super) struct LlmSpawn {
    pub url: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
}

/// Hand-written so the credential cannot be leaked by a `{:?}` somewhere down
/// the line: a derived `Debug` would print it verbatim.
impl std::fmt::Debug for LlmSpawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSpawn")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Trim `raw` and treat a blank result as unset.
fn normalize(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

impl LlmSpawn {
    /// Resolve from a loaded [`Config`] plus optional per-spawn CLI overrides,
    /// using an injected secret store.
    ///
    /// `Config` has already folded `SPELUNK_LLM_URL` / `SPELUNK_LLM_MODEL`
    /// over the personal config file, so an override here is the top of the
    /// precedence chain.
    pub(super) fn resolve_with_store(
        cfg: &Config,
        url_override: Option<&str>,
        model_override: Option<&str>,
        store: &dyn SecretStore,
    ) -> Result<Self> {
        let url = normalize(url_override).or_else(|| normalize(cfg.llm_url.as_deref()));
        let model = normalize(model_override).or_else(|| normalize(cfg.llm_model.as_deref()));
        // Resolving the credential is the only secret-store read on this path,
        // and it happens only here (never in `Config::load`).
        let key = llm_key::resolve_with_store(store)?;
        Ok(Self { url, model, key })
    }

    /// Resolve using the host's default secret store.
    pub(super) fn resolve(
        cfg: &Config,
        url_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self> {
        let store = spelunk_core::config::default_secret_store()?;
        Self::resolve_with_store(cfg, url_override, model_override, store.as_ref())
    }

    /// The endpoint the daemon is to be configured with, and the model to send
    /// it, or `None` when no endpoint resolved.
    ///
    /// A model with no endpoint to send it to is not a configuration, so the
    /// model is reachable only through the endpoint. Both output channels read
    /// the configuration from here, which is what keeps them from disagreeing.
    fn endpoint(&self) -> Option<(&str, Option<&str>)> {
        Some((self.url.as_deref()?, self.model.as_deref()))
    }

    /// The daemon arguments carrying the non-secret LLM values.
    ///
    /// Empty unless an endpoint URL resolved, which keeps the daemon arg list
    /// byte-identical to an unconfigured spawn.
    pub(super) fn args(&self) -> Vec<OsString> {
        let Some((url, model)) = self.endpoint() else {
            return Vec::new();
        };
        let mut args: Vec<OsString> = vec!["--llm-url".into(), url.into()];
        if let Some(model) = model {
            args.push("--llm-model".into());
            args.push(model.into());
        }
        args
    }

    /// How the child's three LLM environment variables must be set, in order:
    /// `Some` means set to that value, `None` means unset it on the child.
    ///
    /// Every variable is named on every spawn. The child otherwise inherits
    /// this process's environment, and `spelunk-server`'s `--llm-url` /
    /// `--llm-model` carry clap `env` attributes, so an inherited value the CLI
    /// deliberately resolved away would still reach the daemon behind its back:
    /// an exported `SPELUNK_LLM_URL=""` arrives as a present-but-empty endpoint
    /// rather than as no endpoint. Naming all three makes the daemon's view
    /// exactly what this process resolved, whatever the parent exported.
    ///
    /// The credential travels here and nowhere else.
    pub(super) fn child_env(&self) -> Vec<(&'static str, Option<String>)> {
        let (url, model) = match self.endpoint() {
            Some((url, model)) => (Some(url.to_string()), model.map(str::to_string)),
            None => (None, None),
        };
        vec![
            (llm_key::ENV_LLM_URL, url),
            (llm_key::ENV_LLM_MODEL, model),
            (llm_key::ENV_LLM_KEY, self.key.clone()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::config::secret_store::MemoryStore;

    fn clear_env() {
        unsafe {
            std::env::remove_var("SPELUNK_LLM_KEY");
            std::env::remove_var("SPELUNK_LLM_URL");
            std::env::remove_var("SPELUNK_LLM_MODEL");
        }
    }

    fn cfg_with(url: Option<&str>, model: Option<&str>) -> Config {
        Config {
            llm_url: url.map(str::to_string),
            llm_model: model.map(str::to_string),
            ..Config::default()
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn env_entry(spawn: &LlmSpawn, name: &str) -> Option<String> {
        let env = spawn.child_env();
        let found = env.iter().find(|(n, _)| *n == name);
        assert!(
            found.is_some(),
            "{name} must be named on every spawn, or the child inherits it: {:?}",
            env.iter().map(|(n, _)| *n).collect::<Vec<_>>()
        );
        assert_eq!(
            env.iter().filter(|(n, _)| *n == name).count(),
            1,
            "{name} must be named exactly once"
        );
        found.unwrap().1.clone()
    }

    #[test]
    #[serial_test::serial]
    fn no_url_emits_no_llm_args() {
        clear_env();
        let store = MemoryStore::default();
        let spawn =
            LlmSpawn::resolve_with_store(&cfg_with(None, None), None, None, &store).unwrap();

        assert!(spawn.args().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn url_without_model_emits_only_the_url() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec!["--llm-url".to_string(), "http://127.0.0.1:1234".to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn url_and_model_emit_both_flags() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("gpt-oss")),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec![
                "--llm-url".to_string(),
                "http://127.0.0.1:1234".to_string(),
                "--llm-model".to_string(),
                "gpt-oss".to_string(),
            ]
        );
    }

    // A model with no endpoint to send it to is not a configuration.
    #[test]
    #[serial_test::serial]
    fn model_without_url_emits_nothing() {
        clear_env();
        let store = MemoryStore::default();
        let spawn =
            LlmSpawn::resolve_with_store(&cfg_with(None, Some("gpt-oss")), None, None, &store)
                .unwrap();

        assert!(spawn.args().is_empty(), "got {:?}", strings(&spawn.args()));
    }

    #[test]
    #[serial_test::serial]
    fn the_key_never_reaches_argv() {
        clear_env();
        let secret = "sk-llm-secret";
        for (url, model) in [
            (None, None),
            (Some("http://127.0.0.1:1234"), None),
            (None, Some("gpt-oss")),
            (Some("http://127.0.0.1:1234"), Some("gpt-oss")),
        ] {
            let store = MemoryStore::default();
            llm_key::set_with_store(secret, &store).unwrap();
            let spawn =
                LlmSpawn::resolve_with_store(&cfg_with(url, model), None, None, &store).unwrap();

            assert_eq!(spawn.key.as_deref(), Some(secret));
            let rendered = strings(&spawn.args());
            assert!(
                rendered.iter().all(|a| !a.contains(secret)),
                "credential leaked into argv for ({url:?}, {model:?}): {rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .all(|a| a != "--llm-key" && a != "--llm-key-file"),
                "a key flag was emitted for ({url:?}, {model:?}): {rendered:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_resolved_key_appears_once_in_the_child_env() {
        clear_env();
        let store = MemoryStore::default();
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            env_entry(&spawn, "SPELUNK_LLM_KEY"),
            Some("sk-llm-secret".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn no_key_means_the_child_entry_is_cleared_not_left_inherited() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(env_entry(&spawn, "SPELUNK_LLM_KEY"), None);
    }

    // The daemon reads SPELUNK_LLM_URL / SPELUNK_LLM_MODEL through clap `env`,
    // so anything this process resolved away has to be cleared on the child
    // rather than merely left out of argv. An exported `SPELUNK_LLM_URL=""` is
    // the case that made this visible: it resolves to no endpoint here and used
    // to arrive at the daemon as a present-but-empty one.
    #[test]
    #[serial_test::serial]
    fn a_resolved_endpoint_is_pinned_on_the_child_and_an_unresolved_one_is_cleared() {
        clear_env();
        let store = MemoryStore::default();

        let configured = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("gpt-oss")),
            None,
            None,
            &store,
        )
        .unwrap();
        assert_eq!(
            env_entry(&configured, "SPELUNK_LLM_URL"),
            Some("http://127.0.0.1:1234".to_string())
        );
        assert_eq!(
            env_entry(&configured, "SPELUNK_LLM_MODEL"),
            Some("gpt-oss".to_string())
        );

        let blanked =
            LlmSpawn::resolve_with_store(&cfg_with(Some(""), Some("")), None, None, &store)
                .unwrap();
        assert_eq!(env_entry(&blanked, "SPELUNK_LLM_URL"), None);
        assert_eq!(env_entry(&blanked, "SPELUNK_LLM_MODEL"), None);
    }

    // A model resolved without an endpoint emits no argv flag, so the child's
    // model variable must be cleared too: leaving it set would configure a
    // model the CLI decided not to send.
    #[test]
    #[serial_test::serial]
    fn a_model_without_an_endpoint_is_cleared_on_the_child() {
        clear_env();
        let store = MemoryStore::default();
        let spawn =
            LlmSpawn::resolve_with_store(&cfg_with(None, Some("gpt-oss")), None, None, &store)
                .unwrap();

        assert!(spawn.args().is_empty());
        assert_eq!(env_entry(&spawn, "SPELUNK_LLM_URL"), None);
        assert_eq!(env_entry(&spawn, "SPELUNK_LLM_MODEL"), None);
    }

    // Config::load has already folded SPELUNK_LLM_URL over the config file, so
    // the value the CLI resolved is authoritative and must be spelled out in
    // argv rather than left to the child's inherited environment.
    #[test]
    #[serial_test::serial]
    fn an_explicit_url_override_beats_the_inherited_env() {
        clear_env();
        unsafe { std::env::set_var("SPELUNK_LLM_URL", "http://from-env:1234") };
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://from-env:1234"), None),
            Some("http://from-arg:1234"),
            None,
            &store,
        );
        clear_env();

        assert_eq!(
            strings(&spawn.unwrap().args()),
            vec!["--llm-url".to_string(), "http://from-arg:1234".to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_explicit_model_override_beats_the_inherited_env() {
        clear_env();
        unsafe { std::env::set_var("SPELUNK_LLM_MODEL", "from-env") };
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("from-env")),
            None,
            Some("from-arg"),
            &store,
        );
        clear_env();

        assert_eq!(
            strings(&spawn.unwrap().args()),
            vec![
                "--llm-url".to_string(),
                "http://127.0.0.1:1234".to_string(),
                "--llm-model".to_string(),
                "from-arg".to_string(),
            ]
        );
    }

    // `SPELUNK_LLM_URL=""` reaches Config as `Some("")`, which is an override
    // that blanks the personal config rather than falling through to it. The
    // end state is no endpoint, not an `--llm-url ""` argument.
    #[test]
    #[serial_test::serial]
    fn a_blank_config_url_configures_no_endpoint() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("   "), Some("gpt-oss")),
            None,
            None,
            &store,
        )
        .unwrap();

        assert!(spawn.args().is_empty(), "got {:?}", strings(&spawn.args()));
    }

    // A blank flag value is an empty override, not an instruction to clear the
    // configured endpoint: the lower-precedence source still applies.
    #[test]
    #[serial_test::serial]
    fn a_blank_url_override_falls_back_to_the_configured_url() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            Some("  "),
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec!["--llm-url".to_string(), "http://127.0.0.1:1234".to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_blank_model_override_falls_back_to_the_configured_model() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("gpt-oss")),
            None,
            Some(""),
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec![
                "--llm-url".to_string(),
                "http://127.0.0.1:1234".to_string(),
                "--llm-model".to_string(),
                "gpt-oss".to_string(),
            ]
        );
    }

    // A store can hold a blank entry that `set_with_store` would have rejected
    // (written by an older build, or hand-edited into secrets.toml). It must
    // read as no credential, not as an empty-string one the child then carries.
    #[test]
    #[serial_test::serial]
    fn a_blank_stored_key_yields_no_child_env_entry() {
        clear_env();
        let store = MemoryStore::default();
        store
            .set(spelunk_core::config::secret_store::KEY_LLM_KEY, "   ")
            .unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(spawn.key, None);
        assert_eq!(env_entry(&spawn, "SPELUNK_LLM_KEY"), None);
    }

    // Trimming is what makes a stray newline from a piped `auth set-key` behave,
    // so the value handed to the child must be the trimmed one.
    #[test]
    #[serial_test::serial]
    fn a_stored_key_reaches_the_child_trimmed() {
        clear_env();
        let store = MemoryStore::default();
        store
            .set(
                spelunk_core::config::secret_store::KEY_LLM_KEY,
                "  sk-llm-secret\n",
            )
            .unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            env_entry(&spawn, "SPELUNK_LLM_KEY"),
            Some("sk-llm-secret".to_string())
        );
    }

    // A `{:?}` on this struct must never be the thing that leaks the key.
    #[test]
    #[serial_test::serial]
    fn debug_output_redacts_the_key() {
        clear_env();
        let store = MemoryStore::default();
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        let rendered = format!("{spawn:?}");
        assert!(!rendered.contains("sk-llm-secret"), "got {rendered}");
        assert!(rendered.contains("redacted"), "got {rendered}");
    }

    // The spawn path must resolve the credential from whichever backend
    // SPELUNK_SECRET_STORE selects, never by reaching past it to the keychain.
    #[test]
    #[serial_test::serial]
    fn resolves_from_a_file_backed_store_without_a_keychain() {
        clear_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let store =
            spelunk_core::config::secret_store::FileStore::new(tmp.path().join("secrets.toml"));
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();

        assert_eq!(store.kind(), "file");
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(spawn.key.as_deref(), Some("sk-llm-secret"));
    }
}
