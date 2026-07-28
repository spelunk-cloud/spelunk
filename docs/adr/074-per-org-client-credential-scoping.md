# ADR-074: Per-organization scoping of the WorkOS client credential

**Date:** 2026-07-18
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** the WorkOS-analogue of
[ADR-071](071-per-server-client-bearer-scoping.md), which gave the
self-hosted `server_key` bearer a per-origin home in the secret store. This
ADR asks the same question of the cloud/WorkOS credential: `spelunk login`
and `spelunk org switch` currently manage a single stored session, and a
developer who legitimately holds membership in more than one organization
(the consultant / multi-client case) cannot represent more than one of them
at once. It adopts ADR-071's "one secret-store entry, not one per key"
shape where the mechanics agree, and departs from it where WorkOS's
refresh-token semantics genuinely differ from a static bearer string (see
D1 and D3).

## Context

### One stored session, and switching mutates it in place

`spelunk login --org <slug>` and `spelunk org switch <slug>` both end by
calling WorkOS's `/user_management/authenticate` refresh grant and writing
the result over the single `[auth]` table in the personal
`~/.config/spelunk/config.toml`
(`crates/spelunk-core/src/config/mod.rs`, `AuthTokens { access_token,
refresh_token, expires_at, org_id }`, persisted by
`crates/spelunk-core/src/config/persist.rs::save_auth_tokens`). Every
resolution path, `Config::load`'s bearer precedence, `ensure_fresh_token`'s
proactive refresh, and `switch_org`'s org resolution, reads and writes this
one entry (`crates/spelunk-cli/src/cli/cmd/org.rs`,
`crates/spelunk-cli/src/cli/cmd/auth_api.rs`).

That is fine for one operator working one organization at a time. It breaks
down for a consultant or an agent-running operator serving two clients: to
touch client B's data, they run `org switch <client-b>`, which overwrites
the one `[auth]` entry with client B's tokens. Any other process, in
particular a long-running agent mid-task that was scoped to client A, either
holds a still-valid short-lived access token for a few more minutes and then
fails to refresh (the stored refresh token it would need is gone, replaced
by client B's), or, worse, re-reads the config fresh on its next invocation
and silently proceeds as client B. Either way, the switch is destructive to
concurrent work that was not part of the switch.

### The repo has no way to pin an org either

`.spelunk/config.toml` (the committed, project-level config) carries
`server_url`, `project_id`, and `server_ca`
(`crates/spelunk-core/src/config/mod.rs::ProjectConfig`), but nothing that
says which cloud organization this repo belongs to. A consultant with
separate repos per client has no way to make "an invocation inside
client-a's repo always resolves client-a's org," so correctness currently
depends on the operator remembering to `org switch` before every session in
every repo, the same discipline problem ADR-071 found in
`SPELUNK_SERVER_KEY` juggling, one layer up.

### Why a flat per-org string map (ADR-071's shape) is not quite enough

ADR-071's `server_keys` map works because a `server_key` is a static bearer
string with no lifecycle: store it, read it back, done. A WorkOS session is
not that. Each entry needs its own `access_token` / `refresh_token` /
`expires_at` and its own independent refresh lineage, and WorkOS refresh
tokens are documented as single-use: exchanging one invalidates it and
returns a new one. Reusing ADR-071's flat `map<key, string>` verbatim would
have no way to express "this org's token is stale, refresh it," and a naive
per-org design that always forks a new org's tokens from whatever the
*globally active* refresh token currently is would reintroduce the exact
destructive-switch problem this ADR exists to remove: forking consumes the
source token, so if the source belongs to a different org's slot than the
one being onboarded, that other org's session dies in the process. The
decision below is shaped to avoid ever needing to fork from a *different*
org's live token once that org has been onboarded once.

## Decision

**Cache one full `AuthTokens` tuple per organization in a single secret-store
entry (D1); let `.spelunk/config.toml` pin a repo to one of them (D2);
resolve and refresh strictly within one org's own entry, never by touching a
sibling (D3); and make `org switch` / `logout` operate on that cache instead
of the flat legacy session (D4).**

### D1 – one secret-store entry holding a per-org token map

A single new secret-store entry, `(service = "spelunk", user =
"org_tokens")`, whose payload is one JSON object:

```json
{
  "active": "org_01H...",
  "orgs": {
    "org_01H...": { "access_token": "...", "refresh_token": "...", "expires_at": 1737400000 },
    "org_02K...": { "access_token": "...", "refresh_token": "...", "expires_at": 1737401500 }
  }
}
```

keyed by WorkOS org id (the same id `org switch` already resolves slugs and
local UUIDs to; see `resolve_workos_org_id` in
`crates/spelunk-cli/src/cli/cmd/org.rs`). `active` names which entry backs an
invocation that has no other way to pick one (D2's fallback). One entry, not
one per org, for the same reason ADR-071 D1 gives: a distinct keychain item
per org would re-prompt for access on every new client, and one item, once
granted, covers all of them.

The `SecretStore` trait is unchanged, exactly as ADR-071 leaves it: this is
still an opaque `get`/`set`/`delete`, with the JSON shape and org-keying
living in the config layer above it.

### D2 – `.spelunk/config.toml` gets an `org` field

```toml
org = "client-a"
```

added to `ProjectConfig` and `Config` alongside `project_id`, accepting the
same forms `org switch` already accepts (a WorkOS org id, a slug, or a local
org UUID, resolved the same way `resolve_workos_org_id` does today). Not a
secret: an org slug is exactly as shareable as the `project_id` it sits next
to, so it belongs in the committed file, unlike a credential.

Precedence, highest first, mirroring the existing `server_url` /
`project_id` override chain:

1. `--org` (an explicit per-invocation flag, where a command already
   accepts one, e.g. `spelunk login --org`),
2. `SPELUNK_ORG` environment variable,
3. `.spelunk/config.toml`'s `org` (project-level, committed),
4. the cache's `active` pointer (D1), i.e. today's implicit single-org
   behavior, unchanged for an operator who has only ever used one
   organization.

A repo that pins `org` is scoped to that org for every invocation inside it
regardless of whatever `active` currently points to elsewhere on the same
machine, closing the context-bleed case Q1 describes.

### D3 – resolution and refresh never leave one org's entry

Given the org resolved by D2's precedence, bearer resolution:

1. looks up that org's entry in the D1 cache;
2. if present and unexpired, uses it directly, no network call;
3. if present but expired, refreshes it by sending **that entry's own
   `refresh_token`** to WorkOS with `organization_id` set to **the same
   org**, a same-org refresh, and writes the rotated pair back into that
   same slot; no other org's entry is read or written;
4. if absent (this org has never been cached on this machine), it is
   onboarded by one of two paths, and the choice matters for concurrency,
   not just convenience (see the spike below): the **recommended** path is
   an explicit `spelunk login --org <target>`, an independent WorkOS
   device-authorization grant that depends on no existing token at all and
   mints its own session, so it never touches whatever org is currently
   `active`. The **fallback** path is today's existing fork, refreshing
   whatever's currently `active` with the new `organization_id`, which
   mints tokens for the target org but, since WorkOS refresh tokens are
   single-use, costs the previously-`active` org its current token in the
   process. Either way the cost is paid once, at first encounter with a new
   org, not on every subsequent switch between two already-cached orgs,
   which is the actual pattern the consultant case complains about, but only
   the independent-login path leaves the previously-active org's session
   untouched while onboarding the new one.

This is the property that answers Q2: once two orgs both have entries in the
cache, moving between them never touches the other's entry, so a
long-running agent scoped to org A is unaffected by a switch to org B that
happens elsewhere on the same machine, as long as org A was already onboarded
before that switch.

**Spike (2026-07-19): does WorkOS actually let two orgs be independently
refreshable, or is there only one refresh lineage per user?** Before this
ADR merged, a review comment raised exactly this doubt: WorkOS might tie
refresh tokens to a single per-user lineage rather than a per-session one, in
which case refreshing org A would silently cost org B's token too, and D3's
"never forking from a sibling" property would not hold in practice. Checked
against WorkOS's published API reference:

- The refresh grant
  (`POST /user_management/authenticate`, `grant_type=refresh_token`) accepts
  an optional `organization_id` used to select which org's authorization the
  new access token carries; if omitted, the existing scope is kept. Refresh
  tokens are documented as single-use and rotating: each successful refresh
  returns a **replacement** refresh token, and the used one stops working.
  This confirms the "single-use, rotating" premise D1's rationale already
  assumes, and it confirms the *fork* mechanism in D3.4 (refresh org A's
  token while asking for org B's `organization_id`) really does consume org
  A's token to mint org B's, exactly as D3.4 already documents as a one-time,
  not-glossed-over cost.
- WorkOS's session-listing endpoint
  (`GET /user_management/users/{id}/sessions`, listing "all active sessions
  for a specific user") returns a **set** of sessions
  per user, each with its own `id`, `organization_id`, `auth_method`, and
  timestamps, individually revocable by the session id carried in an access
  token's `sid` claim. A user is not limited to one active session; each
  successful `authenticate()` call (a fresh device-authorization exchange,
  in particular) mints its own session, and the refresh grant's own
  documentation ties a refresh's success to "the session" (singular) behind
  the token being refreshed, not to the user account as a whole.

Read together, these confirm D3's assumption **for the specific onboarding
path this ADR already recommends first in D3.4**: an independent
`spelunk login --org <target>` (its own device-authorization exchange, no
existing token involved) mints a session, and therefore a refresh lineage,
that is distinct from any other org's session for the same user, so
refreshing it neither reads nor rotates a sibling org's token. The doubt
raised in review is correct about the *fork* path specifically (D3.4's
second, explicitly-costed option), not about the design as a whole: forking
shares one lineage and pays for it once; independent per-org login does not
share a lineage and is what makes ongoing, side-effect-free switching between
already-cached orgs possible. Nothing here required changing D1 through D4;
it sharpens which of D3.4's two onboarding paths actually earns the
no-cross-talk property the rest of the ADR relies on. Implementation should
include a live-WorkOS check (two independent `login --org` exchanges for the
same test user, then a refresh of each) alongside the local-cache simulation
already called for, since that specific cross-session claim is inferred from
WorkOS's documented session model rather than stated in so many words on a
single page.

**Migration.** The legacy single `[auth]` entry, if found on upgrade, is
migrated into `orgs[<its org_id>]` (and set as `active`) the first time
resolution runs, then removed, the same one-time, transient shape ADR-071
D2's `server_keys` migration uses rather than reading both indefinitely. This
migration path and the legacy `[auth]` table it retires are deprecated as of
this ADR and removed no earlier than **v1.1**, matching ADR-071's own
migration window.

### D4 – `org switch` and `logout` operate on the cache

- **`spelunk org switch <target>`**: if `<target>` already has a fresh (or
  refreshable via its own token) entry, this becomes a local operation,
  point `active` at it, no WorkOS call at all. Only a `<target>` with no
  existing entry falls through to the network-calling onboarding path in
  D3.4.
- **`spelunk org list`** (new): prints each cached org (resolved display
  name where available, else the raw org id) and marks which is `active`.
  Never prints token material, matching ADR-071 D3's `list-servers`.
- **`spelunk logout`** (bare): clears every cached org entry plus the
  legacy `[auth]` remnant, the same "de-authenticate me from spelunk.cloud,
  fully" contract it has today, just applied to N entries instead of one.
- **`spelunk logout --org <target>`** (new): clears only `<target>`'s
  entry, leaving every other cached org's session intact, symmetric with
  ADR-071's `--server <url>`.

## Non-goals

- **Not changing WorkOS's refresh-token semantics.** Refresh tokens stay
  single-use and rotating; this ADR does not ask WorkOS to relax that. It
  works within the constraint by giving each already-onboarded org its own
  independent lineage, so single-use rotation only ever affects the org
  being refreshed.
- **Not zero-cost isolation for a never-before-seen org.** Onboarding a
  brand-new org still costs one network round trip, and the fork path in
  D3.4 still spends whatever org was previously `active`. This is a strict
  improvement over today, where *every* switch is destructive, not a claim
  that the very first encounter with a second org is free of side effects.
  A user who wants to avoid that one-time cost entirely can always onboard
  with `spelunk login --org <target>` instead of the fork.
- **Not a new authorization model.** An org-scoped access token can only
  ever do what WorkOS and cloud-api already let that org's membership do;
  caching more than one such token locally does not widen what any single
  token grants.
- **Not touching the self-hosted `server_keys` map from ADR-071.** That
  remains the credential for the server-key kind (non-cloud origins); this
  ADR only concerns the cloud/WorkOS kind. The two caches are separate
  secret-store entries and do not interact.
- **Not a `--project`-scoped credential.** The map is keyed by organization,
  the tenancy boundary WorkOS and cloud-api actually enforce. Multiple
  projects under the same org share that org's cached credential, which is
  the existing model.

## Consequences

- **The consultant / multi-client case works once each org has been visited
  once.** Switching between two already-cached organizations is instant and
  leaves every other cached organization's session untouched, so a
  long-running agent scoped to one client is not interrupted by a switch to
  another.
- **A repo can pin its org.** `.spelunk/config.toml`'s `org` field means
  opening or cloning a client's repo scopes any invocation inside it
  correctly, without relying on whichever org happens to be globally
  `active` elsewhere.
- **First contact with a new org still has a cost.** One network round
  trip to onboard, and (via the fork path) a small chance of disrupting
  whatever org was active at that exact moment. Both are documented above
  rather than papered over.
- **Debugging "which org backed this call" gains a step.** `spelunk org
  list` (D4) is the answer, mirroring ADR-071's `list-servers`.
- **The legacy `[auth]` table stops being read once migrated,** per D1's
  migration, removed entirely no earlier than v1.1.

## Security implications

- **Same trust boundary as ADR-071.** The cache lives in the same OS
  keychain / owner-only file fallback as the `server_keys` map; no new
  storage location is introduced.
- **`org` in `.spelunk/config.toml` is a lookup key, not a grant.** It only
  selects which cached credential an invocation uses; WorkOS and cloud-api
  remain the sole authority on whether the operator is actually a member of
  that org. A config-pinned org the operator does not belong to produces
  the existing clear membership error (`resolve_arg_to_workos_org_id`), never
  a silent fallback to a different org's token.
- **No secret ever transits argv.** Unchanged from today's device-grant
  flow; `org list` prints organization identifiers only, never token
  material.
- **Blast radius per token is unchanged.** Each cached token is exactly as
  scoped as it is today; caching several concurrently does not make any one
  of them capable of more.
