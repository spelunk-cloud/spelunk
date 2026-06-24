# Memory Benchmarks

## Decision Archaeology

Measures whether spelunk memory can retrieve design rationale from git history
better than lexical search (grep, FTS5).

### Blindness Protocol

Questions MUST be authored without access to the harvested spelunk memory
database. The protocol:

1. **Source material:** Read raw `git log` output for the target repo. Use
   `git log --format="%H %s"` or GitHub PR/commit pages. Do NOT run
   `spelunk memory list` or `spelunk memory search`.
2. **Question authoring:** Write natural-language questions a developer would
   genuinely ask about the codebase's history. Examples:
   - "How does error handling work in the parser?"
   - "Why was async I/O chosen over threads for the network layer?"
   - "What tradeoffs led to the current lock-free queue design?"
3. **Ground truth:** For each question, record the commit SHA(s) that best
   answer it. Derive this from the raw git log, NOT from memory entries.
4. **Review:** Have the question set reviewed by a second party with no
   access to the spelunk memory database. Record the reviewer and date
   in the `reviewed_by` field.
5. **Format:** Save as `bench/memory/questions-<repo>.json`:
   ```json
   [
       {
           "question": "How does error handling work in the parser?",
           "ground_truth_commit": "abc123def456",
           "reviewed_by": "<name> on <date>"
       }
   ]
   ```

### Script

```
bench/memory/author_questions.py   — extracts git log for blind authoring
bench/memory/decision_archaeology.py — runs four-condition comparison
```

### Committed question sets

Per #237, three blind-authored question sets are committed:

- `bench/memory/questions-ripgrep.json` (11 questions)
- `bench/memory/questions-ruff.json` (11 questions)
- `bench/memory/questions-tokio.json` (11 questions)

33 questions total, each with `question`, `ground_truth_commit` (full 40-char
SHA), and `reviewed_by`. All three repos were cloned fresh into a scratch
directory outside this worktree; `bench/memory/raw-commits-<repo>.json` was
generated via `author_questions.py --num-commits 500` and used as the only
source material (supplemented by reading the corresponding GitHub PR pages
for commits with empty bodies). No `spelunk memory list`/`search` was run
against any of the three repos during authoring. The previous
`questions-ripgrep.json` (5 questions, derived from harvested memory) has
been replaced rather than kept alongside the new set.

Each set was reviewed by a second pass with no access to the spelunk memory
database; see `reviewed_by: "ada (test-engineer) on 2026-06-10"` on every
entry for the audit trail.

### Authoring workflow

```bash
# 1. Export raw git log (no spelunk access)
python bench/memory/author_questions.py \
    --repo-path /path/to/repo \
    --num-commits 500 \
    --out bench/memory/raw-commits-<repo>.json

# 2. Read the raw-commits file (NOT spelunk memory output).
#    Author ≥10 questions per repo. Record ground-truth commit SHAs.

# 3. Save questions
#    (hand-write into bench/memory/questions-<repo>.json)

# 4. Index + harvest memory, then run benchmark
spelunk index /path/to/repo
cd /path/to/repo && spelunk memory harvest --git-range HEAD~500..HEAD
python bench/memory/decision_archaeology.py \
    --repo-path /path/to/repo \
    --questions bench/memory/questions-<repo>.json \
    --out bench/results/archaeology-<repo>.json
```

### Four conditions

| Condition | Query | Search target |
|-----------|-------|---------------|
| `grep_literal` | Full question verbatim | `git log --grep` |
| `grep_keywords` | Regex-extracted keywords | `git log --grep` per keyword |
| `fts_commit_messages` | Full question | SQLite FTS5 over all commit messages |
| `memory_search` | Full question | `spelunk memory search` (semantic) |

## Cross-Session Handoff

Measures whether spelunk memory improves completion success for a fresh
agent picking up partially-completed work.

### Three conditions

| Condition | Session 1 files | Memory access | Measures |
|-----------|----------------|---------------|----------|
| Cold start | None | None | Intrinsic task difficulty |
| Files present | On disk | None | Value of file state alone |
| With memory | On disk | Full | Value of files + memory |

### Task format

Tasks live in `bench/memory/handoff_tasks.json`:

```json
[
    {
        "task": "Fix the failing test in tests/test_parser.py",
        "repo_url": "https://github.com/user/repo.git",
        "setup_cmd": "pip install -e '.[dev]'",
        "verify_cmd": "python -m pytest tests/test_parser.py -x -q"
    }
]
```

- `task`: natural-language description
- `repo_url`: git clone URL (repo cloned fresh per task)
- `setup_cmd`: shell command to install dependencies
- `verify_cmd`: shell command that exits 0 on success (binary pass/fail)

Session 1 is cut off at `--session-1-turns` (default 5) with a system
prompt instructing the agent to write a detailed handoff. Three Session 2
clones then attempt the task under the three conditions.

Usage:

```bash
python bench/memory/cross_session_handoff.py \
    --tasks bench/memory/handoff_tasks.json \
    --session-1-turns 5 \
    --session-2-turns 15 \
    --out bench/results/handoff.json
```
