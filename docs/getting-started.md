# Getting Started

`spelunk` is a single binary that helps you understand an unfamiliar codebase
fast: trace how a symbol connects across files, find the code behind a concept,
and assemble the context around a change, all from the CLI with no infrastructure
to stand up. Install it, run `spelunk init` inside a git repository, and the first
`graph` / `search` / `context` already tell you how the code fits together.

That is the starting point. As you keep working, `spelunk` also remembers the
decisions behind the code, so a later session (yours or a teammate's) does not
re-derive them. A local `spelunk-server` is started for you on first use to add
search by meaning; you only think about a shared server when you want to share
that memory with a team (see
[Team setup](#team-setup-shared-memory-with-spelunk-server) at the end).

## 1. Install spelunk

### Windows

#### Install script (PowerShell) — recommended

The PowerShell install script resolves the latest release, downloads the
Windows `.zip`, and installs `spelunk.exe` and `spelunk-server.exe` to
`%LOCALAPPDATA%\Programs\spelunk\`. It also adds that directory to your user
`PATH` automatically.

Open PowerShell and run:

```powershell
irm https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.ps1 | iex
spelunk --version
```

Preview what it would do without writing anything:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.ps1))) -DryRun
```

#### Scoop

The repo doubles as a [Scoop](https://scoop.sh) bucket, so `scoop` installs and
updates `spelunk.exe` and `spelunk-server.exe` from the release `.zip` and keeps
them current with `scoop update`:

```powershell
scoop bucket add spelunk https://github.com/spelunk-cloud/spelunk
scoop install spelunk
spelunk --version
```

#### Manual `.zip` download

Download the `.zip` for your platform from the
[releases page](https://github.com/spelunk-cloud/spelunk/releases). The Windows
archive is named `spelunk-<version>-x86_64-pc-windows-msvc.zip`. Extract it and
place `spelunk.exe` and `spelunk-server.exe` anywhere on your `PATH`
(e.g. `C:\Users\<you>\bin\`).

> **winget:** deferred, available on request. Track the
> [releases page](https://github.com/spelunk-cloud/spelunk/releases) or the repo
> for updates.

---

### macOS and Linux

The recommended install paths are Homebrew (macOS/Linux), the install script,
and the Debian package (Linux). All three drop both `spelunk` and
`spelunk-server` onto your `$PATH`.

#### Install script (macOS and Linux) — recommended

Detects your OS/arch, resolves the latest release tag via the GitHub API,
downloads the matching tarball, and installs both binaries to `/usr/local/bin`
(or `~/.local/bin` when not run as root):

```bash
curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh
spelunk --version
```

Preview what it would do without writing anything:

```bash
curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh -s -- --dry-run
```

#### Homebrew (macOS and Linux)

```bash
brew install spelunk-cloud/spelunk/spelunk
spelunk --version
```

### Debian / Ubuntu (`.deb`)

The release pipeline publishes an `amd64` `.deb`. Substitute the release version
for `<version>` (e.g. `0.8.0`). The download path is pinned to the release tag
(`v<version>`) so the versioned filename always resolves — the version-free
`releases/latest/download/…` form 404s on a versioned asset name (see #340):

```bash
curl -fsSLO https://github.com/spelunk-cloud/spelunk/releases/download/v<version>/spelunk_<version>_amd64.deb
sudo apt install ./spelunk_<version>_amd64.deb
spelunk --version
```

> Install with `apt`, not `dpkg -i`. The package declares the shared libraries
> the binaries link against (including `libdbus-1-3`); `apt` pulls those in,
> whereas `dpkg -i` does not resolve dependencies and leaves the package
> unconfigured on a machine that lacks them. The leading `./` is required, or
> `apt` treats the argument as a package name instead of a file.

### Manual tarball / zip (any platform)

binaries on your `$PATH`. Supported targets:

| Platform | Archive name | Notes |
|----------|-------------|-------|
| macOS (Apple Silicon) | `spelunk-<version>-aarch64-apple-darwin.tar.gz` | |
| Linux x86_64 | `spelunk-<version>-x86_64-unknown-linux-gnu.tar.gz` | Requires glibc 2.31 (Debian 11 Bullseye or newer / Ubuntu 20.04 or newer); on minimal images, install `libdbus-1-3` |
| Linux ARM64 | `spelunk-<version>-aarch64-unknown-linux-gnu.tar.gz` | Requires glibc 2.31 (Debian 11 Bullseye or newer / Ubuntu 20.04 or newer); on minimal images, install `libdbus-1-3` |
| Windows x86_64 | `spelunk-<version>-x86_64-pc-windows-msvc.zip` | |

> **Intel Macs (`x86_64-apple-darwin`):** prebuilt binaries are not published for
> this target — Apple deprecated the architecture and Apple Silicon replaced it on
> new hardware six years ago. Intel Mac users must build from source; see
> [Building from source](building.md) (`cargo build --release` works unmodified on
> `x86_64-apple-darwin`).

```bash
# Example: macOS Apple Silicon. Replace <version> with the release tag, e.g. v0.9.0
curl -L https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk-<version>-aarch64-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Verify
spelunk --version
```

Swap the target in the filename for another platform. Building from source? See
[Building from source](building.md).

### Running spelunk-server as a service (optional)

The release artifacts include service units for keeping a local server running:
a launchd plist (`packaging/spelunk-server.plist`) for macOS and a systemd unit
(`packaging/spelunk-server.service`) for Linux. Most users don't need these —
`spelunk` autostarts the server on demand (see section 2) — but they're useful
on a shared or always-on host.

## 2. Cold start: index and get your first answer

```bash
cd /path/to/your/project
spelunk init
```

That's the whole setup. `spelunk init` registers the project, starts the bundled
`spelunk-server` in the background when run interactively (if one isn't already running),
parses and chunks every source file, and hands the embedding pass to a detached background
worker so the prompt returns after parsing rather than after the full embed pass. Embeddings build in the background; full-text and ast-grep search
work immediately, and semantic search becomes available as embeddings land.

```bash
# Search by meaning, not just text
spelunk search "where do we validate auth tokens"
```

`init` also writes `.spelunk/.gitignore` so the machine-specific SQLite
(`index.db*`, `memory.db*`) stays out of version control, and records the
project slug as `project_id` in `.spelunk/config.toml`. The slug defaults to the
git-derived identity (`host/owner/repo` when an `origin` remote exists, else
`local/<blake3-hex>` of the path); pass `spelunk init --name <slug>` to set an
explicit one for a repo without a remote. `.spelunk/config.toml` stays tracked,
since it is meant to be committed and shared, so the whole team resolves to one
project identity. An existing `project_id` or `.spelunk/.gitignore` is never
overwritten, so re-running `init` is safe.

No config file, no Docker, no external embedder. The server bundles a native
embedding model (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS
via candle); a pre-quantized Q8_0 GGUF (~339 MB) is downloaded once on first use
and cached under `~/.local/share/spelunk/models/`. No LM Studio or other
external inference server is needed. The next section covers commands
that work even before you index.

You can manage the background server explicitly if you want:

```bash
spelunk server start     # start the local daemon (idempotent; auto-binds 127.0.0.1)
spelunk server status    # PID, port, instance id, uptime
spelunk server logs      # last 50 lines of the server log
spelunk server stop      # stop the daemon
```

In non-interactive contexts (CI, agent harnesses) `spelunk init` does **not**
auto-spawn the server — run `spelunk server start` first if you want semantic
search there, or set `SPELUNK_NO_SERVER=1` to stay fully offline.

## 3. Start using it inside your project

`graph` and `search --mode ast-grep` need no index and run in any repository.
Memory and `context` operate on the local project you created with `spelunk init`
(step 2); in an un-initialized directory they fail closed with a `no spelunk
project here` error instead of using a machine-global store. From inside your
project you can:

```bash
# Find the code behind a concept: search takes any phrase, no symbol name needed
spelunk search "error handling" --mode text

# Trace how a symbol connects. graph resolves an exact identifier (a real symbol
# name, e.g. one you just saw in the search results above), not a concept phrase
spelunk graph validate_token

# Store a decision for your team
spelunk memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"

# List your decisions
spelunk memory list --kind decision
```

Memory is stored locally in the project's `.spelunk/memory.db` (and mirrored to
git notes by default), so it travels with the repo. No server or database
service to run.

## 4. Start an agent session

When your agent or team is starting a new coding session, pull all relevant context in one command:

```bash
# Agent entry point — pulls decisions, requirements, questions, handoffs
spelunk context

# Filter by kind
spelunk context --kind decision

# Get JSON for machine processing
AGENT=true spelunk context
```

The default output is compact (a few recent entries per section). Pass
`--budget <N>` (alias `--max-tokens`) to cap total output at N tokens, or
`--limit <N>` to widen the per-section count.

## 5. Set up automatic memory harvesting (optional)

Install a git post-commit hook so `spelunk` automatically extracts memories from commit messages:

```bash
spelunk hooks install
```

Other developers without `spelunk` installed are unaffected. To remove:

```bash
spelunk hooks uninstall
```

---

## Capability tiers: where inference and memory live

spelunk works at three tiers; the team-memory tier can be a server you host
yourself or the managed spelunk.cloud, shown as separate rows below. You do not
pick one by hand; spelunk uses the best one available and degrades cleanly when a
server is not reachable. The load-bearing distinction is that a **local server
does inference only and never stores memory**. Your memory always lives in the
project's local `memory.db` until you *explicitly* configure a team server or use
the managed spelunk.cloud.

| Tier | What runs it | What it adds | Where memory lives |
|---|---|---|---|
| **Built-in** (zero infra) | just the `spelunk` binary | git-notes memory, full-text and ast-grep search, code graph | local `memory.db` |
| **Local semantic server** | a loopback `spelunk-server`, auto-started on demand | semantic / hybrid `search`, `explore`, LLM summaries | still local `memory.db`: the server is **inference only, never a memory store** |
| **Team memory server** | a shared `spelunk-server` you deploy, set via an explicit `server_url` | shared memory across the team | the shared server you run: memory leaves your machine, your code stays local |
| **spelunk.cloud** (hosted) | a managed service: nothing to deploy or maintain | the same shared-team memory as a self-hosted server, without running one | the hosted service: memory leaves your machine, your code stays local |

Built-in works with nothing installed but the binary (the always-available
commands in section 3). The local semantic server is auto-discovered on loopback
(`127.0.0.1`) and started for you the first time a command needs it; it embeds
queries and runs LLM calls, but a project's memory stays in `memory.db`
regardless of whether it is running. Memory moves off the local machine only when
you point at a team server, self-hosted via an explicit `server_url` or the
managed spelunk.cloud (see
[Team setup](#team-setup-shared-memory-with-spelunk-server)); each developer's
code still stays local.

To stay fully offline (CI, air-gapped, or you just don't want a background
process), set `SPELUNK_NO_SERVER=1`: spelunk then runs built-in only, and
inference-only commands exit with a clear message instead of starting anything.

For how discovery works and how to point the CLI at a remote server, see
**[Server setup](server-setup.md)** and
[CLI capability tiers](architecture/capability-tiers.md).

### Using your own LLM endpoint (advanced)

By default the bundled `spelunk-server` provides embeddings (native, via the
candle-served F2LLM-v2-330M model, 896-dim) and, when a chat model is
configured, LLM inference. The embedding **model and its compute path are
both fixed** product-wide: `spelunk` always embeds through the bundled native
embedder, and there is no way to relocate or swap it. LLM inference is
different: the server has no LLM of its own, so you point it at your own
OpenAI-compatible chat-completions endpoint. Configure **the server**: this
is not a CLI `config.toml` key. `spelunk-server` reads these environment
variables (each has an equivalent flag):

| Variable | Flag | Purpose |
|---|---|---|
| `SPELUNK_LLM_URL` | `--llm-url` | Base URL of an OpenAI-compatible chat-completions endpoint for LLM features (`explore`, summaries, `memory harvest`). |
| `SPELUNK_LLM_MODEL` | `--llm-model` | Chat model id to send to that endpoint. |

`explore` and `memory harvest` pick up an LLM-configured local daemon
automatically. Index-time chunk summaries are the exception: they additionally
need an *explicit* `server_url` in `.spelunk/config.toml` (even a loopback one),
not just a reachable server: see
[Third-party models](third-party-models.md#what-this-unlocks) for the full
absence-behavior and the team-server equivalent.

For the auto-started local daemon, export the variables and then restart the
server so it picks them up. The daemon inherits your shell environment, but a
daemon that is already running keeps its old configuration until restarted:

```bash
export SPELUNK_LLM_URL="http://127.0.0.1:1234"
export SPELUNK_LLM_MODEL="your-chat-model-id"

spelunk server stop     # if one is already running
spelunk server start    # starts with the endpoint configured above
```

Or, if you run `spelunk-server` yourself, pass the flags directly:

```bash
spelunk-server --llm-url http://127.0.0.1:1234 --llm-model your-chat-model-id
```

This is an advanced override; most users never set it: `explore`, summaries,
and `memory harvest` are simply unavailable without an LLM configured, and
semantic search works regardless since the native embedder needs no
configuration at all.

### Index your project for semantic search

`spelunk init` (section 2) already indexes and embeds your project against the
local server. If you've configured a custom embedding endpoint above, restart
the server and run `init` again so chunks are embedded through that endpoint:

```bash
cd /path/to/your/project
spelunk init
```

This:
1. Registers your project in the global registry
2. Parses every source file and indexes chunks
3. Embeds chunks using your configured server
4. Stores everything in `~/.local/share/spelunk/<project-slug>.db`

Output:
```
spelunk initialised for github.com/acme/my-project

  Index:   142 files, 1 840 chunks
  DB:      ~/.local/share/spelunk/my-project.db
  Project: github.com/acme/my-project  (written to .spelunk/config.toml)
  Embeddings: 1 840 vectors
```

**Subsequent runs** only re-index changed files (via blake3 hash), and also
backfill embeddings for any chunk that was parsed but never embedded (for
instance if the embedder model was still loading on the first run):

```bash
spelunk index /path/to/your/project
```

Force a full re-index after changing the embedding model:

```bash
spelunk index /path/to/your/project --force
```

### Use semantic search

```bash
# Finds code by concept, not just text
spelunk search "error handling in the HTTP layer"

# Hybrid search (semantic + full-text)
spelunk search "authentication" --mode hybrid

# Expand with 1-hop call graph
spelunk search "authentication" --graph

# Fit results within a token budget for agents
spelunk search "database layer" --budget 4000

# Machine-readable output
spelunk search "database migrations" --format json
```

### Check index health

```bash
spelunk status                              # index statistics
spelunk check                               # verify index is up to date
spelunk check --format porcelain --files    # list files that need re-indexing
```

---

## Next steps

- [Commands reference](commands.md) — every flag and option
- [Memory](memory.md) — storing project context across sessions
- [Agent Guide](agent-guide.md) — using `spelunk` with AI coding agents
- [Remote agents](remote-agents.md) — running an agent in a Docker container against your local server
- [Server setup](server-setup.md): exposing spelunk-server to remote agents over TLS
- [Building from source](building.md) — for contributors and platform builders

---

## Team setup: Shared memory with spelunk-server

Working with a team? Point everyone at a shared `spelunk-server` so they share decisions, requirements, and context instead of siloing them locally. This is a *different* server from the local one spelunk autostarts for inference — it's a long-lived, deployed instance with an API key.

Each team member's code stays local — only memory travels to the server.

### Quick setup

Add `.spelunk/config.toml` at your repo root (commit it):

```toml
# .spelunk/config.toml — commit this, no secrets
server_url = "https://spelunk.internal.example.com"
project_id = "my-awesome-app"
```

> **`server_url` must be `https://` unless it points at loopback**
> (`127.0.0.1` / `::1` / `localhost`) — a non-loopback `http://` URL is
> rejected at startup, with no opt-out, because the CLI attaches your bearer
> token to these requests. See [Server setup](server-setup.md) for putting TLS
> in front of a deployed server.

Each developer provides their own key with `spelunk auth set-key`, scoped to
this server's URL:

```bash
spelunk auth set-key --server https://spelunk.internal.example.com
```

The key is stored in your OS keychain (macOS Keychain, Linux Secret Service,
Windows Credential Manager) rather than in plaintext, keyed by the server's
origin so a second project's server key never collides with this one. For CI /
headless use, the `SPELUNK_SERVER_KEY` environment variable works everywhere
and takes precedence over the stored key:

```bash
export SPELUNK_SERVER_KEY="your-shared-api-key"
```

If you have an old personal `~/.config/spelunk/config.toml` with a bare
`server_key = "..."`, it is picked up and migrated into the per-server store
automatically the first time it's needed; no action required. A `server_key`
line in a project's checked-in `.spelunk/config.toml` is no longer read at all,
so remove it if one is still there. On a host with no keychain, spelunk falls
back to an owner-only `~/.config/spelunk/secrets.toml`. For the full
credential-storage rules and the `SPELUNK_SECRET_STORE` override, see the
[Commands reference](commands.md#spelunk-auth).

`project_id` stays a human-readable slug, and it is sent to the server exactly
as configured. Both a self-hosted spelunk-server and the hosted cloud API accept
either a slug or a UUID as the project key, so nothing is looked up and nothing
is cached. See [Server setup](server-setup.md#client-configuration) for details.

After setup, all `spelunk memory` commands transparently use the server. Seed it
with your existing local memory, then keep recording decisions as usual:

```bash
spelunk memory push    # one-way: seed the server with your existing local entries
spelunk sync           # force a synchronous two-way reconcile (usually not needed; see below)
```

In the default `local_first` mode you rarely run `spelunk sync` by hand. Your
writes commit to the local `memory.db` immediately and never block on the
network; from an interactive terminal a background reconciler then drains what
you recorded up to the server and pulls teammates' entries down, so the shared
memory converges on its own. `spelunk sync` is the explicit escape hatch for
when you want that reconcile to happen synchronously now rather than in the
background, such as a CI job that needs entries pushed before it exits. Code
never travels; only memory does.

For full setup and deployment guide: **[Server setup](server-setup.md)**: Docker, configuration, API reference.

### Enterprise / MDM deployment

Rolling spelunk out to a managed fleet? The
[`examples/mdm/`](../examples/mdm/README.md) directory shows how to deploy and
pre-configure `spelunk` and `spelunk-server` via MDM (managed config file,
fleet-wide environment, and a macOS profile for a managed server daemon),
grounded in spelunk's real config surface.
