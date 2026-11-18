# Foreman v2 (durable, approvals, worktrees, detached runs)

This document records the implementation plan for the second foreman layer. The
first layer (`/mode foreman`) is already complete: a read-only main session
delegates through `subagent`, an independent reviewer must PASS, and a bounded
settle gate handles rework.

v2 adds the remaining runtime capabilities without changing the default
subagent behavior:

1. A durable per-session journal (`<session>.foreman.jsonl`) with atomic
   snapshots. It restores mode settings, phase, worker generation, reviewer
   call id, nudge count, pending approvals and the next-worker approval flag.
   No SQLite dependency is added: the journal is a small append-only stream,
   and JSONL plus `fsync`/atomic-rename covers the durability requirement.
2. An approval queue. `approvals=manual` blocks worker delegations with a
   stable reason, records a pending request, and resumes the foreman after
   `/approvals` (TUI) or `pir foreman approve` (detached run). Denied calls do
   not create a worker generation.
3. Optional git-worktree isolation per worker. Worktrees are created only for
   clean repositories, merged back on success, and retained on conflict so the
   foreman can inspect or clean them. Reviewer calls run against the merged
   workspace; FAIL is fix-forward, not an automatic rollback.
4. An optional process executor (`executor=process`) for subagent tasks. It
   runs `pir --mode json --no-session` as a child process with the agent body,
   model and tool allowlist, parses the JSON event stream and enforces the same
   turn/wall-clock/abort budgets.
5. A detached runner: `pir foreman run/status/logs/abort/approve/deny/resume/
   clean`. It reuses the existing detached jobs supervisor. There is no
   long-lived multi-client daemon and no live TUI attach; `logs --follow` plus
   `resume` cover monitoring and continuation.

User-facing commands:

```text
/mode foreman | normal | plan
/foreman [worktree on|off] [router inline|process] [approvals auto|manual]
/approvals

pir foreman run "<task>" [--foreground] [--worktree] [--router ...] [--approvals ...]
pir foreman status|logs|abort|approve|deny|resume|clean ...
```

Defaults are `approvals=auto`, `router=inline`, `worktree=off`, so existing
foreman behavior is unchanged. The new `subagent` JSON fields (`isolation`,
`worktreeMerge`, `executor`) are optional; absent fields keep the current
single/parallel/chain in-process path byte-for-byte.

Implementation order:

1. Journal/settings/recovery + retention sidecars.
2. Approval queue, `/approvals`, `/foreman`.
3. Subagent worktree support.
4. Subagent process executor.
5. `JobStore::spawn_argv` + `pir foreman` CLI.
6. Tests (unit, real git worktree, process executor, CLI, TUI) and docs.
7. Full plan mode: `submit_plan` tool, plan approvals (sharing the approval
   queue), automatic Plan→Normal execution handoff on approval, and durable
   plan text/decision in the journal. Detached plan approvals switch both the
   journal and sidecar to normal.

Acceptance covers the existing mode/subagent suites, new journal/approval/
worktree/process/CLI tests, fmt/clippy, and the TUI scenarios
`s57_mode_foreman.py`, `s58_foreman_v2.py` and `s59_mode_plan.py`.
