# ADR-050: CLI auto-update check

**Date:** 2026-06-28  
**Deciders:** Architect  
**Trigger:** Users on the curl/PowerShell installer have no way to
learn that a newer release exists; they keep running an old `spelunk` until they
happen to re-run the install one-liner. We want a lightweight, opt-out update
*notification* that respects package-manager- and MDM-managed installs.

---

## Context

The CLI is distributed through several channels:

- The curl/PowerShell one-liner (`install.sh` / `install.ps1`), which drops
  `spelunk` + `spelunk-server` into `/usr/local/bin` or `~/.local/bin` and
  downloads `spelunk-${VERSION}-${TARGET}.tar.gz` from
  `https://github.com/spelunk-cloud/spelunk/releases`.
- Package managers (Homebrew is live; winget is planned). These
  own the binary, place it under a manager-controlled prefix (Homebrew Cellar,
  `/opt/homebrew`, `/usr/local/Cellar`, …), and expect the user to upgrade
  *through the manager* (`brew upgrade`). The binary is often on a read-only or
  manager-owned path.
- IT/MDM provisioning (an MDM example is planned), where the
  endpoint is centrally managed and the user must not self-mutate it.

Current facts the design builds on:

- `--version` already exists (#449): clap derives it from `CARGO_PKG_VERSION`, so
  the running binary's version is `env!("CARGO_PKG_VERSION")` (e.g. `0.9.0`).
- Release tags are `vMAJOR.MINOR.PATCH` (e.g. `v0.9.0`); the GitHub
  `tag_name` carries the leading `v`, the crate version does not. Comparison must
  normalise the `v` prefix.
- Config lives at `~/.config/spelunk/config.toml` (`spelunk_config_dir()` in
  `spelunk-core/src/config.rs`), env vars are prefixed `SPELUNK_`.
- GitHub release assets expose a `digest` field shaped `sha256:<hex>` (already
  consumed by `release.yml`), so per-asset checksums are available from the API
  for free.

### Forces

- **Safety first.** The binary may sit on a path owned by root or a package
  manager. Silently overwriting it can break a managed install, fight `brew`, or
  require privilege escalation we should never request implicitly.
- **Never break a normal command.** A flaky network, a GitHub rate-limit, or a
  parse error must never delay or fail `spelunk search`, `spelunk index`, etc.
- **Cheap to maintain.** The founder's intent is explicitly "doesn't have to be
  advanced." We want the smallest thing that is useful and safe.
- **Forward-compatible disable surface.** The planned MDM and winget paths
  both need a single, well-known way to turn this off. That mechanism is the
  load-bearing part of this ADR.

---

## Decision

### D1 — Update behaviour: **notify-only** (no self-replacing binary in v1)

When a newer release is detected, print a single non-fatal line to **stderr**
after the user's command completes, e.g.:

```
A new version of spelunk is available: 0.9.0 → 0.10.0
  https://github.com/spelunk-cloud/spelunk/releases/latest
  Upgrade: re-run the install script, or `brew upgrade spelunk` if installed via Homebrew.
```

We **do not** download and replace the running binary in this iteration.
Self-replacing a binary carries real integrity and safety obligations that are
out of proportion to the founder's "doesn't have to be advanced" intent:

- signature/checksum verification of the downloaded artifact before swap,
- atomic, partial-write-safe replacement (download to temp, verify, rename) to
  avoid bricking the binary on an interrupted write,
- correct handling of permissions on package-manager-/root-owned paths, and the
  privilege-escalation question that follows,
- platform asymmetry (replacing a running executable on Windows is its own can of
  worms).

Notify-only sidesteps all of this: the network artifact is never executed and
never written over the live binary, so the worst-case failure is a missing or
stale notification, never a broken install.

A `spelunk self-update` *subcommand* (interactive, explicit, with checksum
verification via the asset `digest`) is the natural follow-up, but is **out of
scope for this ADR** — file a separate task. The notification message therefore
points at the install script / package manager, **not** at a `self-update`
command that does not yet exist.

### D2 — Disable surface + managed-install auto-detection

Three layers, checked in order; the first that says "disabled" wins:

1. **Env var — `SPELUNK_NO_UPDATE_CHECK`.** Truthy (`1`, `true`, `yes`) disables
   the check entirely. This is the knob MDM profiles and CI set. (Consistent with
   the existing `SPELUNK_NO_SERVER` precedent.)
2. **Config key — `[update] check = false`.** A new optional `[update]` table in
   `config.toml`:

   ```toml
   [update]
   check = true        # default true; set false to disable the update check
   ```

   Modelled as `Option<UpdateConfig>` on `Config` so the serde default preserves
   today's behaviour (absent table ⇒ checks enabled). Read through a
   `Config::update_check_enabled()` accessor, never directly.
3. **Auto-detection of managed installs (default-off without any config).** Even
   when neither of the above is set, the check is **suppressed** when the running
   binary appears to be managed by a package manager or a read-only/managed path,
   so we never nag a user to "re-run the installer" against an install we don't
   own. Detection is heuristic and conservative — when in doubt, suppress (a
   missed notification is harmless; a wrong one is annoying and, for MDM, wrong):
   - the resolved binary path (`std::env::current_exe()`, canonicalised) is under
     a known package-manager prefix: any path component `Cellar`, or a prefix of
     `/opt/homebrew`, `/home/linuxbrew/.linuxbrew`, `/usr/local/Cellar`,
     `/var/lib/flatpak`, `/snap`, `/nix/store`, or (Windows) a winget/`WindowsApps`
     path; **or**
   - the directory containing the binary is **not writable** by the current user
     (a strong signal the user can't self-upgrade anyway).

   Precedence note: an explicit `SPELUNK_FORCE_UPDATE_CHECK=1` may override
   auto-detection (escape hatch for testing); the explicit *disable* paths (D2.1,
   D2.2) always win over force.

Concrete names to implement:
- env (disable): `SPELUNK_NO_UPDATE_CHECK`
- env (force, test/override): `SPELUNK_FORCE_UPDATE_CHECK`
- config: `[update] check = <bool>` (default `true`)

### D3 — Cadence + state (separate state file, not `config.toml`)

- **State location:** a small machine-local state file
  `~/.config/spelunk/state.toml`, **distinct from `config.toml`**. `config.toml`
  is user-authored / git-trackable / written by `login`; the update timestamp is
  churny machine state and must not create noisy diffs or risk corrupting
  hand-edited config. State file shape:

  ```toml
  [update]
  last_check = "2026-06-28T10:00:00Z"   # RFC3339 UTC
  last_seen_version = "0.10.0"          # optional: latest tag observed, for offline re-notify
  ```

- **Trigger:** on CLI startup, after arg parsing, the check is considered "due"
  when `now - last_check >= 24h` (≥ 1 day). If due, run the check; on success,
  rewrite `last_check` (and `last_seen_version`). If the file is missing/unparseable,
  treat as due and recreate it.
- **Best-effort + non-blocking:** the check must never block or fail the user's
  command:
  - Hard timeout on the GitHub request (**≤ 2s**, `reqwest` with a connect+read
    timeout).
  - Any error (network down, DNS, HTTP 403 rate-limit, malformed JSON, timeout)
    is **swallowed** — log at `debug` only, print nothing, and still update
    `last_check` so we don't hammer GitHub on every invocation when it's
    unreachable.
  - The notification is emitted on **stderr after** the primary command's work,
    so it never interleaves with machine-readable stdout (plumbing/JSONL output
    stays clean). The check is skipped entirely for plumbing/JSONL commands and
    when stdout/stderr is not a TTY (non-interactive/scripted use), in addition
    to the D2 disable paths.

### D4 — Release source + version determination

- **Source:** `GET https://api.github.com/repos/spelunk-cloud/spelunk/releases/latest`,
  `Accept: application/vnd.github+json`, with a `User-Agent: spelunk/<version>`
  header. Unauthenticated (60 req/hr/IP is ample given the ≥24h cadence); a
  403/rate-limit is treated as a swallowed error per D3. Read `tag_name`.
- **Current version:** `env!("CARGO_PKG_VERSION")` (the same source `--version`
  uses), so the running binary always self-reports accurately.
- **Comparison:** strip a leading `v` from `tag_name`, parse both sides as
  semver, and notify only when `latest > current`. Pre-release / non-semver tags
  are ignored (no notification). Never notify on equal or downgrade.

---

## Security considerations

- **No code execution / no write-over-binary.** Notify-only means the only
  external input consumed is a JSON document parsed into a version string and a
  URL we render literally; nothing downloaded is executed or written over the
  live binary. The threat surface is parsing untrusted JSON — bounded by the
  response-size/timeout limits and strict deserialization.
- **Render, don't follow.** The release URL is printed as text; the CLI does not
  fetch or open it.
- **State file is non-secret** (timestamp + version only). It must never hold
  tokens; written with `0600` for tidiness.
- **MDM/managed honour.** Auto-detection (D2.3) plus the env/config disables
  ensure a centrally managed endpoint is never told to self-upgrade. This is the
  acceptance-critical security property for the MDM path.
- This ADR introduces a new outbound network call (CLI → api.github.com) and a
  new state file; update `docs/security/THREAT-MODEL.md` accordingly during
  implementation.

---

## Consequences

- The MDM example documents `SPELUNK_NO_UPDATE_CHECK=1` (and/or
  `[update] check = false`) as the managed-install opt-out; auto-detection is a
  belt-and-braces backstop.
- The winget path relies on the same disable surface + the winget path
  heuristic in D2.3 so winget-managed installs don't self-nag.
- A future `spelunk self-update` subcommand can reuse D4's release lookup and the
  asset `digest` (`sha256:<hex>`) for verified, explicit, opt-in upgrades — but
  is a separate, security-reviewed task, not part of this one.

## Alternatives considered

- **Auto-download-and-replace (rejected for v1):** highest user convenience but
  the integrity/permissions/partial-write/privilege surface is disproportionate
  to the intent and risks breaking managed installs. Deferred to an explicit
  `self-update` subcommand.
- **Background async check (rejected):** a detached thread/process that updates
  state out-of-band adds lifecycle complexity (orphaned tasks, races on the state
  file) for no real benefit at a ≥24h cadence; a bounded inline 2s check is
  simpler and safe.
- **Store timestamp in `config.toml` (rejected):** mixes churny machine state
  with user-authored/git-trackable config; risks diff noise and clobbering
  hand-edits. Hence the separate `state.toml` (D3).
