# Enterprise MDM deployment for spelunk

This directory shows how an administrator can deploy and pre-configure
`spelunk` and `spelunk-server` across a managed fleet using Mobile Device
Management (MDM). It is example configuration and documentation only; nothing
here changes how spelunk behaves at runtime.

The shared config templates and this overview are cross-platform. The
platform-specific server deployment lives in per-OS subdirectories: macOS in
[`macos/`](macos/), Windows (Intune/GPO + Windows Service) in
[`windows/`](windows/README.md).

> These templates are community examples. Test them on a single device before
> rolling out to a fleet, and adapt the paths, ports, and credentials to your
> environment.

## How spelunk reads its configuration

Knowing the configuration surface is the whole game for an MDM rollout, so it
is worth being precise. `spelunk` resolves settings in this order (later
layers win):

1. Built-in defaults.
2. The global config file `~/.config/spelunk/config.toml` (personal, per user).
   spelunk uses `~/.config` on every platform, including macOS; it does not use
   `~/Library/Application Support`.
3. The project config file `.spelunk/config.toml`, discovered by walking up from
   the working directory (team-wide, committed to the repo, no secrets).
4. Environment variables (for example `SPELUNK_SERVER_URL`, `SPELUNK_SERVER_KEY`,
   `SPELUNK_PROJECT_ID`, `SPELUNK_MODE`, `SPELUNK_NO_SERVER`).

Credentials persist in the OS keychain by default (macOS Keychain, Linux Secret
Service, Windows Credential Manager), never in the config file. The
`SPELUNK_SERVER_KEY` environment variable overrides the stored credential and is
the recommended way to push a shared API key to managed or headless hosts.

### A note on macOS managed preferences

Many macOS apps expose a managed-preferences (MCX) domain so an MDM can force
application settings through a `com.apple.ManagedClient.preferences` payload.
**spelunk does not have one.** It reads TOML files and environment variables,
not a macOS preference domain. So an MDM rollout for spelunk deploys those two
things:

- the config file (and/or environment) that pre-configures the CLI, and
- optionally, a managed `spelunk-server` daemon.

The macOS profile in this directory uses the managed-preferences payload only
for the supported, generic purpose of installing a launchd job, not to set
spelunk's own settings.

## What is in this directory

| File | Purpose |
|------|---------|
| `spelunk-config.toml` | Example global config to push to `~/.config/spelunk/config.toml`. Every key is a real field read by `spelunk`. |
| `spelunk.env.example` | Example managed environment (shared API key, server URL, fleet policy) to deliver via launchd, a profile script, or Group Policy. |
| `macos/cloud.spelunk.server.mobileconfig` | macOS configuration profile that installs a managed `spelunk-server` LaunchDaemon on a shared or always-on host. |
| `windows/` | Windows fleet guide: Intune/GPO config delivery + a PowerShell script that runs `spelunk-server` as a Windows Service. See [`windows/README.md`](windows/README.md). |

## Two deployment shapes

Most fleets are one of two shapes. Pick the one that matches you.

### Shape A: developer laptops, no shared server

Each machine runs fully self-contained. spelunk autostarts a local
`spelunk-server` on demand for inference; there is nothing to provision and no
shared state.

Steps:

1. **Install the binaries.** Package `spelunk` and `spelunk-server` for your MDM
   (a managed `.pkg` on macOS, a `.deb` on Debian/Ubuntu, or the install script
   wrapped in a script payload). Both binaries must land on the system `PATH`,
   for example `/usr/local/bin`.
2. **Optionally push fleet policy** by deploying `spelunk.env.example` values,
   for example `SPELUNK_NO_SERVER=1` on air-gapped machines or a pinned
   `SPELUNK_MODE`.

That is the whole rollout. No config file is required for the local-only case.

### Shape B: shared team memory server

A long-lived `spelunk-server` holds the team's shared memory, and every laptop
points at it. This shape uses all three files.

> **Trust model.** A `spelunk-server` instance is a single trust domain: the
> shared `SPELUNK_SERVER_KEY` is the only access boundary, and every keyholder
> is a full administrator of every project on that instance — they can list,
> read, write, supersede, archive, and delete any project's memory, not just
> their own team's. There is no per-project or per-user ACL. If you are
> deploying this server to more than one team or organisation that must not see
> each other's memory, deploy **separate server instances with separate keys**
> for each — do not put them on one shared instance and rely on project slugs
> for isolation. See
> [ADR-056](../../docs/adr/056-oss-server-tenancy-model.md).

Steps:

1. **Install the binaries** on the laptops (as in Shape A) and on the host that
   will run the shared server.
2. **Run the managed server.** A team-reachable server binds a routable
   interface and terminates HTTPS in-process (ADR-066): bring your own PEM
   certificate and private key, and set a real `SPELUNK_SERVER_KEY`. On a macOS
   server host, install `macos/cloud.spelunk.server.mobileconfig` (it installs a
   system-scoped LaunchDaemon). On Linux, deploy the team systemd unit from
   `packaging/spelunk-server-team.service` (see
   [Server setup](../../docs/server-setup.md)); `packaging/spelunk-server.service`
   is the per-developer local-inference unit, not the team server. On Windows,
   run it as a Windows Service with `windows/Install-SpelunkServerService.ps1`
   (see [`windows/README.md`](windows/README.md)).
3. **Pre-configure the laptops** so users do not have to. Push
   `spelunk-config.toml` to `~/.config/spelunk/config.toml` (server URL, project
   slug, sync mode) and deliver the shared `SPELUNK_SERVER_KEY` via the managed
   environment in `spelunk.env.example`.

After this, `spelunk memory` commands on every laptop transparently use the
shared server with no per-user setup.

## Pushing spelunk configuration

Because spelunk reads files and environment rather than a macOS preference
domain, you deploy configuration with your MDM's generic file or script
mechanism. The mechanics differ per platform and per MDM, but the targets are
always the same.

### The config file

Write `spelunk-config.toml` (edited for your environment) to each user's
`~/.config/spelunk/config.toml`.

- **macOS / Linux:** an MDM script payload that creates `~/.config/spelunk/` and
  copies the file in for each managed user. Keep the file readable only by its
  owner if it contains anything sensitive (it should not; keep secrets in the
  environment).
- **Windows:** the same `~/.config` path resolves to
  `%USERPROFILE%\.config\spelunk\config.toml` (not `%APPDATA%`); deploy the file
  with a managed script or login script. See [`windows/README.md`](windows/README.md).

A team-wide subset can instead live in a committed `.spelunk/config.toml` at the
repository root (`server_url`, `project_id`), which needs no MDM at all. Use the
global file for machine-level defaults and the project file for repo-level ones.

### The environment

Deliver the variables in `spelunk.env.example` through whatever your platform
supports:

- **macOS:** a launchd `EnvironmentVariables` payload (the server profile here
  shows the pattern), or a managed shell profile drop-in.
- **Linux:** a file under `/etc/profile.d/`, a systemd `Environment=` directive,
  or your MDM's environment payload.
- **Windows:** machine or user environment variables via Group Policy or an
  Intune configuration profile.

`SPELUNK_SERVER_KEY` is the variable to prioritise here: it carries the shared
credential and overrides the keychain, so it works on headless and freshly
imaged machines with no interactive login.

## Customising the templates

1. **macOS profile:** regenerate every `PayloadUUID` with `uuidgen`, set
   `PayloadOrganization`, confirm the `spelunk-server` binary path, and set a
   real `SPELUNK_SERVER_KEY`. Adjust `--port` and `--db` for your host.
2. **Config file:** set `server_url` and `project_id`, or delete them for the
   local-only Shape A. Uncomment the inference keys only if you run your own
   embedding/LLM endpoint.
3. **Environment file:** set the shared key and server URL, or set
   `SPELUNK_NO_SERVER=1` for an offline fleet. Do not set both a server URL and
   the offline kill-switch.

## Verifying a rollout

On a managed machine after deployment:

```bash
spelunk --version            # binaries are on PATH
spelunk status               # index + config health
spelunk context              # confirms the configured server is reachable
```

If you deployed a shared server, `spelunk context` on a laptop should surface
team memory from it. If you deployed `SPELUNK_NO_SERVER=1`, server-only features
report that they are unavailable instead of starting anything.

## Further reading

- [`windows/README.md`](windows/README.md) - the Windows fleet guide (Intune/GPO
  config delivery + running `spelunk-server` as a Windows Service).
- [Getting started](../../docs/getting-started.md) - install paths and the team
  setup walkthrough.
- [Server setup](../../docs/server-setup.md) - exposing `spelunk-server` to
  remote machines over TLS.
- [Commands reference](../../docs/commands.md) - the full list of environment
  variables and config keys.
- `packaging/spelunk-server.plist` / `packaging/spelunk-server.service` - the
  per-user LaunchAgent and the Linux systemd unit the macOS profile here is
  based on.
