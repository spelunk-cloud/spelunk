# Supported platforms & requirements

What spelunk v1 runs on, what it needs from the host system, and which
versions receive fixes.

## Prebuilt binaries

| Platform | Requirement | Notes |
|----------|-------------|-------|
| macOS (Apple Silicon) | — | Embedding runs GPU-accelerated via Metal |
| Linux x86_64 (glibc) | glibc 2.31+ (Debian 11 / Ubuntu 20.04 era or newer) | Release binaries are built in a Debian 11 container |
| Linux arm64 (glibc) | glibc 2.31+ | Same baseline as x86_64 |
| Windows x86_64 | — | `.zip` archive with `.exe` binaries |

Intel Macs are not shipped as prebuilt binaries; they build from source — see
[Building from source](building.md). Musl-based distributions (Alpine) are
untested; build from source and report what you find.

Download URLs and archive formats are listed in [Releasing](releasing.md).

## Host requirements

- **git** — spelunk shells out to `git` for memory (git-notes), worktree
  handling, and hooks. Any maintained git 2.x works; there is no exotic
  feature floor. Memory features require the project to be a git repository.
- **SQLite** — none required. SQLite and the `sqlite-vec` extension are
  bundled into the binaries; there is no system dependency.
- **Network** — none required for the core local flows. The bundled embedding
  model (~339 MB) is downloaded once on first server start; after that,
  semantic search runs entirely on-machine. Full-text search, the code graph,
  and memory work with no server and no network at all.
- **Disk** — the index lives in `.spelunk/` inside your project; expect it to
  be a fraction of the source tree's size, plus the one-time model download in
  the model cache directory.

## Version support policy

| Version | Supported |
|---------|-----------|
| Latest release | Bug fixes and security fixes |
| Older releases | Best-effort backports for critical security issues only |

See [SECURITY.md](../SECURITY.md) for how to report vulnerabilities privately.

Compatibility between the CLI and a team `spelunk-server` of a different
version: within the v1 line the server API under `/v1/` evolves additively.
Run matching versions where you can; adjacent versions are expected to
interoperate for the memory workflows.

[Version skew](version-skew.md) states the supported window per peer, what
happens outside it, and how to tell a capability the peer did not advertise
from one the CLI could not read.
