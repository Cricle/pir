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
   not create a worker generation. An approval authorizes exactly the task it
   was granted for (bound by approval id): approving task A never admits a
   different task B. A denied task can be requested again — its stable id is
   reopened as a fresh pending request rather than being deduped away.
   Reviewer PASS is persisted by worker generation, so a resumed session does
   not re-gate an already verified worker. `/mode normal --force` is the
   emergency exit while a run is busy.
3. Optional git-worktree isolation per worker. Worktrees are created only for
   clean repositories, merged back on success, and retained on conflict **or
   on a failed run** so the foreman can inspect the worker's partial changes.
   Reviewer calls run against the merged workspace; FAIL is fix-forward, not
   an automatic rollback.
4. An optional process executor (`executor=process`) for subagent tasks. It
   runs `pir --mode json --no-session` as a child process with the agent body,
   model and tool allowlist, parses the JSON event stream and enforces the same
   turn/wall-clock/abort budgets. The parent's provider-scoped explicit
   credential is inherited through `PIR_SUBAGENT_API_KEY` (an env var, never
   an argv secret), and the subagent depth carries across the process boundary
   via `PIR_SUBAGENT_DEPTH` so the global nesting cap stays effective.
   A parallel batch returns an error if any leg fails — a broken worker is no
   longer reported as a successful batch.
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

## Residual limitations and the durable engine

Two remaining edge cases share one architectural root cause: a run that waits
for an asynchronous external event has no explicit, resumable wait state.

- **Headless background completion**: in `pir print`, a background tool call
  that finishes after the terminal model turn is persisted as a
  `backgroundToolResult` entry and reported through a `background_tool_end`
  event, but the model is not given another turn to consume the result.
- **Approval waiting**: the foreman/plan approval gate blocks with
  `terminate:false` and asks the model (via prompt/notice text) to end its turn;
  the approval decision then arrives as a follow-up. Making the block
  `terminate:true` was tried and reverted — it breaks the approval follow-up
  because tool-call termination is a batch hint, not a suspend/resume primitive.

`src/durable/` is a mature, high-fidelity port of `pi-durable`: atomic committed
state (`src/durable/session/`), a check-pointed task state machine with
`Waiting{on}` / `Sleeping{until}` / `Wake` (`src/durable/harness/scheduler/`),
a submission inbox (`src/durable/harness/inbox.rs`), and background tasks.
Conceptually it is the right engine for "suspend on an event, wake, resume from
checkpoint". In practice it is **not wired to any production path**: every
`crate::durable` reference is inside `src/durable/`, and its consumers are tests,
benches and examples only. The module describes itself as a second harness
beside `agent_loop`/`agent_session`, not the engine beneath them
(`src/durable/mod.rs:12-16`), and still has open seams (unwired `DeferredPoll`,
`HarnessError::NotPorted`, an experimental events adapter).

Migrating foreman/headless orchestration onto durable to fix these two narrow
cases would be a rewrite of run orchestration (scheduler/wake/approval
wiring), not an incremental fix. The chosen approach is therefore a local,
bounded wait-and-resume in the print host for the headless case, and keeping
the approval design constraint as documented behavior. Durable remains the
right long-term target only if the product goal becomes a unified,
crash-recoverable run/subagent/background/approval orchestrator.
