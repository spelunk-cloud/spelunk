# Getting Started

`spelunk` is a single binary. Install it, run `spelunk init` inside a git
repository, and you have working semantic search — no server to stand up, no
database to provision. A local `spelunk-server` is started for you in the
background on first use; you only think about servers when you want to *share*
memory with a team (see [Team setup](#team-setup-shared-memory-with-spelunk-server)
at the end).

## 1. Install spelunk

The recommended install paths are Homebrew (macOS/Linux), the install script,
and the Debian package (Linux). All three drop both `spelunk` and
`spelunk-server` onto your `$PATH`.

### Install script (macOS and Linux) — recommended

Detects your OS/arch, resolves the latest release tag via the GitHub API,
downloads the matching tarball, and installs both binaries to `/usr/local/bin`
(or `~/.local/bin` when not run as root):

```bash
curl -fsSL https://spelunk.cloud/install.sh | sh
spelunk --version
```

Preview what it would do without writing anything:

```bash
curl -fsSL https://spelunk.cloud/install.sh | sh -s -- --dry-run
```

### Homebrew (macOS and Linux)

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
sudo dpkg -i spelunk_<version>_amd64.deb
spelunk --version
```

### Manual tarball (any platform)

Download the tarball for your platform from the
[releases page](https://github.com/spelunk-cloud/spelunk/releases) and put both
binaries on your `$PATH`. Release tarballs are named
`spelunk-<version>-<target>.tar.gz`:

```bash
# Example: macOS (universal). Replace <version> with the release tag, e.g. v0.8.0
curl -L https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk-<version>-universal-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Verify
spelunk --version
```

Per-arch targets (`x86_64-apple-darwin`, `aarch64-apple-darwin`,
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) follow the same
pattern — swap the target in the filename. Building from source? See
[Building](building.md).

### Running spelunk-server as a service (optional)

The release artifacts include service units for keeping a local server running:
a launchd plist (`packaging/spelunk-server.plist`) for macOS and a systemd unit
(`packaging/spelunk-server.service`) for Linux. Most users don't need these —
`spelunk` autostarts the server on demand (see section 2) — but they're useful
on a shared or always-on host.
> **Intel Macs (`x86_64-apple-darwin`):** we no longer publish a prebuilt binary for
> this target — Apple deprecated the architecture, and Apple Silicon replaced it on
> new hardware six years ago. Intel Mac users should build from source instead; see
> [Building](building.md) (`cargo build --release` works unmodified on `x86_64-apple-darwin`).

> Building from source? See [Building](building.md).

## 2. Cold start: working search in under a minute

```bash
cd /path/to/your/project
spelunk init
```

That's the whole setup. `spelunk init` registers the project, parses and chunks
every source file, starts the bundled `spelunk-server` in the background when run
interactively (if one isn't already running), and embeds your code so semantic
search works out of the box:

```bash
# Search by meaning, not just text
spelunk search "where do we validate auth tokens"
```

No config file, no Docker, no external embedder. The server bundles a native
embedding model (Nomic Embed Text v1.5, via fastembed-rs); the weights are
downloaded once on first use and cached under
`~/.local/share/spelunk/models/`. There is no LM Studio or other external
inference server to run by default. The next section covers the
always-available commands that work even before you index.

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

## 3. Start using it immediately — no setup required

No configuration needed. From inside any git repository, you can immediately:

```bash
# Trace callers and callees for any symbol
spelunk graph validate_token

# Full-text search
spelunk search "error handling" --mode text

# Store a decision for your team
spelunk memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"

# List your decisions
spelunk memory list --kind decision
```

Memory is stored in git notes — no server, no database, no configuration.

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

## Server mode vs no-server mode

Everything spelunk does falls into one of two tiers, decided at runtime by
whether a `spelunk-server` is reachable. You don't choose a tier — spelunk picks
the best one available.

| | **No-server mode** | **Server mode** |
|---|---|---|
| When | No server reachable (or `SPELUNK_NO_SERVER=1`) | A server is running — usually the local one started for you |
| Search | text + AST (`--mode text`, `--mode ast-grep`) | + semantic / hybrid search by meaning (`--mode auto`/`semantic`) |
| Memory add/list/show | git-notes + local SQLite | same (or a shared server, if configured) |
| `spelunk explore` | unavailable | available (server runs the LLM loop) |
| Team memory sync | — | `spelunk memory push` / `spelunk sync` to a shared server |

In v0.8.0 the local server is **autostarted in the background** the first time
you run a command that needs it (e.g. `spelunk init` or a semantic
`spelunk search`), so most users are in server mode without doing anything. The
always-available commands in section 4 work in either mode.

To stay fully offline (CI, air-gapped, or you just don't want a background
process), set `SPELUNK_NO_SERVER=1` — spelunk then runs in no-server mode and
locked features exit with a clear message instead of starting anything.

For how discovery works and how to point the CLI at a remote server, see
**[Server setup](server.md)** and
[CLI capability tiers](architecture/capability-tiers.md).

### Using your own inference server (advanced)

By default the bundled `spelunk-server` provides embeddings (native, via
fastembed-rs) and — when a chat model is configured — LLM inference. If you'd
rather have spelunk talk directly to your own OpenAI-compatible endpoint (e.g.
LM Studio on port `1234`, Ollama on `11434`, or a vLLM proxy) instead of the
native embedder, point it there in `~/.config/spelunk/config.toml`:

```toml
# ~/.config/spelunk/config.toml

# OpenAI-compatible endpoint (default: http://127.0.0.1:1234)
api_base_url = "http://127.0.0.1:1234"

# Must match your endpoint's model identifier
embedding_model = "text-embedding-embeddinggemma-300m-qat"

# Embedding batch size (tune if you run out of memory)
batch_size = 32
```

This is an advanced override; most users never set it — the native embedder in
`spelunk-server` handles embeddings with no extra configuration.

### Index your project for semantic search

`spelunk init` (section 2) already indexes and embeds your project against the
local server. If you've pointed spelunk at your own inference server above, run
it again so embeddings are generated through that endpoint:

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
spelunk initialised for my-project

  Index:   142 files, 1 840 chunks
  DB:      ~/.local/share/spelunk/my-project.db
  Embeddings: 1 840 vectors
```

**Subsequent runs** only re-index changed files (via blake3 hash):

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
- [Self-hosting](self-hosting.md) — exposing spelunk-server to remote agents over TLS
- [Building from source](building.md) — for contributors and platform builders

---

## Team setup: Shared memory with spelunk-server

Working with a team? Point everyone at a shared `spelunk-server` so they share decisions, requirements, and context instead of siloing them locally. This is a *different* server from the local one spelunk autostarts for inference — it's a long-lived, deployed instance with an API key.

Each team member's code stays local — only memory travels to the server.

### Quick setup

Add `.spelunk/config.toml` at your repo root (commit it):

```toml
# .spelunk/config.toml — commit this, no secrets
server_url = "http://spelunk.internal:7777"
project_id = "my-awesome-app"
```

Each developer adds the API key to their personal config:

```toml
# ~/.config/spelunk/config.toml — never commit this
server_key = "your-shared-api-key"
```

> The older `memory_server_url` / `memory_server_key` keys are still accepted as
> deprecated aliases for `server_url` / `server_key`.

`project_id` stays a human-readable slug. If the server routes projects by an
internal UUID (as a team/cloud memory server does), the CLI resolves the slug
for you on first use and caches the result locally, so no manual UUID lookup is
needed. See [Server setup](server.md#client-configuration) for details.

After setup, all `spelunk memory` commands transparently use the server. To migrate existing local memories:

```bash
spelunk memory push
```

For full setup and deployment guide: **[Server setup](server.md)** — Docker, configuration, API reference.
