# Durable specification (pir)

Normative reference for `src/durable/`: a **pir-side digest** of upstream `@earendil-works/pi-durable`
`docs/spec.md` (upstream HEAD `2ec55d808`). Upstream remains authoritative for anything not restated here; §12
lists deliberate Rust differences.

```text
upstream package   pi/packages/durable
upstream spec      pi/packages/durable/docs/spec.md
pir module         src/durable/
```

**Positioning.** pir mirrors upstream's sibling split (`src/durable/` ↔ `pi-durable`): this module is a **second
harness beside** `agent_loop`/`agent_session`, never the engine under them (see `../RESULTS.md`); do not stack the
two turn drivers on one conversation.

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
rejected, and `failFast` marks the siblings of the first non-`completed` member.

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
    // conversation / entry / entry_in / find_latest_head_marker / scan_entries
    // task / scan_tasks / submission / scan_submissions / submission_by_request
    // find_document / document / scan_documents / close
}
```

Ordered scans are cursor-based; a cursor continues in the order it was created with and rejects a query asking for
the other. ID order: ascending is oldest first; defaults are entries newest first, everything else oldest first. A
`document.copy` reads committed pre-batch source state and persists one independent child base; deltas cannot cross
a stored version boundary.

### 10.1 Failures

A storage error ends the session: every later and in-flight call fails with `SessionFailed`, nothing is retried.
Only a read returning `StorageRequestError`, or whose caller's context was aborted, fails just that call; a read
after close fails with `SessionClosed`.

## 11. Backends

- **Memory** is the reference semantics: it copies retained write values and all read results.
- **JSONL** writes sidecar records first, then one main marker per commit; without `fsync` it promises
  process-crash consistency only. Recovery removes torn final lines, drops unconfirmed sidecar tails, and poisons
  the backend on any uncertain append failure; `main.jsonl` is never compacted.
- **SQLite** is implemented (`src/durable/storage/sqlite/`) as a third backend. Out of scope: namespaced `prefix`,
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
   forks a full draft, and diffs at assembly with `chord::diff_revisions`. Op vocabulary is identical, cost
   `O(value)`, bounded because live documents stay small (tool-output windows bounded, summaries ≤500 chars).
5. **UTF-16 string units.** `Truncate` counts UTF-16 code units and the differ emits UTF-16 lengths, matching
   `String.prototype.length`/`.slice`; a split surrogate becomes U+FFFD (the lossy decode of the same slice).
6. **No `~/.pi` writes.** The caller supplies the storage root.
7. **Zero new dependencies.** serde, serde_json, thiserror, tokio and tokio-util were already in the dependency
   graph.
8. **`env/**`, `images/**`, `testing/**`, `tools/**`, `codemode/**` are out of this port.** pir has native
   equivalents for the latter five; the SQLite backend is ported separately under `storage/sqlite/`.
