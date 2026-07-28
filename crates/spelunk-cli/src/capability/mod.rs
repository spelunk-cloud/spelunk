//! Capability tier detection for the spelunk CLI.
//!
//! Tier 0 (Offline): no server_url configured, or server unreachable.
//! Tier 1 (Server):  server_url set and GET /v1/health succeeds.
//!
//! ## Loopback auto-discovery (spelunk#316 / 0.8.0)
//!
//! When `cfg.server_url` is `None` **and** `SPELUNK_NO_SERVER` is not set, the probe
//! attempts to reach a locally-running spelunk-server before falling through to
//! `Tier::Offline`:
//!
//! 1. Read `~/.local/state/spelunk/server.port` (written by `spelunk server start`);
//!    use `http://127.0.0.1:<port>` if the file exists.
//! 2. Otherwise probe `http://127.0.0.1:7777` with a **250 ms** timeout (distinct from
//!    the 2 s timeout used for explicitly-configured remote URLs).
//! 3. On success, treat as `Tier::Server` with `auto_discovered = true`.
//! 4. On failure, return `Tier::Offline`.
//!
//! `SPELUNK_NO_SERVER=1` short-circuits all loopback probing and forces `Tier::Offline`.
//!
//! The probe runs lazily on the first call that needs Tier 1 and its result
//! is cached for the process lifetime.
//!
//! ## Module layout
//!
//! - [`state`]: the data types parsed from `/v1/health` (`Capabilities`,
//!   `EmbedderState`, `ServerLimits`).
//! - [`tier`]: the resolved [`Tier`] enum itself.
//! - [`probe`]: loopback auto-discovery + explicit `server_url` health probing,
//!   and the per-process `Tier` cache (`get_tier`).
//! - [`diagnostics`]: probe-failure classification and TLS error rendering.
//! - [`guard`]: the `require_*` functions commands call to gate a feature on
//!   a `Tier`.

mod diagnostics;
mod guard;
mod probe;
mod state;
mod tier;

pub use diagnostics::{ConnFailure, explicit_probe_failure};
pub use guard::{inference_server_required_message, require_explicit_server_url, require_tier1};
pub(crate) use probe::spelunk_state_dir;
pub use probe::{get_inference_tier, get_inference_tier_fresh, get_tier};
// `Capabilities` is only reached from outside this module by other crates'
// `#[cfg(test)]` code (`Capabilities::all()`), so a non-test build sees this
// re-export as unused.
#[allow(unused_imports)]
pub use state::Capabilities;
pub use state::{EmbedderState, ServerLimits};
pub use tier::Tier;
