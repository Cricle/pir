# Foreman mode

`/mode foreman` runs a **read-only main session** that delegates implementation
to workers through `subagent`, gates each result through an independent
reviewer, and settles with a bounded rework loop. Three settings add durability,
approvals, isolation and detached runs on top of that default.

Implementation: `crates/pir-builtin-mode/` (`mode.rs`, `delegation.rs`,
`settings.rs`, `approvals.rs`, `journal.rs`); detached CLI:
`src/main_cli/foreman_mode.rs`.

## Commands

```text
/mode foreman | normal | plan
/foreman [worktree on|off] [router inline|process] [approvals auto|manual]
/approvals

pir foreman run "<task>" [--foreground] [--worktree] [--router ...] [--approvals ...]
pir foreman status|logs|abort|approve|deny|resume|clean ...
```

Defaults: `approvals=auto`, `router=inline`, `worktree=off`. With those the
behavior is identical to the original foreman layer.

## Settings

`ModeSettings` (`settings.rs`), persisted per session:

| Setting | Values | Default | Effect |
|---|---|---|---|
| `approvals` | `auto` \| `manual` | `auto` | `manual` blocks each worker delegation until a user decision |
| `router` | `inline` \| `process` | `inline` | `process` runs one `pir --mode json --no-session` child per task |
| `worktree` | `on` \| `off` | `off` | Each worker runs in an isolated git worktree |

The active settings are rendered into a `<foreman_settings>` block in the
system prompt. Only static text is inserted — user input never reaches the
prompt.

## Tool surface

`MODE_ALLOWED_TOOLS` (`delegation.rs`) — the direct tools a foreman or plan
session may call:

```text
submit_plan, subagent, read, grep, find, ls, tool_search, todo,
background_status, background_abort
```

`submit_plan` is visible only in plan mode. Everything else is blocked, so the
main session cannot edit files itself. Child tool sets are narrower:

| Child | Tools |
|---|---|
| plan-mode subagent | `read`, `grep`, `find`, `ls` |
| reviewer | `read`, `grep`, `find`, `ls`, `bash` |

The reviewer keeps `bash` because its contract requires a smoke check, but
loses every write-capable tool.

## Durable journal

Per-session sidecar `<session>.foreman.jsonl` (`journal.rs`): append-only JSONL,
rewritten as one snapshot above 256 KiB or at shutdown. It restores mode
settings, phase, worker generation, reviewer call id, nudge count, pending
approvals and the next-worker approval flag. No SQLite dependency — JSONL plus
`fsync` and atomic rename cover the durability requirement.

Call ids are stored as stable FNV-1a hashes rather than raw ids, so a long
session's journal stays small and survives Rust upgrades.

Journal version `1`; approval history is capped at 64 records.

## Approvals

`approvals.rs`. With `approvals=manual` a worker delegation is blocked with a
stable reason and a pending request is recorded. The decision arrives via
`/approvals` (TUI) or `pir foreman approve <id>` (detached run), and the foreman
resumes.

- Approving task A never admits task B — each approval is bound to its id.
- A denied task can be requested again; its id reopens as a fresh request
  instead of being deduped away.
- A denied call creates no worker generation.
- Reviewer PASS is persisted per worker generation, so a resumed session does
  not re-gate an already verified worker.
- `/mode normal --force` is the emergency exit while a run is busy.

`ApprovalKind` is `worker` (the default, so older journals replay unchanged) or
`plan`. A plan approval carries a `Verification` clause: one or more concrete
shell commands that must be executed and succeed. Free-form prose is
deliberately not representable — it cannot be checked, so a model's claim could
stand in for evidence.

## Worktree isolation

Enabled with `worktree on` or per-call `isolation: "worktree"` on a `subagent`
task.

- Worktrees are created only for clean repositories.
- Successful runs merge back into the main workspace; `worktreeMerge` controls
  how.
- A conflict **or a failed run** retains the worktree so the foreman can inspect
  the worker's partial changes.
- Reviewer calls run against the merged workspace. A FAIL is fix-forward, not an
  automatic rollback.

## Process executor

`router=process`, or `executor: "process"` on a `subagent` task, runs each task
as a child `pir --mode json --no-session` process
(`crates/pir-core/src/plugins/subagent/process.rs`). It parses the JSON event
stream and enforces the same turn/wall-clock/abort budgets as the inline path.

- The parent's provider-scoped credential is inherited through
  `PIR_SUBAGENT_API_KEY` — an env var, never an argv secret.
- `PIR_SUBAGENT_DEPTH` carries the nesting depth across the process boundary so
  the global cap stays effective.
- A parallel batch returns an error if any leg fails; a broken worker is never
  reported as a successful batch.

## Detached runs

`pir foreman run/status/logs/abort/approve/deny/resume/clean` reuses the `jobs`
supervisor (`pir jobs`). A detached run is a normal `pir --mode json` process;
the mode plugin restores `foreman` from the `.mode.json` sidecar, so no separate
daemon protocol exists.

Manual approvals are asynchronous: the first run may settle at the blocked
delegation, then `approve`/`deny` appends the decision and starts a new run.
There is no long-lived multi-client daemon and no live TUI attach —
`logs --follow` plus `resume` cover monitoring and continuation.

## Plan mode

`/mode plan` keeps the main session read-only and exposes `submit_plan`. On
approval the run hands off to normal execution, and the plan text and decision
are stored durably in the journal. Detached plan approvals switch both the
journal and the sidecar to normal mode.

## Known constraints

- **Approval waiting blocks the turn.** The approval gate returns
  `terminate: false` and asks the model (via prompt/notice text) to end its
  turn; the decision then arrives as a follow-up. Making the block
  `terminate: true` was tried and reverted: tool-call termination is a batch
  hint, not a suspend/resume primitive.
- **Headless background completion is resolved.** A background tool call that
  finishes after the terminal model turn used to be persisted but never
  consumed. The print host now does a bounded `wait_for_idle` before settling
  and, when a completion arrived, re-projects the session and runs one more
  turn (`should_replay_background`, capped by
  `AGENT_BEFORE_SETTLE_CONTINUATION_CAP`).
- **The durable engine is not wired to production.** `crates/pir-durable/`
  is a high-fidelity port of `pi-durable` with committed state, a
  checkpointed task state machine (`Waiting{on}`/`Sleeping{until}`/`Wake`), a
  submission inbox and background tasks — conceptually the right engine for
  "suspend on an event, wake, resume from a checkpoint". In practice every
  `crate::durable` consumer is a test, bench or example; the crate describes
  itself as a second harness beside `agent_loop`/`agent_session`, not the engine
  beneath them. Moving foreman onto it would be a rewrite of run orchestration,
  so the local bounded wait-and-resume above was chosen instead. Durable stays
  the right long-term target only if the product goal becomes a unified,
  crash-recoverable run/subagent/background/approval orchestrator.
