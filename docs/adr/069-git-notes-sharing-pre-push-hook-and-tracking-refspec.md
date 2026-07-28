# ADR-069: Share spelunk memory on `git push`, via an opt-in hook and a tracking-ref fetch refspec

**Date:** 2026-07-14
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** resolves the Open question
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md) deferred
("Does 'travels with the repo' require configuring the notes refspec?").
ADR-068 made `refs/notes/spelunk` the durable carrier for pre-`init` memory,
which put the whole "travels with the repo" promise on that ref being visible
across clones. The answer is yes, and the mechanism currently on `main` (#582)
is wrong in a way that loses notes and breaks plain `git fetch`, so this ADR
also **corrects** it. Leaves [ADR-067](067-fail-closed-no-local-project.md)'s
isolation floor and ADR-004's inference-vs-storage split untouched: everything
here is repo-scoped git plumbing, with no new store of record.

## Context

`git notes` under `refs/notes/spelunk` are not pushed, fetched, or cloned by
default. #582 addressed the fetch half by having `spelunk init` configure an
`origin` fetch refspec (`configure_notes_refspec`,
`crates/spelunk-cli/src/cli/cmd/init.rs:263`), and deliberately left the push
half manual. Its own doc comment states the reason, and it is a real constraint
worth keeping:

> Push refspec is deliberately NOT set: any `remote.origin.push` value
> overrides git's default branch push, so a normal `git push` would stop
> pushing the current branch.

So `init` instead prints a hint to run
`git push origin refs/notes/spelunk` after each memory change. A spike against
git 2.55.0 established that both halves of that design are broken: the manual
push hint produces unreachable notes, and the configured fetch refspec both
breaks `git fetch` and silently destroys local notes. The findings below are
observed behaviour, not projections.

## Decision

**Publish notes on `git push` through an opt-in pre-push hook that delegates to a
spelunk plumbing command; merge with `cat_sort_uniq`; fetch into a tracking ref
rather than over the live one; have spelunk itself merge that tracking ref on its
read paths, so reading a teammate's memory needs no opt-in; and put a lock around
the note read-modify-write, without which the merge silently eats entries.**

### D1 – notes sharing is coupled to `git push`, via an opt-in pre-push hook

> **Amended after review of the first implementation (#617).** The trigger
> decision below is unchanged and was not reconsidered: `git push` is still the
> only correct moment to publish. What changed is **where the publish logic
> lives**. As first drafted, D3 specified the flow as a shell hook body
> (`fetch`, then `git notes merge -s cat_sort_uniq`, then `push --no-verify`).
> That logic now moves into Rust behind a plumbing command (**D7**), and the hook
> becomes a shim that calls it. The superseded flow is recorded in D3 rather than
> edited away.

Not to `memory add`, and not to a timer.

A note attached to a **locally unpushed commit** can reach origin while its
target object does not. Origin then answers `fatal: could not get object info`,
and a fresh clone fetches the notes ref but cannot resolve its target. The
memory is **orphaned**: teammates read notes on *their* HEAD and never see it.
This is not a corner case. It is what the current post-`memory add` push hint
produces whenever a developer records a decision before pushing the commit it
describes, which is the normal order of work.

Push-on-`memory add` and a background timer both fail the same way, because both
fire independently of whether the commits are pushed, and both force network
egress outside any user-initiated sharing action. `git push` is the only moment
that reliably coincides with "this code is being shared," so it is the only
correct trigger. A pre-push hook also sidesteps the `remote.origin.push`
constraint above: the hook pushes the notes ref as a separate invocation and
never touches the branch-push default.

### D2 – `cat_sort_uniq` is the canonical merge strategy for `refs/notes/spelunk`

spelunk appends each entry as a JSON line to HEAD's note (`append_record`,
`crates/spelunk-core/src/storage/git_notes/mod.rs`), which makes a note a
**union set** of records, not a document with an authored shape. The merge
strategy has to match that. Measured on divergent notes:

| Strategy | Result |
|---|---|
| `cat_sort_uniq` | exit 0, clean union, both entries, no duplicates, no conflict markers |
| `union` | keeps both, injects a blank-line artifact |
| `ours` / `theirs` | exit 0, and **silently destroys one side's memory** |
| default (`manual`) | **exit 1**, CONFLICT, leaves `.git/NOTES_MERGE_WORKTREE` and a stuck partial merge |

Only `cat_sort_uniq` matches the union-set semantics, and it is never the
default, so it must be passed **explicitly** on every merge. A 3-developer
round-trip converged to identical notes with zero loss, and stayed converged
across repeated syncs (idempotent).

Two consequences are accepted deliberately:

- **Concurrent edits union rather than conflict.** There is no user
  interaction to resolve, and force-push never enters the picture.
- **Read order is no longer chronological.** Confirmed on merged output, which
  interleaves lines by `id` rather than by time. `cat_sort_uniq` sorts
  lexicographically, so `read_records`
  (`crates/spelunk-core/src/storage/git_notes/mod.rs`) must sort by `created_at`
  on read. It currently returns records in blob line order, which is
  chronological only because appends happen to land in order today.

**D2's safety rests on git's newline normalization, not on spelunk's code.**
`append_to_git_notes` builds the body as `format!("{}\n{}", …)`
(`git_notes/mod.rs:45-49`) with **no trailing newline**. git adds it when
storing via `notes add -F`. That is the only reason `cat_sort_uniq` does not
weld the last line of one side onto the first line of the other and corrupt both
records (verified: every line survives the union parseable). This is load-bearing
behaviour owned by an external tool, so the implementation must carry a
**regression test** that would fail if that normalization ever changed. It must
not be left as an implicit assumption.

### D3 – opt-in, best-effort, and blocking only when spelunk itself is gone

> **Amended after review of the first implementation (#617).** Two things in
> this section changed, and both are recorded rather than edited away.
>
> **The `command -v spelunk` skip is withdrawn, and the reason it carried was
> wrong.** As first written, this section justified the guard as a
> `command -v spelunk` skip "so teammates without spelunk are unaffected."
> **That case cannot occur:** `.git/hooks/` is never cloned, so a teammate never
> receives the hook at all. This ADR was the sole carrier of that wrong fact,
> the same failure mode #616 had to correct in D5. The guard is replaced by an
> embedded absolute path, for a *different* reason that nothing had written
> down (below).
>
> **"Exits `0` unconditionally" is now scoped, not deleted.** The empirical
> finding behind it stands: a hook exiting `1` aborts the branch push outright,
> and in the spike origin never received the commit. That still governs
> **publish failures**. It no longer governs a **missing binary**, which is now
> the one case allowed to fail loudly.
>
> The **hook flow** as first specified (`fetch`, then
> `git notes merge -s cat_sort_uniq` from the tracking ref, then
> `push --no-verify`) moves out of shell and into Rust behind a plumbing
> command (**D7**). D1's trigger decision is untouched.

- **Installed explicitly** (`spelunk hooks install --pre-push`), never silently.
  It reuses the guard pattern already established by the post-commit hook
  (`crates/spelunk-cli/src/cli/cmd/hooks.rs`,
  `crates/spelunk-cli/src/cli/cmd/init.rs`): bail if a non-spelunk pre-push hook
  is present, and keep an idempotent marker.
- **`hooks install` embeds the resolved absolute path of the binary**, rather
  than looking spelunk up on `PATH`. The guard this replaces was load-bearing for
  a reason never recorded: `install.sh` falls back to `${HOME}/.local/bin` when
  `/usr/local/bin` is not writable (`install.sh:74-82`) and then tells the user to
  add it to their **shell profile** (`install.sh:130-139`). macOS GUI apps inherit
  their environment from launchd, not from a shell profile, so Tower, GitHub
  Desktop, VS Code and IntelliJ run hooks **without** `~/.local/bin` on `PATH`.
  Simply dropping the guard would therefore break `git push` from every GUI client
  for anyone on that install path. Embedding the path separates the two cases,
  which a `PATH` lookup could not:

  | Case | Behaviour |
  |---|---|
  | spelunk genuinely removed | the absolute path fails, the hook exits non-zero, the push stops and says so |
  | spelunk present, but absent from a GUI client's `PATH` | irrelevant: there is no `PATH` lookup |
  | publish fails (offline, remote rejects) | exits `0`, the push proceeds |

  **The cost is accepted, not absent:** the embedded path goes stale if the binary
  is moved or reinstalled elsewhere. It then fails loudly, and
  `spelunk hooks install --pre-push` re-resolves it. That was chosen over failing
  silently: a user is better served by being told a tool it expected is gone than
  by cruft sitting untidied forever.
- **A publish failure never blocks the user's `git push`.** Memory sharing must
  not be able to cost someone their push, so every failure to fetch, merge or
  publish exits `0` and warns on stderr. Only a missing binary is exempt.
- **A recursion guard is mandatory, and survives the move into Rust.** A naive
  pre-push hook that pushes the notes ref recursed **740 levels deep** and stopped
  only by exhausting the process table (`cannot fork() ... Resource temporarily
  unavailable`). All outer pushes failed invisibly while the branch push still
  reported success. The Rust command still runs `git push`, which still fires
  pre-push, so the hazard is unchanged: the nested push MUST use `--no-verify`,
  which makes git skip pre-push entirely, with the `SPELUNK_NOTES_PUSH` env
  sentinel as belt-and-braces on top.
- **Retry at most 3 times** on non-fast-forward. This is not a guess: under a
  concurrent 3-way race the third developer only succeeded on attempt 3. The
  retry predicate stays narrow: anything that is not a lost race (offline, a
  rejecting remote) would fail identically three times. Never force-push.
- **Hook flow:** the shim `exec`s the plumbing command (D7), which performs
  `fetch`, then `git notes merge -s cat_sort_uniq` from the tracking ref, then
  `push --no-verify`.
- **`spelunk init` must announce the step.** Opt-in only works if it is
  discoverable, so `init`'s summary output must state that sharing memory with
  teammates requires installing the pre-push hook, and name the command. This is
  a requirement on the implementation, not a docs-only note: a user who never
  reads `docs/` must still learn that their memory stays local until they take
  one more step. It replaces the `PUSH_HINT` line (D4), so `init` stops
  advertising the orphan-prone manual push and points at the hook instead.

### D4 – correct the fetch refspec to a tracking ref

Replace #582's `+refs/notes/spelunk:refs/notes/spelunk` with
**`+refs/notes/spelunk*:refs/notes/origin/spelunk*`**
(`FETCH_REFSPEC`, `crates/spelunk-cli/src/cli/cmd/init.rs:264`). Three separate
findings force this, all on git 2.55.0:

- **The non-glob form breaks plain git.** It requires the remote ref to exist.
  With no notes on the remote, which is every repo until someone pushes,
  `git fetch origin` exits **128** and `git pull` exits **1**, both with
  `fatal: couldn't find remote ref refs/notes/spelunk`. `spelunk init` therefore
  breaks the user's normal git workflow. A glob tolerates the missing remote ref
  and fetch exits 0.
- **The leading `+` on a working ref destroys local notes.** It force-updates
  the destination: a local unpushed note was silently replaced by the remote's
  on a plain `git fetch`, reported only as `(forced update)`, and recoverable
  only via reflog. That is data loss of the product's core asset.
- **A glob alone is not enough.** `…spelunk*:…spelunk*` fixes the fetch break
  but still clobbers the local ref. Only the **tracking** destination is safe.

**The tracking ref was then attacked directly, and held.** The concern worth
testing was that the `+` still force-updates *something*, so drift or an
unlucky interleaving might destroy user-authored notes anyway. It does not
reproduce. Tested: a local and remote note merging; drift (fetch R1, teammate
pushes R2, then the hook runs); a teammate pushing in the window between the
hook's fetch and its push (non-fast-forward, and the retry converged); and a
**remote rewind**, where the tracking ref was force-updated backwards and the
working ref still **retained** the dropped entries. The `+` on the *tracking*
ref destroys nothing the user authored, which is the whole point of moving the
destination off the working ref.

**Consequence, recorded plainly:** fetched notes land in
`refs/notes/origin/spelunk` and are **not** directly visible to
`git notes --ref=spelunk` or `spelunk memory list` until merged. "Travels
automatically on `git fetch`" therefore becomes **fetch + merge**. D5 decides
who performs that merge.

**No migration.** #582's refspec landed on 2026-07-12 and the most recent tag,
v0.9.3, predates it (2026-07-08), so no release contains the broken value and in
practice nobody carries it on disk. Anyone who built that revision and ran
`init` is following the tree closely enough to read a CHANGELOG note and fix
their own config. The implementation should not carry migration code for a
population that almost certainly does not exist.

**Because migration is dropped, the CHANGELOG note is the only remedy, so the
command it carries must actually work.** The obvious form is broken:

```
git config --unset remote.origin.fetch '+refs/notes/spelunk:refs/notes/spelunk'
error: invalid pattern: +refs/notes/spelunk:refs/notes/spelunk
```

The value argument is a **regex**, and a leading `+` is not a valid one. The
CHANGELOG must carry a verified form instead, either

```
git config --unset --fixed-value remote.origin.fetch '+refs/notes/spelunk:refs/notes/spelunk'
```

(git >= 2.30), or the escaped regex `'\+refs/notes/spelunk:refs/notes/spelunk'`.

**Re-running `init` is the natural fix and it does not work.**
`configure_notes_refspec` (`init.rs:263`) adds the refspec with `git config
--add`, guarded by an idempotence check that matches only the **identical**
string. An affected user re-running `init` therefore keeps the old clobbering
refspec, gains the new one alongside it, stays clobbered, and is told
`configured notes fetch refspec on 'origin'`. That trap is exactly why the
CHANGELOG note has to name the explicit `--unset` command rather than say "run
`init` again."

The `init` push hint (`PUSH_HINT`, `init.rs:265`) tells users to push notes
after each memory change, which is exactly the orphaning failure in D1. It is
replaced by the hook install hint (D3), not kept as a fallback.

### D5 – spelunk performs the notes merge on its own read paths

> **Provenance, recorded rather than smoothed over.** D5 was added to this ADR
> on a review suggestion and written in untested, unlike D1 through D4, which
> came out of the original spike. A later spike then refuted it as drafted: the
> read-path merge silently lost entries to a lock-free write (D6), and the cheap
> short-circuit this section originally claimed does not exist. The decision
> survived; several of its stated properties did not. Everything below is the
> spiked version.

D4 leaves fetched notes in `refs/notes/origin/spelunk`, and D1's hook merges
them only for people who **push**. That is not everyone. A teammate who only
fetches or pulls, the common case for reviewing and reading, would never trigger
a merge and would therefore never see anyone else's memory. Closing that gap
with a complementary fetch hook is not possible: **git has no post-fetch hook.**
The documented hook set on git 2.55.0 is applypatch-msg, commit-msg,
fsmonitor-watchman, the p4-* family, post-applypatch, post-checkout,
post-commit, post-index-change, post-merge, post-receive, post-rewrite,
post-update, pre-applypatch, pre-auto-gc, pre-commit, pre-merge-commit,
pre-push, pre-rebase, pre-receive, prepare-commit-msg, proc-receive,
push-to-checkout, reference-transaction, sendemail-validate, and update. Nothing
in it fires after a bare `git fetch`. The two near misses do not work:
`post-merge` fires on `git pull`'s merge but not on `fetch`, and
`reference-transaction` fires on **every** ref transaction, including the notes
merge's own ref writes (a recursion hazard), at the wrong altitude entirely.

**So spelunk does the merge itself, rather than delegating it to git.** On its
own read paths, spelunk merges `refs/notes/origin/spelunk` into
`refs/notes/spelunk` with `-s cat_sort_uniq`: in `spelunk memory list` and
`spelunk context`, and at `spelunk init`, where the git-notes import already
hydrates the index.

Why this rather than a hook or a git config setting:

- **It covers the fetch-only consumer**, the exact population D1 cannot reach.
  Their first merge is a fast-forward and costs almost nothing: 9.3ms at 1000
  notes, 10.6ms at 20000.
- **Nothing to install and no git config surgery.** It works for a teammate who
  never runs `spelunk hooks install`.
- **The strategy stays per-invocation.** spelunk passes `-s cat_sort_uniq` on
  the call and never writes the user's `notes.mergeStrategy`, whose default is
  `manual`. Their own `git notes merge` keeps behaving exactly as they
  configured it.
- **Repeated reads converge.** The union merge is idempotent (D2), so a read
  path can run it every time without drift.
- **A missing tracking ref never reaches the caller, though not always by
  exiting 0.** It has two arms, verified for users with no remote at all and
  after a `fetch --prune` deletes the ref. With notes already on the working ref
  the merge is a no-op that exits 0. With the working ref empty too, which is
  the fresh solo user who has recorded nothing yet, git exits **128** with
  `Cannot merge empty notes ref`. `merge_tracking_notes` swallows either outcome
  and reads regardless, so the read-path merge is safe to run unconditionally:
  the solo user never sees it, and it needs no "do I have a remote?" special
  case.

**The merge is synchronous, not backgrounded.** The founder's constraint is that
it must not block the user's command and must work with the network down or
flaky. Both hold: the merge does no network (see Security implications) and the
measured cost is inline-acceptable (see the divergent-object numbers under
Consequences). Async fails on both counts. It defeats "reading is automatic,"
because notes would land one invocation late, and it makes the D6 race strictly
more likely, since a background merge is concurrent with foreground commands by
construction.

**The cost, recorded honestly:** a read command mutates a local ref, which is
normally a thing to avoid. It is acceptable because the mutation is local-only
and touches no network, and because it is precisely the carrier to index
hydration that ADR-068's model already implies (git notes are the carrier; the
local store is the queryable index over it).

**There is no cheap short-circuit, and none is needed.** An earlier draft of
this ADR claimed the merge could skip work by comparing refs, "costing a ref
comparison and nothing more." That is false. A no-op merge is **9.2ms flat at
every scale**, because git already short-circuits internally, and a `git
rev-parse` subprocess guard costs **8.0ms** to save about 1ms. Only an
in-process read of the ref file (17µs) is worth having, and it is an
optimisation, not a correctness requirement.

**D5 is safe only with D6.** As written above it silently loses entries; the
lock is not optional.

### D6 – serialize note writes with a spelunk-owned lock

> **The bounded-degradation clause below is superseded by D8.** Its closing
> paragraph reads the budget's expiry as "skip the merge and read anyway," which
> is right for the read-path merge it was written about and wrong for a writer:
> a writer that proceeds past the budget performs the very read-modify-write
> this section exists to prevent. D8 replaces that policy and states it per
> caller. The rest of D6, including the decision to take a spelunk-owned lock
> around the whole read-modify-write, stands and was not reconsidered.

`append_to_git_notes` (`crates/spelunk-core/src/storage/git_notes/mod.rs:38-73`)
is a **lock-free read-modify-write**: step 2 reads the note body
(`notes show`), step 4 writes the whole body back (`notes add -f -F -`), and
nothing guards the gap between them. A merge that lands in that gap is silently
overwritten by the write-back. Measured: **40 of 40 trials lost the merged
entry, every one exiting 0.**

The loss is **sticky**, not a transient miss. The merge commit is in the working
ref's history afterwards, so the tracking ref is an ancestor and every later
merge correctly reports "Already up to date." The entry still exists in the
tracking ref, but `memory list` never shows it again. Git's own ref locking
cannot help here: the loss happens at the **content** layer, not the ref layer,
and both writers hold the ref lock legitimately in turn.

**This is live today, without D5.** Git worktrees **share one notes ref**: a
worktree's `refs/notes/spelunk` resolves through `--git-common-dir` to the main
repository's copy. Parallel agents working in separate worktrees are therefore
all writing the same ref, and this project's own `CLAUDE.md` instructs every
agent to run `spelunk memory add`. So two concurrent `memory add`s already drop
one entry silently. D5 does not create this bug; it widens it from "two writers
race" to "a read command silently eats a teammate's decision."

**The decision:** a spelunk-owned lock (`flock` or equivalent) around the
**whole** read-modify-write in `append_to_git_notes`, and around the D5
read-path merge. The lock is **bounded**: if it is contended or the wait exceeds
a small budget, **skip the merge and read anyway**. That is safe because the
merge is idempotent (D2), so the next read catches up. A read must **never**
fail because it could not take the lock.

### D7 – the publish flow is a spelunk plumbing command; the hook is a shim

Added on review of the first implementation (#617), which put the flow in the
hook body as shell.

**The shim still needs `#!/bin/sh`, and that is not the problem.** A git hook
must be an executable file, and Git for Windows ships its own `sh` and runs hooks
through it. The existing `POST_COMMIT_HOOK` (`hooks.rs:32`) already relies on
exactly this, and the `windows-latest` CI cell exercises it today on default
features. "Windows has no `/bin/sh`" is not the reason to move, and this ADR
should not be read as recording that. The reason is that **scripting logic** in
shell is the wrong home for it, on three counts:

- **The shell cannot take the D6 lock, and that is a correctness gap, not a
  style objection.** The lock is a **cross-process** file lock on
  `<git-common-dir>/spelunk-notes.lock`
  (`crates/spelunk-core/src/storage/git_notes/lock.rs`), so in principle a hook
  could take it. In practice it cannot do so portably: `flock(1)` is a
  util-linux tool, absent from stock macOS and from Git for Windows' shell.
  The shell hook's merge therefore ran **unlocked**, so a concurrent
  `spelunk memory add` during a push could still lose a record by exactly the
  read-modify-write in D6. Moving publish into Rust closes that gap rather than
  relocating it: `File::try_lock` is available on every supported platform, and
  the publish path takes the same lock as every other writer. The portability
  argument and the lock argument are the same argument.
- **The logic is duplicated outside the tested codebase.** The retry predicate,
  the recursion sentinel and the explicit `-s cat_sort_uniq` are all decisions
  this ADR makes, re-expressed in a second language that shares no types with
  the Rust that makes them.
- **The test cost is disproportionate.** #617's hook body is **58 lines (26 of
  them executable)** and carries a **984-line** integration test
  (`crates/spelunk-cli/tests/pre_push_hook.rs`) driving real git repositories to
  cover it. In Rust the same logic is reachable by ordinary unit tests. (An
  earlier note on #617 put the shell at "~200 lines"; measured, it is 58. The
  smaller number is the honest one and it does not weaken the case.)

**The command is `spelunk plumbing publish-notes`.** The verb-noun name matches
the namespace's existing pattern and reuses this ADR's own vocabulary
(*publishing*, as distinct from *reading*, per the first Consequence).

**This changes what the plumbing namespace means, which is worth stating rather
than slipping in.** All eight existing subcommands (`cat-chunks`, `ls-files`,
`parse-file`, `hash-file`, `knn`, `embed`, `graph-edges`, `read-memory`) are
**read-only JSONL emitters**. `publish-notes` is the first that **writes** and
performs **network I/O**. That is still plumbing in git's sense, where
`git update-ref` writes and `git send-pack` talks to a remote, so the namespace
is the right home. But "plumbing == read-only" stops being true of it, and
anything that assumed so needs to stop.

**It is plumbing, and is treated as such: discoverable but not promoted.** Not
hidden, and not shouted about. `spelunk hooks install --pre-push` stays the
porcelain a user is pointed at (D3), and the `init` announcement names that
command, not this one.

### D8 – a writer that cannot take the notes lock fails; it never proceeds unlocked

Added on review of a `windows-latest` CI failure in D6's own regression guard.

> **The closing diagnosis below ("What D8 does not do") is superseded by the
> implementation's evidence.** It reasoned from a single run that lost 1 of 8
> entries and concluded the defect was lock **identity** across worktrees.
> Three further `windows-latest` failures refute that as the mechanism: one
> lost **6 of 8** entries in the single-repo guard, one process, one repo, no
> worktrees, where no identity split is possible. In every failing run the
> survivors are a contiguous **tail** of the serialization order (ids `[6, 8]`;
> `[8, 2, 4]`; and twice all-but-one), which is the fingerprint of exactly one
> writer reading an **empty** note mid-sequence and rewriting the ref with only
> its own line. The write path made that possible: `append_to_git_notes` read
> the existing note with `git notes show` and treated **any** failure as "no
> note yet" (`.unwrap_or_default()`), so one transient git failure inside the
> guarded section wiped every prior entry, while the writer held the lock the
> whole time, and the lock excluded correctly. The fix distinguishes "no note
> found" (exit 1) from a failed read (anything else); a failed read is retried
> briefly (it is side-effect free, and every observed failure was transient:
> the same read succeeded for sibling writers moments apart) and then fails
> the writer rather than guessing empty. The identity concern was real hygiene and is
> hardened anyway (`--path-format=absolute` where git knows it, output-checked
> because `rev-parse` **echoes** unknown flags with exit 0 rather than
> rejecting them, then canonicalized), with a regression test pinning that
> worktree contenders converge on one lock file. But note the OS primitive
> locks the underlying **file**, not the path string, so two spellings of one
> path never excluded nothing; only paths resolving to genuinely different
> files could, and no failing run required that. D8's contention policy is
> unchanged by this; what it governs was never the loss mechanism observed.
>
> **A second premise below is also corrected by observation: budget expiry is
> not always a bug's symptom.** This section argues the budget is "set
> generously enough that reaching it means a bug rather than a busy repo". A
> `windows-latest` run falsified that: eight legitimate concurrent writers
> serialized correctly on a slow runner, and the back of the queue exceeded
> the 5s budget with nothing pathological anywhere; every over-budget writer
> failed visibly (~5.4s in, naming the lock) while every serialized entry
> survived. The policy stands exactly as written, since expiry stays an error
> and never a downgrade. What does not stand is reading that error as proof of
> a stuck holder: heavy legitimate write concurrency can reach it, the normal
> remedy is a retry, and the error text says so. The concurrency guards assert
> the D8 invariant accordingly: every entry lands or its writer fails visibly,
> never "all must land".

**The rule D6 left implicit, stated:** the contention policy is set by *what is
lost when the lock is missing*, not by whether the caller is nominally a read or
a write. Three cases, three answers:

- **Proceeding unlocked can destroy a record.** `append_to_git_notes`,
  `append_record` and `archive_record` are the exact read-modify-write #185
  describes. They must hold the lock or **fail**. They never proceed unlocked.
- **The work is idempotent and retried on the next invocation.** D5's read-path
  merge and D7's publish lose nothing permanent by not running now. They
  **skip**, report the skip, and never fail the caller.
- **The lock cannot be established at all on this filesystem.** Serialization is
  impossible there, so failing every write would make spelunk unusable on that
  filesystem in order to prevent a race that needs a second concurrent writer to
  matter. These **proceed unlocked, loudly**. This is the one degradation kept,
  and it is kept narrow.

**Why "never fail the caller" was the wrong reading for a writer.** D6 bounded
the lock: "if it is contended or the wait exceeds a small budget, skip the merge
and read anyway... A read must never fail because it could not take the lock."
That clause is about **the merge on a read path**, where skipping costs
freshness and nothing else. The implementation generalized it to writers.
`lock_notes` returns `None` on a contended timeout and all three writer call
sites discard it (`let _lock = lock_notes(...)`), so a writer that times out
performs the unserialized read-modify-write D6 exists to prevent, and exits 0.
"A command must never fail because of lock contention" is being bought with "a
command sometimes silently loses the user's decision." For a tool whose promise
is that it remembers why, that is the worse half of the trade. An error the user
can see and retry costs them a command. A silent clobber costs them the record.

**The wait budget is a watchdog, not a contention threshold.** `lock.rs`
justifies its 5s budget by the holder cost ("~30ms"), which frames it as a
number tuned against expected contention. That framing is wrong twice over.
First, an OS advisory lock (`flock`, `LockFileEx`) is released by the kernel
when the holding process dies, so a **stale lock cannot happen** and there is no
crashed-holder case to time out for. The only thing a budget protects against is
a live holder that never releases, which is a bug, or a deadlock. Second, a
threshold tuned on one platform's timings becomes a correctness knob the moment
its expiry silently changes behaviour.

So the budget stays a **single constant**. It is *not* scaled per platform and
*not* derived from observed holder cost: both add tuning to a number that should
never be reached. It is set generously enough that reaching it means a bug
rather than a busy repo, and its expiry is an **error**, never a downgrade.
Making expiry loud is what makes a generous budget safe, and making the budget
generous is what keeps the loud expiry rare enough that "a writer can fail" is
not a practical regression.

**`Option` is the wrong shape and is replaced.** `None` today means three
different things: someone else holds the lock, the lock file cannot be opened,
and the lock path could not be resolved. D8 gives those three different answers,
so they cannot share one return value. `lock_notes` returns a three-way outcome
(acquired, contended, unavailable-with-reason). The guard is `#[must_use]` so a
caller cannot collapse the distinction back down with `let _`.

**D7's publish path under contention: skip, report, do not fail the push.**
Publishing is a fetch, a merge and a write-back, so by mechanism it is a writer.
But its work is idempotent (D2), and records it did not publish stay in the
local ref and publish on the next push, so a skipped publish loses nothing
permanent. It therefore takes the second branch, not the first. This keeps D3's
best-effort stance intact (spelunk does not block a push) without reintroducing
the unlocked write-back that D7 moved publish into Rust to close. The skip is
reported on the push output rather than swallowed: a user whose memory did not
publish needs to know that it did not.

**The degraded path must be observable, because its silence is why this needed a
CI failure to surface.** Every branch above reports through `tracing::warn!`
today. The integration tests install no subscriber, so those events go nowhere,
and a test run cannot distinguish "the lock was held" from "the lock was skipped
and the write raced." The outcome above is a returned value precisely so that
callers and tests can assert on it directly rather than inferring it from
damage.

**What D8 does not do: it does not by itself fix the `windows-latest` failure
that prompted it.** The evidence rules the timeout path out as that mechanism.
In one run the cross-worktree guard
(`append_to_git_notes_concurrent_worktrees_all_survive`) failed in **2.569s**,
having lost 1 of 8 entries. In the same run, on the same runner,
`append_to_git_notes_proceeds_when_lock_budget_is_exhausted` passed in
**6.189s**. That test forces a real budget exhaustion, so it calibrates the cost
of one: over 5s. A 2.569s failure cannot contain a 5s wait. In the same run the
single-repo guard (`append_to_git_notes_concurrent_writers_all_survive`, eight
concurrent writers in one process) **passed** in 2.460s, and the lock's own
contract tests passed, which together show the primitive excludes and times out
correctly on Windows.

What is left is that two contenders do not converge on one lock file when they
sit in **different worktrees** on Windows. `notes_lock_path` reaches that path by
two different branches: a main worktree gets a relative answer from
`git rev-parse --git-common-dir` (`.git`) and joins it against the repo root,
while a linked worktree gets an absolute answer and uses it as-is. On unix both
branches land on the same file, which is why only the Windows cell fails.
Deciding which of those two strings is wrong on Windows needs a Windows host,
and the warning that would say so is discarded, per the observability point
above. That is a defect in lock **identity** and is fixed separately. D8 governs
what a writer does once it knows it does not hold the lock, which is a question
the identity fix does not answer and which the current code answers wrongly.

### D9 – the notes lock covers the merge only, never the fetch or the push

Added on review of D7's implementation (#630), which has this right and records
nothing about why.

D7 puts a fetch, a merge and a push behind one command, and D8 makes a writer
that cannot take the lock fail. Together they make the lock's **scope** inside
`publish-notes` load-bearing in a way neither section states. Taking the lock
once around the whole operation is the tidier-looking code and is the bug.

**The budget is bounded; network I/O is not.** D8 sets the wait budget as a
watchdog, "generously enough that reaching it means a bug rather than a busy
repo", and rests its case that "a writer can fail" is not a practical regression
on exactly that rarity. A `fetch` or a `push` against a slow or unreachable
remote outlasts any budget chosen on that reasoning. A lock spanning either does
not merely stretch D8's premise, it **falsifies** it: budget expiry stops meaning
a bug and starts meaning a slow network, and a `memory add` that happens to
coincide with such a push fails for a reason that is not a bug. D8's error is
correct because it is rare. D9 is what keeps it rare.

`lock.rs` shows how directly this is load-bearing. It justifies its budget by the
holder cost, "a few git subprocesses, ~30ms", and concludes that the budget is
"orders of magnitude above realistic contention". That holds only while a holder
does local git work and nothing else. Scope the lock over a fetch or a push and
the holder cost stops being a spelunk constant and becomes the remote's latency.

The constraint outlives the policy it was first drafted against. Under D6's
withdrawn proceed-unlocked clause a network-spanning lock converted a rare race
into a reliable one; under D8 it converts a rare failure into a reliable one. It
is the same defect either way, which is why the scope rule is stated separately
from the contention policy: the lock's hold time stops being a property of
spelunk's own work and becomes a property of the network.

**Neither network step needs the lock.** `fetch` writes only the tracking ref
(D4), so it never contends for the working ref's content. `push` only reads the
working ref, and git's ref locking already makes that read atomic; the worst case
is that it publishes a state one entry stale, which the next push carries.
Staleness is not loss.

**Per attempt, not around the retry loop.** D3 retries a lost race up to 3 times,
and each attempt is a fetch, a merge and a push. The lock is taken and released
inside each attempt's merge. A guard hoisted out of the loop would hold it across
up to three network round trips, which is the same defect three times over.

**#630 satisfies this by reuse rather than by intent, which is the reason to
record it.** `publish_notes` runs `fetch` and `push` itself and delegates the
merge to `merge_tracking_notes`, which takes `lock_notes` in its own body and
drops the guard on return, so no lock is held across either network call. Nothing
at that call site says the arrangement is load-bearing. The obvious tidy-up,
hoisting the lock to the top of `publish_notes` so that the whole flow is
"properly" serialized, removes it silently and reads as an improvement.

D8 governs what publish does when it **cannot** take the lock: skip, report, do
not fail the push. D9 governs how much of publish the lock covers when it
**can**.

## Non-goals

- ~~**Not** fixing #185, the read-modify-write race within a single repo.~~
  **Withdrawn. #185 is in scope, via D6.** The first draft of this ADR deferred
  it as an orthogonal local race. The spike showed that is not tenable: the same
  lock-free read-modify-write that #185 describes silently swallows D5's merge in
  40 of 40 trials, and it is **already losing entries today** for concurrent
  agents in shared worktrees, with no D5 involved. Deferring it would ship a read
  command that eats a teammate's decision. `cat_sort_uniq` resolves the
  inter-repo race; only D6's lock resolves the intra-repo one, and this ADR now
  requires both.
- **Not** reconciling `NoteRecord.id` collisions. `id` is a local SQLite rowid
  and `remote_id` is only set on server sync, so on the local-first path two
  developers both produce `id:1` for different entries. `cat_sort_uniq` dedupes
  whole lines, so nothing is lost, but the carrier can legitimately hold
  colliding ids. **#598 resolves this**, not this ADR: it amends ADR-068 with a
  canonical content-addressed identity (`content_id`, a `sha256` over the
  entry's canonical JSON semantic core, plus a stable `entity_id` across edits
  and supersede), which removes the rowid collision at its source. This ADR
  depends on nothing more than line-level dedupe, so the two land
  independently.
- **Not** reusing the name `spelunk memory push`. That already means "push local
  memory to the team server," and overloading it onto the notes path would
  conflate two different destinations.
- **Not** setting `remote.origin.push`, for the reason #582 recorded and this
  ADR keeps: it would override git's default branch push.
- **Not** making notes sharing on by default. D3 is opt-in.

## Consequences

- **The promise splits cleanly into reading and publishing.** *Reading*
  teammates' memory is automatic for anyone who fetches, because spelunk merges
  the tracking ref on its own read paths (D5). *Publishing* your own memory is
  opt-in and needs the hook (D1, D3). So "travels with the repo" holds
  unconditionally in the direction users notice first, and the docs should
  describe the asymmetry rather than flatten it.
- **`spelunk init` stops breaking `git fetch` / `git pull`.** This is a bug fix
  to shipped behaviour, not only a new feature, and it is the part that should
  land first.
- **Notes read order must be sorted on read.** D2's lexicographic union is a
  behaviour change that `read_records` has to absorb (sort by `created_at`).
  Anything that assumed blob order is chronological needs to stop.
- **Read commands acquire a local write.** `memory list` and `context` now
  merge a ref (D5). No network, no remote effect, but anything assuming these
  commands are pure reads of local state needs to account for it.
- **#185 moves from deferred to required, and a concurrency bug gets fixed on
  the way.** D6's lock is a precondition of D5, and it also closes a live
  entry-losing race that predates this ADR for parallel agents in worktrees.
- **The publish path joins the lock protocol, closing a gap the shell left
  open.** The shell hook of #617 could not portably take D6's lock, so its merge
  ran unlocked and a concurrent `spelunk memory add` during a push could still
  lose a record. That was accepted at the time as a tracked follow-up. D7 closes
  it as a side effect: publish is now a Rust caller of the same
  cross-process lock as every other writer, so it needs no separate fix.
- **`spelunk plumbing` is no longer a read-only namespace.** `publish-notes`
  (D7) is its first writing, network-touching subcommand. Anything that treated
  the namespace as safe-by-construction for scripting or sandboxing needs to
  account for it.
- **The hook stops being a place where decisions live.** After D7 the installed
  file is a shim, so changing the retry count, the merge strategy or the
  recursion guard is a code change under test, not an edit to a string constant
  that a user may already have on disk. Users who installed an older hook keep
  the shell body until they re-run `spelunk hooks install --pre-push`, which is
  the same re-resolution path the embedded absolute path already needs (D3).
- **Note size is not the cost axis; divergence is.** Growing a note is nearly
  free (10 to 1000 entries moves the merge from 11.9ms to 14.9ms). The cost
  scales with **divergent annotated objects**, at roughly 0.6ms each: 5000
  annotated with 100 divergent is 94.6ms, 5000 with 5000 divergent is 2.14s, and
  20000 with 20000 divergent is 12.7s. Realistic steady state, where a fetch
  brings 1 to 100 changed notes, is **12 to 95ms**, which is fine inline.
- **No migration, one CHANGELOG line.** #582 shipped in no release, so the
  broken refspec has no real population. The change note tells the handful of
  people tracking `main` to fix their config by hand, with the `--fixed-value`
  command from D4, since re-running `init` will not fix them.
- **`init`'s refspec test changes.** `crates/spelunk-cli/tests/init_notes_refspec.rs`
  pins the old value and its round-trip expectations; both move to the tracking
  ref.
- **Revisit if:** the number of **divergent annotated objects** in a normal
  fetch climbs toward the thousands, which is the axis that actually degrades
  (seconds, not milliseconds), or if the union model produces notes noisy enough
  that lexicographic union becomes a readability problem. Note size alone is not
  a trigger.

## Security implications

- No new trust boundary and no new store of record. Everything here is
  repo-scoped git plumbing over a ref the user's own repo already holds.
- **Network egress is bounded to the user's own `git push`.** This is stronger
  than the alternatives rejected in D1: push-on-`memory add` and a timer would
  both send data at moments the user did not initiate. The hook adds no egress
  the user was not already performing, and to no host other than the remote they
  chose. D7 makes the publish flow **directly invocable**
  (`spelunk plumbing publish-notes`), which does not widen this bound: running it
  is itself a user-initiated action, and it reaches only the remote it is given.
  It is the first plumbing subcommand that touches the network at all, so the
  namespace no longer implies "no egress" (see Consequences).
- **D5's read-path merge does not fetch, and this was verified rather than
  assumed.** With the remote made unreachable the merge still succeeds (~12ms).
  `GIT_TRACE` and `GIT_TRACE_PACKET` show no transport. And the positive proof:
  an entry pushed to the real origin *after* the tracking fetch was **absent**
  from the merge result, so the merge demonstrably does not reach out and pick
  up new remote state. It merges only the tracking ref the user's own `git
  fetch` already populated. This is deliberate: making reads fetch would put
  egress on a code path the user never pointed at a remote, and it is what lets
  D5 work with the network down.
- The `memory add` secret scan (`contains_secret`, run before any persistence)
  is unchanged and still runs before anything reaches a note, so the hook can
  only ever push already-scanned content.
- The hook never force-pushes (D3), so it cannot destroy remote history. The
  destructive behaviour identified here is #582's `+` on a working ref (D4),
  which this ADR removes.
