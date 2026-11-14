# Durable specification (pir)

Normative reference for `src/durable/`: a **pir-side digest** of upstream `@earendil-works/pi-durable`
`docs/spec.md`. The reference tree is upstream commit `38ff99177` (package `1.4.0`); the digest began from
`2ec55d808` (package `1.3.0`) and was re-checked against `38ff99177` for the ported modules. Upstream remains
authoritative for anything not restated here; §12 lists deliberate Rust differences.

```text
upstream package   pi/packages/durable
upstream spec      pi/packages/durable/docs/spec.md
upstream revision  38ff99177 (package 1.4.0; digest started at 2ec55d808, package 1.3.0)
pir module         src/durable/
```

**Positioning.** pir mirrors upstream's sibling split (`src/durable/` ↔ `pi-durable`): this module is a **second
harness beside** `agent_loop`/`agent_session`, never the engine under them (see `../RESULTS.md`); do not stack the
two turn drivers on one conversation.

## Rust quickstart (developer usage)

`pir::durable` is core: it needs no cargo feature and is **not** re-exported by `pir::sdk`, so an embed imports it
directly (`use pir::durable::…`). Upstream `@earendil-works/pi-durable` is likewise a library; there is no CLI
command on either side. A runnable external-crate example is
`.play/lanes/lane-sdk-dev/apps/durable-probe/` (JSONL open, root + entries, fork, reopen, torn-line recovery,
document migration/checkpoint). The in-tree `examples/dev_durable_timers.rs` is the minimal host-timer example
below (`cargo run --example dev_durable_timers --no-default-features`); `examples/dev_durable_reports.rs`
(`cargo run --example dev_durable_reports --no-default-features`) shows `HarnessOptions.on_report` versus the
timer report sink and the `TaskRuntime::set_timer` phase lifecycle.

```rust
use std::sync::Arc;
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::{ConversationOwnership, Session, SessionOptions};
use pir::durable::{Context, JsonlStorage, Storage};
use serde_json::json;

let ctx = Context::background();
let storage: Arc<dyn Storage> = Arc::new(JsonlStorage::open(&root).await?);
let session = Session::open(storage.clone(), SessionOptions::default());

let root_conv = session.create_root_conversation(ctx.clone()).await?; // id 1
let first = session
    .append_entry(
        root_conv.id,
        EntryDraft {
            kind: "user".to_owned(),
            model: Some(vec![json!({"role": "user", "content": "hello"})]),
            data: Some(json!({"text": "hello"})),
            head: None,
            edits: None,
        },
        ctx.clone(),
    )
    .await?;

// The child inherits entries up to `first`; its `parent` records (conversation, entry).
let child = session
    .create_conversation(
        ConversationOwnership::Ownerless,
        Some((root_conv.id, first.id)),
        ctx.clone(),
    )
    .await?;

// Resume: close both, reopen the same directory, and read the same ids/entries.
session.close().await?;
storage.close().await?;
```

Reads use the table helpers, e.g. `session.scan_entries(EntryQuery { conversation_id, min_entry_id: None,
max_entry_id: None, order: Some(ScanOrder::Ascending) }, limit, cursor, &ctx)`; `session.conversation(id, &ctx)`
returns the record (including `parent`) after reopen. Every committed batch is one `main.jsonl` marker line.

**JSONL recovery and limits.** Opening a JSONL root truncates an unterminated final line to the last `0x0a` and
drops unconfirmed sidecar tails; every committed entry/record survives. `main.jsonl` is never compacted. With the
default `JsonlStorageOptions { fsync: false }` the guarantee is process-crash consistency, not power-loss
durability; storage errors fail the session and it must be reopened. The caller always supplies the storage root —
the module never reads or writes `~/.pi`.

**Documents.** `Session::doc(resolved, definition, ctx, |draft| …)` loads (migrating an older stored version),
applies the edit and commits atomically. Build the definition with
`CommonDocDefinition::session(kind, version)?.behaviour(initial, migrate, checkpoint_when)` (or a
conversation/task-scoped constructor), and resolve the address with `ResolvedAddress::new(kind, semantics, owner,
key)`. A version change stores a base even with no operations; a same-version change stores a delta unless the
definition's `checkpoint_when(&value)` returns true, which upgrades it to a base. `migrate` is
`fn(from_version: i64, value: serde_json::Value) -> Result<serde_json::Value, DocumentError>` in Rust (upstream is
`(value, fromVersion)`). `Session::doc` carries the identity only: to have `checkpoint_when` consulted, install a
`DocBehaviours` resolver in `SessionOptions.behaviours` (the harness does this for its built-in documents);
without one no predicate runs, but version-change bases still apply.

**Tasks and timers.** `Session::create_task`/`set_task` store task state machines; timer plumbing lives in
`durable::timers` (`Timers`, `native_timer`) and the scheduler that runs tasks/phases is
`durable::harness::Harness` (settings, registry, submissions, sleeping/wake). The lower-level `Session` kernel
does not run tasks by itself.

The two `define_task` functions are **not interchangeable**. `durable::tasks` is the metadata surface the
agent-hooks registry stores: `TaskDefinition::define(kind, version, phases)` plus
`tasks::define_task(definition)` produce a registry token (`TaskDefinition { kind, version, phases }`), not
phase handlers. Executable definitions are built with `durable::harness::task::define_task(name, version,
initial).phase(...).build()`; `TaskScheduler` runs those.

**Host timers (`HarnessOptions.timers`).** A host supplies a `SetTimer` and a report sink; the same `Timers`
value feeds the scheduler (sleep/wake) and `TaskRuntime::set_timer`:

```rust
use std::sync::Arc;
use pir::durable::harness::{create_registry, Harness, HarnessOptions};
use pir::durable::timers::{SetTimer, TimerCallback, TimerCancel, TimerOptions, Timers};
use pir::durable::Context;

struct HostTimer;

impl SetTimer for HostTimer {
    fn set(
        &self,
        callback: TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        if options.background {
            return None; // decline: background timers only free memory
        }
        // Arm `callback` after `delay_ms` with the host's own clock and return
        // its disarm token; `None` here would fall back to the native timer.
        // The host must tolerate a disarm after the callback already ran.
        let _ = (callback, delay_ms);
        Some(TimerCancel::new(Box::new(|| { /* disarm the host timer */ })))
    }
}

let report: Arc<dyn Fn(String) + Send + Sync> =
    Arc::new(|message| eprintln!("durable timer: {message}"));
let timers = Timers::host_timers(Some(Arc::new(HostTimer)), report);
let harness = Harness::open(
    storage,
    HarnessOptions {
        registry: Some(Arc::new(create_registry())),
        timers: Some(timers),
        ..HarnessOptions::default()
    },
    Context::background(),
)
.await?;
```

`Timers::host_timers(None, report)` (equivalent to `Timers::native(report)`) uses the native timer directly.
A declined **foreground** timer falls back to the native timer and the report closure receives
`HarnessOptions.setTimer declined a timer that paces work; using the native timer` once per `Timers` value; a
declined **background** timer stays declined and is not reported. A panic inside `set` is treated exactly like
`None`. `TaskRuntime::set_timer(callback, delay_ms)` is the in-phase form: the invocation's end disarms every
timer it armed, a call after the end rejects, and a late fire is a no-op. The timer `report` sink is separate
from `HarnessOptions.on_report`, which receives `(&str, &ReportInfo)` for harness failures. When
`HarnessOptions.timers` is omitted, the scheduler wraps the native timer with a report closure that routes a
timer-callback failure to `on_report` as `ReportInfo::HostError { callback: HostCallback::SetTimer, .. }`
(`src/durable/harness/scheduler/mod.rs:1939-1961`; upstream's `hostTimers` wiring); when a `Timers` value is
supplied, its own `report` closure is the only sink for that message. When a host arms the
callback on its own thread, that thread must re-enter the Tokio runtime before invoking it (`Handle::enter()`):
the scheduler wake path calls `tokio::spawn`, so a bare thread panics with "there is no reactor running". The
in-tree `examples/dev_durable_timers.rs` shows the thread + `Handle::enter()` shape end to end.

`examples/dev_durable_reports.rs` asserts the two sinks stay distinct: `runtime.report(...)` arrives at
`on_report` as `ReportInfo::TaskError`, while the fixed decline text arrives only at the timer report sink.
The same example shows the phase-scoped `set_timer` lifecycle: the invocation end disarms the timer (both
`TimerHandle::is_cancelled()` and the host cancel token run), and `set_timer` or `report` called after the
phase ended returns `TaskRuntimeError::Ended`.

## 1. Terms and invariants

- A **session** owns one mutation line, conversations, entries, tasks, submissions and documents.
- A **conversation** is a transcript scope and may fork another; an **entry** is an immutable transcript record; a
  **task** is a durable state machine attached to one conversation; a **document** is mutable JSON state stored as
  a base plus delta operations.
- A **turn** is one assistant response and the tool calls it makes; a **run** is the sequence of turns from an
  admitted input to the final answer.

The core rule: a session atomically commits immutable entries, full task records and tracked documents; only
committed state is observable.

Required invariants (upstream §1, implemented in `session/`):

1. One commit is atomic across every record and document write.
2. A document update is published only after its storage commit succeeded.
3. Every visible progress is durable; there is no volatile publication path.
4. External effects never run inside the mutation transaction.
5. Entries and IDs are immutable and never reused after a committed write.
6. Document drafts are fully revoked when their transaction callback settles.
7. The mutation line is held through storage settlement and committed-state adoption; commit observers run
   synchronously only to capture immutable state, document watchers later, off the line.
8. A storage error is fatal to the open session: it publishes nothing and must be reopened.

## 2. Core records

IDs and sequences are plain numbers. `ConversationId`, `EntryId`, `TaskId`, `SubmissionId`, `DocumentId` are brand
types over `i64` (`durable::ids::Id<K>`); `Seq` is a separate brand so an ID can never be used as a document commit
point. `ROOT_CONVERSATION_ID = 1`.

```rust
ConversationRecord { id, parent?: { conversation_id, at: EntryId }, owner?: { conversation_id, task_id } }
EntryRecord { id, conversation_id, kind, model?: Vec<JsonValue>, data?: JsonValue,
              head?: EntryId, edits?: Vec<ContextEdit>, by_task_id?: TaskId }
TaskRecord { id, conversation_id, kind, version, input, owner?, background,
             abort_requested, abort_reason?, abandon_on_restart?, started_at?,
             ended_at?, restarts?, state, memos? }
SubmissionRecord { id, conversation_id, request_id?, type: input|write,
                   status: queued|placed|done|unanswered, entry?, answer?, reason?, detail? }
```

`TaskRecord` and `SubmissionRecord` additionally carry optional `trace`/`links` fields (propagation carriers;
omitted when empty, see `src/durable/types.rs:533-552`).

Conversation history parenting (`parent`) and task ownership (`owner`) are separate: `parent` controls inherited
entries and documents; `owner` records task attribution and connects scopes for subtree abort and idle traversal.

### 2.1 Entries and context

The active transcript is the raw entry range from the newest applicable `head` through the tail. Context derivation
(`harness::context::derive_context`):

1. Find the newest visible entry `H` at or before the cutoff that has `head`; let `from = H.head` (or transcript
   start when `H` is absent) and scan visible entries from `from` through the cutoff.
2. The newest edit per target wins: `omit` contributes nothing, `replace` contributes its messages instead of the
   target's. If `H` exists, context entries are `H` followed by non-head entries.
3. Keep every positional system message and its tool/section changes; move each assistant's tool results directly
   after it, in tool-call order.
4. Synthesize an error result for every call without one; drop results with no preceding call.
5. Exclude model-less entries and assistant messages with `aborted`, `error` or `deferred` stop reasons.
6. Move the first system message to the front when only user messages precede it, or it is the baseline.

## 3. Documents

A definition declares `kind` (public protocol, never renamed), `version` (positive integer, migrated on access),
scope (`session`/`conversation`/`task`), history (`latest`/`rewindable`), fork policy (`asOf`/`current`/`initial`),
`initial()`, optional `migrate(from, value)` and `checkpoint_when(value)`.

- A version change stores a **base**, even with no operations.
- `checkpoint_when` runs once during assembly, after every validation; true turns the delta into a base.
- Retirement is a `retiredAt` stamp; records are never reused. A retired incarnation's observers get a `null` root
  replacement, then close.
- Fork copies read the source's **stored** base and are definition-free: the child never runs definition code at
  fork time.

## 4. Transactions and storage ownership

One commit callback per transaction. Table reads are allowed before the first table write; any table read after a
table write is `ReadAfterWrite`. Document access and read-your-writes stay available either way. A transaction
stages conversation creates/forks, entry appends, task creates/replacements, submission
creates/settlements/placements, and document drafts; at settlement it either aborts every draft (callback or
assembly failure) or produces one atomic write batch.

Storage owns atomicity, global ID ownership, immutable conversation/entry creation, one live document incarnation
per address, content before retirement and detached values. The session owns the mutation line; a backend adds no
second caller-facing commit mutex but must make one admitted batch atomic.

## 5. Tasks

```text
state: pending | running | waiting { on, policy } | sleeping { until }
     | completing { outcome } | terminal { outcome }
outcome: completed { result } | failed { error, result? } | aborted { reason?, result? }
       | orphaned { reason } | faulted { error }
```

### 5.1 Effect sandwich

```text
commit intent phase → perform the external effect → commit outcome or next phase
```

Reopening in an intent phase means the effect may have happened: the phase handler retries safely, polls an
external handle, or records interruption.

### 5.2 Terminal tasks

A terminal transition atomically writes the terminal record, appends any result entries, retires every task-scoped
document, and resolves any submissions the task settled. `startedAt` is stamped at the first change to `running`,
`endedAt` at the change to `terminal`; both survive waits and reopen.

### 5.3 State, abort and ownership

The scheduler serially reserves eligible tasks, then runs handlers off the session line; one in-memory invocation
per task holds its abort controller and completion signal. Abort protocol: commit `abortRequested`, signal and join
the active run invocation, schedule the abort invocation. Abort is **bottom-up**: an abort invocation starts only
once the task's ordinary owned work drained.

### 5.4 Structured concurrency

Tasks and conversations form one ownership tree. **Ordinary owned work** of `T` is every task reached below `T`
excluding background subtrees. Two rules:

- **Abort flows down.** A live owner's cancellation intent marks its ordinary owned work; background tasks are
  boundaries.
- **Owned work does not outlive its owner's finish.** A terminal state committed while ordinary owned work is live
  is stored as `completing`; the final `terminal` commit follows once that work drains, evaluated on the commit's
  candidate records.

Waiting: a phase may commit `{ status: waiting, checkpoint, on, policy }`; a cycle in the wait-for graph is
rejected, and `failFast` marks every other live task in `on` (the one that already holds or ended with a
non-`completed` outcome keeps its own outcome) via `src/durable/harness/scheduler/reconcile.rs:123-135`.

### 5.5 Sleeping, timers and wakeups

A phase may commit `{ status: sleeping, checkpoint, until }`. The task holds no invocation and resumes at its
checkpoint when the harness clock reaches `until`; the deadline survives any number of reopens, an abort mark ends
a sleep early, and there is no sleep inside a phase. `Harness.wake()` reports `{ active, at }` for hosts that arm
their own wakeup (`onWake` / `resume()`).

### 5.6 Crash-loop guard

Open counts the restarts of each task cut off by a crash, eviction or reset (`restarts`). A task at `restartLimit`
is parked: blocked with reason `restart_limit`, not run, reported once. `retryTask()` runs it again. A close writes
the tasks it stopped back as `pending` **uncounted**.

## 6. Submissions and inbox

Submission records back awaitable host objects. Admission is harness-internal; boundaries place queued submissions,
run tasks settle the inputs they answer, and queued items wait in `pi.inbox` as tagged input or write items.

Boundary table (upstream §6): a `postTools` boundary places all writes and the steer items selected by
`steeringMode`; a `final` boundary places writes, steer and follow-up items. A display-only write (`kind` + `data`)
is appended at once, except behind a queued reset. `requestId` deduplicates within one conversation;
`afterFailedRun` decides what a failed run does with inputs queued behind it (`keep`, `continue`, `withdraw`); a
head write whose target is older than the active range start is `stale`.

## 7. Extensions, hooks and tools

An extension is a named bundle of tools, prompt sections, hooks, tool/section wrappers and task definitions in an
application-owned registry; a conversation selects extensions by name in its `pi.agent` document. Extensions
resolve to an `Agent` per request: model, thinking level, offered tools, nested-callable tools, deferred loadable
tools, prompt sections and instructions.

Tools return three channels — `output` for the model, `structuredOutput` for programs, `details` for UIs — plus
diagnostics, usage and `control` (`addTools`, `terminate`, `handoff`); a tool declares a replay policy (`safe` /
`unsafe`) deciding whether a crash-interrupted execution reruns.

## 8. Built-in tasks

| kind | responsibility |
|---|---|
| `pi.generation` | prepare system prompt and tools, request or poll the model, retry, classify the response |
| `pi.tool` | validate, hook, execute, persist output and details, append the result |
| `pi.compaction` | select a transcript range, summarize, place a headed summary |

Built-in entry kinds: `pi.user`, `pi.assistant`, `pi.system`, `pi.tool-result`, `pi.reset`, `pi.compaction`. Live
documents: `pi.agent`, `pi.live`, `pi.inbox`, `pi.provider`, `pi.usage`, and the task-scoped nested-call family.

## 9. Observation and views

`documentState` / `watchDoc` attach to one incarnation and receive its exact committed revisions. A watch never
invokes user code inline or runs two callbacks at once, and after 100 undelivered frames replaces the pending
suffix with one root replacement; a version change arrives as a root replacement, and close ends every state and
watch. The conversation view and task graph are structural mounts from committed publications only.

## 10. Storage contract

```rust
trait Storage: Send + Sync {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<Result<Seq, StorageError>>;
    fn mint_id(&self) -> StorageFuture<Result<i64, StorageError>>;
    // conversation / scan_conversations / entry / entry_in / find_latest_head_marker / scan_entries
    // task / scan_tasks / submission / scan_submissions / submission_by_request
    // find_document / document / scan_documents / close
}
```

Ordered scans are cursor-based; a cursor continues in the order it was created with and rejects a query asking for
the other. ID order: ascending is oldest first; defaults are entries newest first, everything else oldest first. A
`document.copy` reads committed pre-batch source state and persists one independent child base; deltas cannot cross
a stored version boundary.

A `StorageWrite::Task` carries its record behind an `Arc<TaskRecord>` (PIR-BIN-023): the transaction's staged
candidate, the assembled batch and the commit publication share one payload, so a task commit never deep-copies
the input/checkpoint. The serialized form is unchanged (serde `rc`), and the storage backends still clone once
into their own containers where they need owned rows.

### 10.1 Failures

A storage error ends the session: every later and in-flight call fails with `SessionFailed`, nothing is retried.
Only a read returning `StorageRequestError`, or whose caller's context was aborted, fails just that call; a read
after close fails with `StorageError::Closed` (`session/guard.rs:258`; `SessionClosed` is the awaitable end type,
not the error).

## 11. Backends

- **Memory** is the reference semantics: it copies retained write values and all read results.
- **JSONL** writes sidecar records first, then one main marker per commit; without `fsync` it promises
  process-crash consistency only. Recovery removes torn final lines, drops unconfirmed sidecar tails, and poisons
  the backend on any uncertain append failure; `main.jsonl` is never compacted. An mmap/COW evaluation of this
  backend (measured on an 18k-line/8 MiB `main.jsonl`) concluded **mmap is NOT integrated for recovery reads**:
  one-pass recovery over the existing streaming reader is faster (mmap wall +48%, page faults 55×), and Windows
  cannot truncate while a read-only map is alive; `Arc<Mmap>`+`Cow` shared-prefix snapshots remain proven only as
  prototypes (`session_manager`'s tail-page/`load_entries_from_file_mapped` readers are the
  agent-session-side mmap users, not `durable`).
- **SQLite** is implemented (`src/durable/storage/sqlite/`) as a third backend behind the optional `rusqlite`
  driver (compiled with the `extensions` or `context-mode` feature, `src/durable/storage/mod.rs:11-12`); WAL with
  `synchronous = NORMAL` unless `SqliteStorageOptions::fsync` upgrades it to `FULL`
  (`src/durable/storage/sqlite/storage.rs:205-211`). Out of scope: namespaced `prefix`,
  Cloudflare Durable Object adapter, `database`/`node` facades; full `packages/chord` stays deferred (delta subset
  only), chord-lite unscheduled.

## 12. Rust deviations (deliberate, recorded)

Each deviation is explained in the code where it lives.

1. **`Storage` has no `Context` parameter.** A local backend never observes upstream's abandon signal; an admitted
   commit settles regardless of caller cancellation. The session never wraps settlement in a cancel
   (`session/guard.rs`).
2. **`mint_id` returns the raw number.** It is branded at the call site; a generic method would break object safety
   while the harness holds `Arc<dyn Storage>`.
3. **Object-safe boxed futures.** The trait returns `StorageFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send +
   'a>>` instead of `async fn`, for the same reason.
4. **No chord tracker.** Upstream diffs a per-document draft in `O(changed)`; the port keeps the committed value,
   forks a full draft, and diffs at assembly with `chord::diff_objects` (the object form; `diff_revisions` is the
   `Value` wrapper and is used by the harness JSON helpers). Op vocabulary is identical, cost `O(value)`, bounded
   because live documents stay small (tool-output windows bounded by `OutputLimits`, default 50 KiB / 2000 lines,
   and nested-call error summaries capped at 500 chars, `harness/tool.rs:157`).
5. **UTF-16 string units.** `Truncate` counts UTF-16 code units and the differ emits UTF-16 lengths, matching
   `String.prototype.length`/`.slice`; a split surrogate becomes U+FFFD (the lossy decode of the same slice).
6. **No `~/.pi` writes.** The caller supplies the storage root.
7. **Zero new dependencies.** serde, serde_json, thiserror, tokio and tokio-util were already in the dependency
   graph (the harness additionally uses the already-present `futures-util`; the SQLite backend rides the existing
   optional `rusqlite` behind `extensions`/`context-mode`, `src/durable/storage/mod.rs:11-12`).
8. **`env/**`, `images/**`, `testing/**`, `tools/**`, `codemode/**` are out of this port.** pir has native
   equivalents for the latter five; the SQLite backend is ported separately under `storage/sqlite/`.
9. **JSONL persists minted IDs (`ids.jsonl`).** Upstream JSONL keeps only an in-memory watermark, so a minted but
   uncommitted ID is handed out again after reopen (verified against Node: `first=2, second=2`). pir appends every
   mint to `ids.jsonl` and seeds the watermark from it, so ID ownership stays globally monotonic across reopen,
   matching SQLite's `durable_metadata.next_id`.
10. **JSONL `fsync` also flushes the main marker.** Upstream flushes only sidecars before appending the marker; pir
    flushes the marker as well (one extra sync per commit), which is strictly stronger.
11. **JSONL compacts live-task sidecars.** When another task record would grow `task-<id>.jsonl` past 32 KiB
    (`TASK_SIDECAR_COMPACT_BYTES`), the sidecar is rewritten to its newest complete record; upstream keeps every
    replacement record. Recovery semantics are unchanged because a task record is a complete replacement.
12. **SQLite mints persist and submission request keys are unique.** `mint_id` advances
    `durable_metadata.next_id` before returning (upstream advances an in-memory counter only), and schema version 3
    replaces the non-unique `(conversation_id, request_id)` index with a partial unique one after deleting older
    duplicates. Upstream ships schema version 2.
13. **Built-in documents are staged agent-first.** `conversation_definitions()`
    (`src/durable/harness/documents.rs`) lists `pi.agent, pi.live, pi.inbox, pi.provider, pi.usage`, so a new
    conversation's one creation commit writes them in that order; upstream's `conversationCreated`
    (`harness/harness.ts`) stages `pi.live, pi.inbox, pi.usage, pi.provider` and then `pi.agent` via
    `createAgent`. The set, the ids the set mints, and every definition's semantics are identical — only the
    `ordinal`s inside the creation commit (and therefore the doc sidecar file-name ↔ kind mapping, e.g. which of
    `doc-2..doc-6.jsonl` is `pi.agent`) differ. Nothing reads creation order back.
14. **`pi.provider`'s session id is minted lazily as a UUIDv4.** Upstream's `ProviderDoc.initial()` calls
    `uuidv7()` per incarnation, so a fresh conversation stores its session id in the creation commit. pir's
    `DocInitial` is a plain function pointer that cannot capture the conversation id, so the definition's initial
    value is `{}` (`tests/suites/durable_harness_documents.rs:130-132`) and the id is written later:
    `generation.rs:991` stages a `uuid::Uuid::new_v4()` in the generation task's own commit
    (`ensure_provider_session_id`), and `Harness::provider_session_id` /
    `provider::mint_session_id` mint the UUIDv7-shaped fallback for direct harness callers. Same wire field
    (`sessionId`), same first-writer-wins semantics; the value's shape and the commit it first appear in differ.
    Verified A/B against upstream `1.4.0` (`pi/s2` vs `pir/round4` scenario roots, 2026-11-11): pir's creation
    commit stores `pi.provider` as `{}`, upstream stores `{ sessionId: <uuidv7> }`.
