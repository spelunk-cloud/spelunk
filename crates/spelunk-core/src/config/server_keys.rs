//! Per-origin server-key map (ADR-071 D1/D2).
//!
//! The client's bearer credential used to be a single flat `server_key`
//! (`secret_store::KEY_SERVER_KEY`), which cannot represent a developer who
//! holds keys for two different self-hosted `server_url`s (ADR-056's
//! recommended multi-server topology). This module gives the credential a
//! home keyed by the server it belongs to: a single secret-store entry
//! (`KEY_SERVER_KEYS_MAP`) whose payload is a JSON object mapping normalized
//! origin to key. One entry, not one per host, so granting keychain access
//! once covers every server (see the module-level rationale in ADR-071 D1).
//!
//! [`bearer_for`] is the resolution entry point: it decides the credential
//! *kind* (cloud vs. self-hosted) from the target `server_url`'s origin
//! before touching any store, so a given request only ever consults the
//! tier(s) its own kind uses (ADR-071 D2). The legacy flat entry is migrated
//! into the map lazily: the first time server-key-kind resolution needs a
//! credential for an origin the map does not yet have.

use anyhow::{Context, Result};
use std::collections::HashMap;

use super::AuthTokens;
use super::secret_store::{KEY_SERVER_KEY, SecretStore};

/// Key name for the per-origin server-key map: a single secret-store entry
/// distinct from the legacy flat [`KEY_SERVER_KEY`] (ADR-071 D1).
pub const KEY_SERVER_KEYS_MAP: &str = "server_keys";

/// Default spelunk.cloud API origin. Overridable via `SPELUNK_CLOUD_URL`,
/// which is read directly here (and by every cloud-api call site) so bearer
/// resolution, `/v1/me`, and WorkOS client-id selection all agree on the same
/// value. Single source of truth for the constant: `spelunk-cli`'s
/// `auth_api` module re-exports this rather than defining its own copy.
pub const DEFAULT_CLOUD_URL: &str = "https://api.spelunk.cloud";

/// Normalize `url` to its origin: scheme, lowercased host, and explicit
/// port (default port applied for comparison, omitted from the canonical
/// form when it matches the scheme default). This is the WHATWG origin
/// concept. Path, query, trailing slash, and host case do not participate
/// (ADR-071 D1).
pub fn normalize_origin(url: &str) -> Result<String> {
    let parsed =
        reqwest::Url::parse(url.trim()).with_context(|| format!("{url:?} is not a valid URL"))?;
    let origin = parsed.origin();
    if !origin.is_tuple() {
        anyhow::bail!("{url:?} has no origin (must be an http(s) URL)");
    }
    Ok(origin.ascii_serialization())
}

/// The cloud origin bearer resolution branches against (D2).
fn cloud_origin() -> Result<String> {
    let raw = std::env::var("SPELUNK_CLOUD_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_string());
    normalize_origin(&raw)
}

fn read_map(store: &dyn SecretStore) -> Result<HashMap<String, String>> {
    match store.get(KEY_SERVER_KEYS_MAP)? {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).context("parsing the server_keys map from the secret store")
        }
        _ => Ok(HashMap::new()),
    }
}

fn write_map(store: &dyn SecretStore, map: &HashMap<String, String>) -> Result<()> {
    let raw = serde_json::to_string(map).context("serialising the server_keys map")?;
    store.set(KEY_SERVER_KEYS_MAP, &raw)
}

/// Resolve (and lazily migrate) the server-key-kind credential for `origin`
/// (D2 tiers 2-3, non-cloud): a map hit wins; otherwise a legacy flat entry
/// is migrated into the map and deleted in one step, then returned. Migration
/// runs at most once per origin: once the map has an entry, the legacy tier
/// is never consulted again for it.
fn server_key_for_origin(origin: &str, store: &dyn SecretStore) -> Result<Option<String>> {
    let mut map = read_map(store)?;
    if let Some(key) = map.get(origin) {
        return Ok(Some(key.clone()));
    }
    let Some(legacy) = store.get(KEY_SERVER_KEY)? else {
        return Ok(None);
    };
    map.insert(origin.to_string(), legacy.clone());
    write_map(store, &map)?;
    store.delete(KEY_SERVER_KEY)?;
    eprintln!(
        "spelunk: migrated a legacy server key into the per-server key map for {origin}. \
         Run `spelunk auth set-key --server <url>` for any other server you use."
    );
    Ok(Some(legacy))
}

/// Resolve the effective bearer for a request to `server_url` (ADR-071 D2).
///
/// Branches on credential kind by origin before touching any store:
/// * **Cloud kind** (origin matches [`DEFAULT_CLOUD_URL`] /
///   `SPELUNK_CLOUD_URL`): `SPELUNK_SERVER_KEY` env, then `[auth]`'s access
///   token. The map and the legacy entry are never consulted.
/// * **Server-key kind** (any other origin): `SPELUNK_SERVER_KEY` env, then
///   the per-origin map, then (migrating on read) the legacy flat entry.
///   `[auth]` is never consulted.
pub fn bearer_for(
    auth: Option<&AuthTokens>,
    server_url: &str,
    store: &dyn SecretStore,
) -> Result<Option<String>> {
    if let Ok(v) = std::env::var("SPELUNK_SERVER_KEY") {
        return Ok(Some(v));
    }
    let origin = normalize_origin(server_url)?;
    if origin == cloud_origin()? {
        return Ok(auth.map(|a| a.access_token.clone()));
    }
    server_key_for_origin(&origin, store)
}

/// `spelunk auth set-key --server <url>`: store `key` for `url`'s origin.
/// Returns the normalized origin it was stored under.
pub fn set_key_for_origin(server_url: &str, key: &str, store: &dyn SecretStore) -> Result<String> {
    let origin = normalize_origin(server_url)?;
    let mut map = read_map(store)?;
    map.insert(origin.clone(), key.to_string());
    write_map(store, &map)?;
    Ok(origin)
}

/// `spelunk auth list-servers`: origins with a stored key (sorted), plus
/// whether a legacy (not-yet-migrated) flat key is also present. Never
/// returns key material.
pub fn list_origins(store: &dyn SecretStore) -> Result<(Vec<String>, bool)> {
    let map = read_map(store)?;
    let mut origins: Vec<String> = map.into_keys().collect();
    origins.sort();
    let legacy = store.get(KEY_SERVER_KEY)?.is_some();
    Ok((origins, legacy))
}

/// Count of stored server-key credentials: map entries plus one if a legacy
/// entry is still present (used by bare `spelunk logout` to report what it
/// left untouched, D3).
pub fn count(store: &dyn SecretStore) -> Result<usize> {
    let (origins, legacy) = list_origins(store)?;
    Ok(origins.len() + usize::from(legacy))
}

/// `spelunk logout --servers`: clear the per-origin map.
///
/// Only the map. The legacy flat entry (and any plaintext remnant still in
/// `config.toml`) is a separate concern with its own belt-and-braces cleanup
/// in [`super::remove_server_key`]; callers that want both call both (see
/// `spelunk logout`'s `--servers` handling).
pub fn clear_all(store: &dyn SecretStore) -> Result<()> {
    store
        .delete(KEY_SERVER_KEYS_MAP)
        .context("clearing the server_keys map")
}

/// `spelunk logout --server <url>`: clear only that origin's credential.
/// Returns the normalized origin that was cleared.
///
/// A map entry for the origin is removed if present. Otherwise, when a
/// legacy flat entry still exists, it is removed too: pre-migration the
/// legacy entry is the fallback for *every* unmapped origin (see
/// [`server_key_for_origin`]), so it may be serving this very one.
pub fn clear_origin(server_url: &str, store: &dyn SecretStore) -> Result<String> {
    let origin = normalize_origin(server_url)?;
    let mut map = read_map(store)?;
    if map.remove(&origin).is_some() {
        write_map(store, &map)?;
    } else {
        store
            .delete(KEY_SERVER_KEY)
            .context("clearing the legacy server_key entry")?;
    }
    Ok(origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret_store::MemoryStore;

    fn tokens(access_token: &str) -> AuthTokens {
        AuthTokens {
            access_token: access_token.to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 4_000_000_000,
            org_id: "org_1".to_string(),
        }
    }

    fn clear_env() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
            std::env::remove_var("SPELUNK_CLOUD_URL");
        }
    }

    // ── normalize_origin ──────────────────────────────────────────────────

    #[test]
    fn normalize_origin_omits_default_port_and_lowercases_host() {
        assert_eq!(
            normalize_origin("https://Spelunk.Internal.Example.Com/foo?x=1#y").unwrap(),
            "https://spelunk.internal.example.com"
        );
    }

    #[test]
    fn normalize_origin_keeps_explicit_non_default_port() {
        assert_eq!(
            normalize_origin("https://other.example.net:8443/").unwrap(),
            "https://other.example.net:8443"
        );
    }

    #[test]
    fn normalize_origin_ignores_path_query_and_trailing_slash() {
        let a = normalize_origin("http://team.example:7777/a/b?x=1").unwrap();
        let b = normalize_origin("http://team.example:7777/").unwrap();
        let c = normalize_origin("http://team.example:7777").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn normalize_origin_rejects_invalid_url() {
        assert!(normalize_origin("not a url").is_err());
    }

    // ── bearer_for: env always wins, no store touch ─────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_env_wins_over_everything_and_skips_store() {
        clear_env();
        unsafe { std::env::set_var("SPELUNK_SERVER_KEY", "sk-from-env") };

        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();
        let auth = tokens("at-cloud");

        // Even for the cloud origin, env wins and the map/legacy tiers are
        // untouched by the resolution (no side effect on the store).
        let result = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        assert_eq!(result.as_deref(), Some("sk-from-env"));
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-legacy")
        );

        unsafe { std::env::remove_var("SPELUNK_SERVER_KEY") };
    }

    // ── bearer_for: cloud kind ───────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_uses_auth_token() {
        clear_env();
        let store = MemoryStore::default();
        let auth = tokens("at-cloud");

        let result = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        assert_eq!(result.as_deref(), Some("at-cloud"));
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_without_auth_is_none() {
        clear_env();
        let store = MemoryStore::default();
        assert_eq!(bearer_for(None, DEFAULT_CLOUD_URL, &store).unwrap(), None);
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_never_touches_map_or_legacy() {
        clear_env();
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();
        let auth = tokens("at-cloud");

        let result = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        assert_eq!(result.as_deref(), Some("at-cloud"));
        // The legacy entry must be untouched: cloud-kind resolution never
        // migrates or reads it.
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-legacy")
        );
    }

    // ── bearer_for: server-key kind ──────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_non_cloud_origin_uses_map_entry_ignoring_auth() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://team.example:7777", "sk-team", &store).unwrap();
        let auth = tokens("at-cloud");

        // A cloud [auth] token must never leak to a self-hosted origin.
        let result = bearer_for(Some(&auth), "https://team.example:7777", &store).unwrap();
        assert_eq!(result.as_deref(), Some("sk-team"));
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_non_cloud_origin_migrates_legacy_entry_on_first_use() {
        clear_env();
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();

        let result = bearer_for(None, "https://team.example:7777", &store).unwrap();
        assert_eq!(result.as_deref(), Some("sk-legacy"));

        // Migrated into the map...
        let (origins, legacy) = list_origins(&store).unwrap();
        assert_eq!(origins, vec!["https://team.example:7777".to_string()]);
        // ...and removed from the legacy tier.
        assert!(!legacy, "legacy entry must be deleted after migration");
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_non_cloud_origin_no_credential_anywhere_is_none() {
        clear_env();
        let store = MemoryStore::default();
        assert_eq!(
            bearer_for(None, "https://team.example:7777", &store).unwrap(),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_legacy_migration_does_not_leak_to_a_second_unmapped_origin() {
        clear_env();
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();

        // First origin migrates and consumes the legacy entry.
        let first = bearer_for(None, "https://a.example:7777", &store).unwrap();
        assert_eq!(first.as_deref(), Some("sk-legacy"));

        // A second, still-unmapped origin must fail closed, not silently
        // reuse the first origin's now-migrated key.
        let second = bearer_for(None, "https://b.example:7777", &store).unwrap();
        assert_eq!(second, None);
    }

    // ── D1: the map's on-the-wire JSON shape ─────────────────────────────────

    /// D1: the payload behind the single `KEY_SERVER_KEYS_MAP` entry is a
    /// flat JSON object of `origin -> key`, nothing more (no envelope, no
    /// metadata). Verified against the raw string the store holds, not just
    /// through the read helpers that would mask a shape drift.
    #[test]
    fn map_entry_payload_is_a_flat_json_object_of_origin_to_key() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example:7777", "sk-a", &store).unwrap();
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();

        let raw = store.get(KEY_SERVER_KEYS_MAP).unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "https://a.example:7777": "sk-a",
                "https://b.example": "sk-b",
            })
        );
        // A single entry, not one per origin (D1's whole point).
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
    }

    #[test]
    fn empty_map_leaves_no_entry_rather_than_an_empty_json_object() {
        // Before any set_key_for_origin call, the entry must not exist at all
        // (read_map treats "absent" and "{}" the same, but nothing should
        // write an empty object pre-emptively).
        let store = MemoryStore::default();
        assert_eq!(list_origins(&store).unwrap(), (vec![], false));
        assert_eq!(store.get(KEY_SERVER_KEYS_MAP).unwrap(), None);
    }

    // ── set_key_for_origin / list_origins ────────────────────────────────────

    #[test]
    fn set_key_for_origin_normalizes_and_overwrites() {
        let store = MemoryStore::default();
        set_key_for_origin("https://Team.Example:7777/ignored/path", "sk-1", &store).unwrap();
        set_key_for_origin("https://team.example:7777", "sk-2", &store).unwrap();

        let (origins, _) = list_origins(&store).unwrap();
        assert_eq!(origins, vec!["https://team.example:7777".to_string()]);
        assert_eq!(
            bearer_for(None, "https://team.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-2")
        );
    }

    #[test]
    fn list_origins_reports_legacy_flag_independently_of_map() {
        let store = MemoryStore::default();
        assert_eq!(list_origins(&store).unwrap(), (vec![], false));

        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        let (origins, legacy) = list_origins(&store).unwrap();
        assert_eq!(origins, vec!["https://a.example".to_string()]);
        assert!(legacy);
    }

    // ── clear_all / clear_origin / count ─────────────────────────────────────

    #[test]
    fn clear_all_removes_the_map_but_not_the_legacy_entry() {
        // The legacy entry is a separate concern (config::remove_server_key
        // handles it, including the plaintext config.toml remnant); see
        // `clear_all`'s doc comment.
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();

        clear_all(&store).unwrap();

        let (origins, legacy) = list_origins(&store).unwrap();
        assert_eq!(origins, Vec::<String>::new());
        assert!(legacy, "clear_all must not touch the legacy entry");
    }

    #[test]
    fn clear_origin_removes_only_that_origin() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();

        clear_origin("https://a.example", &store).unwrap();

        let (origins, _) = list_origins(&store).unwrap();
        assert_eq!(origins, vec!["https://b.example".to_string()]);
    }

    #[test]
    fn clear_origin_falls_back_to_legacy_when_origin_not_yet_mapped() {
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();

        // This origin was never resolved (so never migrated), but the legacy
        // entry might be serving it: clear_origin removes it defensively.
        clear_origin("https://never-resolved.example", &store).unwrap();

        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None);
    }

    #[test]
    fn clear_origin_is_a_no_op_when_nothing_is_stored() {
        let store = MemoryStore::default();
        clear_origin("https://nothing.example", &store).unwrap();
        assert_eq!(list_origins(&store).unwrap(), (vec![], false));
    }

    #[test]
    fn count_reflects_map_size_plus_legacy() {
        let store = MemoryStore::default();
        assert_eq!(count(&store).unwrap(), 0);
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        assert_eq!(count(&store).unwrap(), 1);
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();
        assert_eq!(count(&store).unwrap(), 2);
    }

    // ── adversarial: independent test-engineer coverage ──────────────────────
    //
    // The tests above are the Engineer's own suite. These probe cases their
    // own tests don't: a corrupted store payload, and a map entry coexisting
    // with the legacy tier for a *different* origin than the one already
    // migrated (the scenario the ADR's "no cross-origin leak" claim rests on
    // but the Engineer's `..._does_not_leak_to_a_second_unmapped_origin` test
    // only exercises with an *empty* map, not a map that already has an
    // unrelated origin in it).

    /// A corrupted `server_keys` payload must fail resolution loudly (`Err`),
    /// never silently fall through to "no credential" or, worse, to the
    /// legacy tier. A silent fallthrough here would be the dangerous case: an
    /// operator could believe a server is unauthenticated-safe (loopback,
    /// firewalled) when in fact resolution swallowed a real error and just
    /// returned `None`, or could get a stale/wrong key from the legacy tier
    /// instead of a clear "your credential store is broken" signal.
    #[test]
    #[serial_test::serial]
    fn corrupted_map_json_fails_resolution_loudly_not_silently() {
        clear_env();
        let store = MemoryStore::default();
        // Not valid JSON at all.
        store.set(KEY_SERVER_KEYS_MAP, "{not valid json").unwrap();
        // A legacy entry is also present: a silent-fallthrough implementation
        // could mistakenly hand this out instead of surfacing the corruption.
        store
            .set(KEY_SERVER_KEY, "sk-legacy-should-not-be-returned")
            .unwrap();

        let result = bearer_for(None, "https://team.example:7777", &store);
        assert!(
            result.is_err(),
            "a corrupted map must fail loudly, not resolve to Some/None silently; got {result:?}"
        );

        // Same for the JSON tools directly: a valid JSON value of the wrong
        // shape (an array, not an object) must also fail, not deserialize
        // into an empty/default map.
        store
            .set(KEY_SERVER_KEYS_MAP, "[\"not\", \"a\", \"map\"]")
            .unwrap();
        let result2 = bearer_for(None, "https://team.example:7777", &store);
        assert!(
            result2.is_err(),
            "a wrong-shaped-but-valid-JSON map must also fail loudly; got {result2:?}"
        );
    }

    /// `set_key_for_origin` / `list_origins` must likewise surface a
    /// corrupted map rather than silently treating it as empty and
    /// overwriting it (which would quietly discard whatever the corrupted
    /// payload's other origins were, destroying credentials for servers the
    /// corruption didn't even touch).
    #[test]
    fn corrupted_map_json_fails_set_and_list_loudly() {
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEYS_MAP, "{not valid json").unwrap();

        assert!(set_key_for_origin("https://a.example", "sk-a", &store).is_err());
        assert!(list_origins(&store).is_err());
    }

    /// Both a per-origin map entry AND the legacy flat key exist
    /// simultaneously (the realistic post-partial-migration state: one
    /// origin was already explicitly `auth set-key`'d while another is still
    /// waiting on its first-use migration). Resolving the *already-mapped*
    /// origin must return the map's value and must NOT touch or delete the
    /// legacy entry, since it isn't this origin's to consume. Resolving the
    /// *unmapped* origin afterward must migrate the legacy entry, and must
    /// leave the first origin's map entry untouched (no cross-origin
    /// overwrite of an unrelated key while writing the map back).
    #[test]
    #[serial_test::serial]
    fn mapped_origin_and_legacy_entry_coexist_without_leaking_or_clobbering() {
        clear_env();
        let store = MemoryStore::default();
        // Origin A was explicitly set via `auth set-key`.
        set_key_for_origin("https://a.example:7777", "sk-a-explicit", &store).unwrap();
        // A legacy flat key also still exists (not yet migrated, so it belongs
        // to whichever origin first resolves through the fallback tier).
        store.set(KEY_SERVER_KEY, "sk-legacy").unwrap();

        // Resolving A must return A's own key, untouched by the legacy tier.
        let a = bearer_for(None, "https://a.example:7777", &store).unwrap();
        assert_eq!(a.as_deref(), Some("sk-a-explicit"));
        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-legacy"),
            "resolving an already-mapped origin must not touch the legacy entry"
        );

        // Resolving a second, unmapped origin B migrates the legacy entry
        // into B's slot...
        let b = bearer_for(None, "https://b.example:7777", &store).unwrap();
        assert_eq!(b.as_deref(), Some("sk-legacy"));
        assert_eq!(store.get(KEY_SERVER_KEY).unwrap(), None, "legacy consumed");

        // ...and A's own key must be exactly what it was before, not
        // overwritten by the read-modify-write that added B.
        let (mut origins, legacy) = list_origins(&store).unwrap();
        origins.sort();
        assert_eq!(
            origins,
            vec![
                "https://a.example:7777".to_string(),
                "https://b.example:7777".to_string(),
            ]
        );
        assert!(!legacy);
        assert_eq!(
            bearer_for(None, "https://a.example:7777", &store)
                .unwrap()
                .as_deref(),
            Some("sk-a-explicit"),
            "A's key must survive B's migration write unchanged"
        );
    }

    /// `clear_origin` on an origin that is present in the map must remove
    /// only the map entry, even when a legacy entry also still exists
    /// (serving some *other*, not-yet-migrated origin); it must not
    /// mistakenly delete the legacy entry too, which would strand whichever
    /// other origin is still relying on it.
    #[test]
    fn clear_origin_on_mapped_origin_does_not_touch_an_unrelated_legacy_entry() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example:7777", "sk-a", &store).unwrap();
        store
            .set(KEY_SERVER_KEY, "sk-legacy-for-someone-else")
            .unwrap();

        clear_origin("https://a.example:7777", &store).unwrap();

        assert_eq!(
            store.get(KEY_SERVER_KEY).unwrap().as_deref(),
            Some("sk-legacy-for-someone-else"),
            "clearing a mapped origin must leave an unrelated legacy entry intact"
        );
        let (origins, _) = list_origins(&store).unwrap();
        assert!(origins.is_empty());
    }

    /// The two-server, two-key motivating case (task's acceptance sketch),
    /// driven purely through the public resolution/storage API with no env
    /// var involved at any point: both origins resolve to their own key on
    /// repeated, interleaved lookups, with no state that could cause a
    /// second read to see the first origin's key.
    #[test]
    #[serial_test::serial]
    fn two_projects_two_origins_two_keys_resolve_independently_interleaved() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://proj-a.example:7777", "sk-proj-a", &store).unwrap();
        set_key_for_origin("https://proj-b.example:9443", "sk-proj-b", &store).unwrap();

        // Interleave lookups (A, B, A, B) to catch any accidental
        // last-write-wins / shared-mutable-state bug a naive cache could
        // introduce.
        for _ in 0..3 {
            assert_eq!(
                bearer_for(None, "https://proj-a.example:7777", &store)
                    .unwrap()
                    .as_deref(),
                Some("sk-proj-a")
            );
            assert_eq!(
                bearer_for(None, "https://proj-b.example:9443", &store)
                    .unwrap()
                    .as_deref(),
                Some("sk-proj-b")
            );
        }
    }
}
