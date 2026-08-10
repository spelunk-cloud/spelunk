use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sync mode
// ---------------------------------------------------------------------------

/// Persistent, per-project control over where memory reads/writes go and whether
/// the CLI ever contacts the cloud.
///
/// Replaces the implicit "is the server reachable" branch that previously drove
/// backend selection. The mode is resolved once from config + environment (see
/// [`Config::resolve_mode`]) and then gates both the capability tier probe and
/// the memory backend selector.
///
/// | mode          | reads          | writes                    | cloud contact            |
/// |---------------|----------------|---------------------------|--------------------------|
/// | `offline`     | local          | local                     | never (even if `server_url` set) |
/// | `local_first` | local          | local, then async background sync | best-effort              |
/// | `cloud_first` | server (error if unreachable) | server (error if unreachable) | required |
///
/// `cloud_first` is the **server-authoritative** option: it is a deliberate
/// override of ADR-004's local-as-source-of-truth invariant, and an
/// unreachable or untrusted server is a hard error. There is no silent local
/// fallback and no local write queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Local-only: never contacts the cloud, even when `server_url` is set.
    /// The provable no-cloud guarantee for OSS testing and air-gapped use.
    Offline,
    /// Default when `server_url` is set: reads and writes are local; a best-effort
    /// background sync converges the cloud replica. Offline-resilient.
    LocalFirst,
    /// Server-authoritative: reads and writes go to the server, and an
    /// unreachable or untrusted server is a hard error. No silent local
    /// fallback, no local write queue.
    CloudFirst,
}

impl SyncMode {
    /// Parse a mode from its serialized string form (case-insensitive).
    ///
    /// Accepts `offline`, `local_first`, and `cloud_first`. Returns `None` for
    /// any other value so callers can decide how to handle an invalid override.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "offline" => Some(Self::Offline),
            "local_first" | "local-first" | "localfirst" => Some(Self::LocalFirst),
            "cloud_first" | "cloud-first" | "cloudfirst" => Some(Self::CloudFirst),
            _ => None,
        }
    }

    /// The accepted values, formatted for an error message
    /// (`offline, local_first, cloud_first`).
    ///
    /// Single source of truth shared by the `SPELUNK_MODE` env-var error and
    /// the `config.toml` parse error, so the two can never drift.
    pub fn valid_values() -> &'static str {
        "offline, local_first, cloud_first"
    }

    /// String form used in config files and `SPELUNK_MODE`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::LocalFirst => "local_first",
            Self::CloudFirst => "cloud_first",
        }
    }

    /// Whether this mode permits any contact with the cloud server.
    ///
    /// `offline` never contacts the cloud; the other two may. Used by the
    /// capability tier probe and the memory backend selector to honour the
    /// kill-switch semantics of `offline`.
    pub fn allows_cloud(&self) -> bool {
        !matches!(self, Self::Offline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SyncMode parse / as_str ───────────────────────────────────────────────

    #[test]
    fn sync_mode_parse_accepts_canonical_and_variant_forms() {
        assert_eq!(SyncMode::parse("offline"), Some(SyncMode::Offline));
        assert_eq!(SyncMode::parse("LOCAL_FIRST"), Some(SyncMode::LocalFirst));
        assert_eq!(SyncMode::parse("local-first"), Some(SyncMode::LocalFirst));
        assert_eq!(SyncMode::parse("cloud_first"), Some(SyncMode::CloudFirst));
        assert_eq!(SyncMode::parse(" cloudfirst "), Some(SyncMode::CloudFirst));
        assert_eq!(SyncMode::parse("bogus"), None);
    }

    #[test]
    fn sync_mode_as_str_round_trips() {
        for m in [
            SyncMode::Offline,
            SyncMode::LocalFirst,
            SyncMode::CloudFirst,
        ] {
            assert_eq!(SyncMode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn sync_mode_allows_cloud() {
        assert!(!SyncMode::Offline.allows_cloud());
        assert!(SyncMode::LocalFirst.allows_cloud());
        assert!(SyncMode::CloudFirst.allows_cloud());
    }

    #[test]
    fn sync_mode_serde_snake_case() {
        // Serialised form must be snake_case so config.toml / wire stay stable.
        let json = serde_json::to_string(&SyncMode::LocalFirst).unwrap();
        assert_eq!(json, "\"local_first\"");
        let parsed: SyncMode = serde_json::from_str("\"cloud_first\"").unwrap();
        assert_eq!(parsed, SyncMode::CloudFirst);
    }
}
