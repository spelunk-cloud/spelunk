# spelunk documentation

> git tracks what changed. spelunk remembers why.

spelunk helps you understand an unfamiliar codebase fast, then remembers the
decisions behind it so the next session does not re-derive them. These docs
follow the path a new user takes, from the first five minutes to running a
shared memory server for a team. Read them in order the first time; use the
reference (stage 4) for lookup afterwards.

## 1. On-ramp (first five minutes)

Understand how an unfamiliar codebase fits together, with zero infrastructure.
Install the binary, run `spelunk init`, and the first `graph` / `search` /
`context` already trace how a symbol connects, find the code behind a concept,
and assemble the context around a change. This is fast understanding (how,
where, what), not a faster grep, and it needs no server.

- [README quick start](../README.md#quick-start): the install one-liner and three commands that work immediately
- [Getting Started → install](getting-started.md#1-install-spelunk): script, Homebrew, `.deb`, or tarball
- [Getting Started → first index and retrieval](getting-started.md#2-cold-start-index-and-get-your-first-answer): `init`, `index`, and your first answer
- [Example: onboarding a new codebase](examples/onboarding-a-new-codebase.md): a full first-session walkthrough

## 2. Getting started (the happy path)

Next, make that understanding stick. The core loop runs end to end on built-in
storage (git-notes memory, full-text and code graph, no daemon): you record your
first decision by hand, and a later `spelunk context` hands it back, so the same
context is not re-derived next time. From there, one step up brings in the local
semantic server for search by meaning. That server is an inference backend only:
it embeds queries and runs summaries, and it never stores memory. Your memory
always lives in the project's local `memory.db`.

- [Getting Started](getting-started.md): the core loop and the local semantic tier
- [Memory](memory.md): decisions, requirements, and context; supersede, do not delete

## 3. Configure your agent

This is where the payoff lands. If you code with an AI agent, you connect it to
spelunk once and the why-layer starts filling itself: as the agent works, the
reasoning behind each change is captured for you, with no time set aside to sit
down and write it up. The decisions a fresh repo could not yet show in stage 1
now accumulate on their own, and every later `spelunk context` or `spelunk
search` hands them back.

The mechanism is the agent itself. Wired to spelunk through a skill (the Claude
Code skill, or a drop-in `AGENT.md`), it records each decision as it makes it, so
the why-layer accrues as a by-product of the work rather than from anyone stopping
to document it. A git hook complements this: a post-commit step runs
`spelunk memory harvest` to catch any reasoning left in commit messages, so
nothing slips through. Nothing else about how you work has to change.

- [Agent Guide](agent-guide.md): how a session should use spelunk, plus automatic capture and JSON output
- [AGENT.md template](examples/AGENT.md): a drop-in file that tells your agent to reach for spelunk first
- [Claude Code skill](../SKILL.md): spelunk packaged as an agent skill
- Automatic capture: [`spelunk hooks install`](commands.md#spelunk-hooks) plus [`spelunk memory harvest`](commands.md#spelunk-memory)

## 4. Reference

Look up exact behaviour once you have the mental model. Every shipped command is
documented and verified against the binary; reference lives after the journey, not
before it.

- [Commands](commands.md): every subcommand, flag, and environment variable
- [Config reference](config-reference.md): every field in `config.toml`, with defaults and env overrides
- [Memory model](memory.md): kinds, cross-project visibility, git-notes write-through
- [Architecture](architecture.md) and [capability tiers](architecture/capability-tiers.md)
- [Plumbing and porcelain](plumbing-and-porcelain.md): JSONL commands for scripts and agents
- [Stability contract](stability.md): what a version bump may change, per surface, and what it may not
- [Security](security/THREAT-MODEL.md): threat model and boundaries (secret scanning is defense-in-depth, not a boundary)

## 5. Local vs server vs team-server

Three tiers, and it helps to understand why each one exists.

Everything works with just the binary. The **local server** is a convenience on
top of that: it keeps the embedding model loaded and running in the background,
so each semantic `search` or `explore` is fast instead of reloading a few hundred
megabytes of model on every command, and your context is ready every time rather
than being rebuilt on each invocation. That is all it does. It stores no memory
of its own, and it listens only on your own machine (loopback), so it is not
reachable from anywhere else.

Sharing memory is a separate, deliberate step. To give a whole team one source of
truth, you run a **team server** and point everyone at it. The team server shares
your memory index, the decisions and requirements behind the code, not the code
itself; each person's code stays on their own machine. It is the same
open-source server, free to self-host. If you would rather not run one yourself,
the hosted spelunk.cloud service is the managed alternative.

| Tier | What it adds | Where memory lives |
|---|---|---|
| Built-in (zero infra) | git-notes memory, full-text and ast-grep search, code graph | local `memory.db` |
| Local semantic server (auto-started on loopback) | faster semantic search, `explore`, summaries | still local `memory.db`: inference only, never a memory store |
| Team memory server (explicit `server_url`) | one shared memory index for the team | the shared server: the only path off the local machine |

Everyone on a team sets an explicit `server_url` (plus a shared server key)
pointing at the same server, and [`spelunk sync`](commands.md#spelunk-sync) keeps
them converged: it pushes the decisions you recorded and pulls the ones your
teammates recorded. Code never travels; it does not need to, you already have git
for that. Only memory does.

- [Getting Started → capability tiers](getting-started.md#capability-tiers-where-inference-and-memory-live): the tier table in context
- [Getting Started → team setup](getting-started.md#team-setup-shared-memory-with-spelunk-server): how to set `server_url` and sync
- [Server setup](server-setup.md): deploy and expose a team server (Docker, systemd, TLS, client config)
- [Remote agents](remote-agents.md): run an agent in a container against your server

---

Contributing? See [Building from source](building.md).
