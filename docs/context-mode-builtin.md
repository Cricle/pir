# `context-mode` builtin — think-in-code execution + FTS5 knowledge base

`builtin:context-mode` is the native Rust port of the `context-mode` npm plugin. The model writes
JavaScript (or TypeScript) that runs in the embedded QuickJS sandbox — the same engine and node
compatibility suite `pir js` and JS extensions use — and only stdout enters the transcript. Raw file
bytes, fetched pages and indexed sections stay in the sandbox/SQLite and are recalled by topic with
`ctx_search`.

- Pure logic: `crates/pir-builtin-context-mode/src/{batch,fetch,chunk,journal}.rs` (request
  validation, HTTP normalization, HTML→markdown, document chunking — published, engine-free).
  Runtime half: `crates/pir-core/src/plugins/builtin/context_mode/{mod,exec,index,js,resume}.rs`
  (QuickJS sandbox, SQLite/FTS5 index); `batch.rs`/`fetch.rs` are thin adapters over the crate.
  Feature `context-mode` (implies `extensions`); entry `crates/pir-core/src/plugins/builtin.rs`
  (`name: "context-mode"`, `replaceable: true`, default-enabled).
- Tests: `crates/pir-builtin-context-mode/tests/*.rs` (the pure helpers); `tests/suites/`
  context-mode cases; real-play review `.play/reviews/PIR-BIN-context-mode.md`.

## 1. Tools (12)

All tools are native builtins; no npm/@anthropic/cloud dependency. Schemas are exact in the source
`parameters()` implementations; the table lists the user-facing contract.

| tool | key parameters | behavior |
| --- | --- | --- |
| `ctx_execute_tool` | `name` (required), `args` (required), `background` | Invokes one **registered callable** tool through the normal nested scheduler, so the call inherits the exposure gate, mutation locks, hooks, abort signal and nested-depth limit. `ctx_*` tools are refused (no self-recursion) and it only works from inside a running tool call. The nested call id is attached to `details.nestedToolCallId`. |

| tool | key parameters | behavior |
| --- | --- | --- |
| `ctx_execute` | `code` (required), `language` (`javascript` default, `typescript`), `file` (optional, upstream shape), `timeoutMs` / `timeout` (default 30s, clamp 120), `intent` | Fresh module per call on the persistent runtime; `file` exposes a confined path as `process.env.CTX_FILE` / `FILE_PATH` (same confinement as `ctx_execute_file`); stdout/stderr capped 1 MiB each; uncaught exception or nonzero exit → error stack. If `intent` is set and stdout > 5 KiB, output is indexed and only matching section titles + snippets return. |
| `ctx_execute_file` | `path` (required), `code` (required), `language`, `timeout`, `intent` | Exposes the confined file as `process.env.CTX_FILE` / `FILE_PATH`; path must resolve inside the session cwd (`..`, symlink escapes, non-files, >32 MiB rejected). Same output caps/intent behavior. |
| `ctx_batch_execute` | `scripts` (1-16, each `code` + optional `label`), `language`, `queries` (≤32), `source`, `timeoutMs` / `timeout` | Runs JS scripts sequentially, one transaction per script; each stdout auto-indexed as `<source>#<label>`; optional queries return top sections in the same round trip. A failing script does not abort the batch (`errorPreview`). |
| `ctx_batch` | `commands` (1-16 shell commands, required), `queries` (1-32, required), `timeoutMs` / `timeout` | Upstream shell batch: commands run sequentially in the session cwd through the resolved shell, every `$ <command>` block is indexed under one `ctx_batch:<hash>` source, and only sections matching `queries` come back (16 KiB reply cap). A failing command is indexed and does not stop the batch. |
| `ctx_index` | `content` XOR `path` XOR `url`, `source`, `contentType` | Indexes markdown (heading split, fenced code kept) or text (~2 KiB chunks); `url` fetches and converts a page first. Every row records the session and worktree that wrote it (schema v2 scope). Directories: depth 5, 200 files, 4096 entries/dir, 2048 dirs, skip node_modules/.git/dist/build/.next/coverage/.venv/__pycache__, no symlinks, root .gitignore respected. |
| `ctx_search` | `queries` or `query`, `scope` (`session`/`worktree` default/`project`), `source`, `contentType`, `limit` (default 3, clamp 1..20), `offset`, `sort` (`relevance`/`timeline`), `after` | BM25 over the FTS5 KB, title hits weighted 5x, porter stemming; deduplicated chunks; `{queries:[{query,results:[...]}]}`. `project` fuses BM25 with recency via reciprocal-rank fusion; `timeline` returns oldest-first and pages with the `after` epoch-ms cursor (each hit carries `indexedAt`); `offset` pages within a query. Empty KB → error telling you to index first. |
| `ctx_resume` | `session`, `seq` | Rebuilds this session's place after compaction: whole journal rows (newest/highest-weight first) within 2,048 bytes, errors first, then index writes, execution and other events. `seq` returns one journal row whole (the pointer form printed for rows that do not fit). |
| `ctx_fetch_and_index` | URL + source/content type | Fetches, converts to text, indexes; raw page never echoed. |
| `ctx_stats` | none | Process counters + knowledge-base statistics, journal usage rows for `session`, `project` and `allTime` (calls/bytes grouped by journal kind), and the active retention limits plus this session's stored rows/bytes (`retention`). |
| `ctx_purge` | `confirm: true` required, one of `source` / `session` / `project: true` | `source` removes that source's chunks and fetch cache; `session` removes that session's journal rows; `project: true` wipes chunks, sources, fetch cache and journal. With no scope the whole chunk store is removed. A present-but-unusable scope value is rejected, never widened. |
| `ctx_doctor` | none | Read-only local diagnostics: platform, JS sandbox status/spawn count, DB path/size/WAL, FTS5 availability/schema version, chunk/source counts, hook switches/thresholds/counters/last error, and the retention limits with this session's `overLimit` state. Never starts the JS runtime or creates the DB; absent KB reads as zero with `exists=false`. |

Intentionally absent: upstream `ctx_upgrade` (npm package updater) and `ctx_insight` (vendor
hosted analytics) have no local meaning; `ctx_doctor` is the local diagnostic equivalent. The
sandbox is the embedded QuickJS + node compatibility suite, not a spawned `node`/`python3`:
`javascript` and `typescript` run in-process, and shell work belongs to `ctx_batch` or `bash`.

## 2. Hooks

`events()` declares exactly three, all default-on:

- **`session_start`** — records session id/cwd, binds the worktree scope, and (when the host marks a
  compaction) seeds the compact recall path; never opens the DB.
- **`tool_call`** (PreToolUse routing) — blocks `curl`/`wget`/PowerShell `Invoke-WebRequest`/`iwr`
  with guidance to `ctx_fetch_and_index`/`ctx_execute`; blocks simple `cat`/`head`/`tail`/`less`
  or `read`/`read_file` of a single non-image file larger than `ROUTING_FILE_THRESHOLD_BYTES`
  (64 KiB) with guidance to `ctx_execute_file`. Only unambiguous single-file commands are
  inspected (no pipelines/redirections/globs); `ctx_*` calls and images are untouched. The block
  uses the standard `{block:true, reason}` merge, so the model sees an error and the tool never
  runs.
- **`tool_result`** (PostToolUse auto-index) — a finalized text result larger than
  `TOOL_RESULT_INDEX_THRESHOLD_BYTES` (20 KiB) is indexed under `session:<session id>` (fallback
  `tool:<name>`), bounded to `TOOL_RESULT_INDEX_MAX_BYTES` (1 MiB). The same payload is appended to
  the session journal (`tool_result`) so `ctx_resume` can point back at it; the payload is never
  mutated.

Switches: `PIR_CONTEXT_MODE_ROUTING=0` disables blocking; `PIR_CONTEXT_MODE_INDEX_RESULTS=0`
disables auto-indexing (`false`/`off`/`no` also accepted).

## 2a. Per-session retention (the database stays bounded)

The knowledge base is fed by hooks, so every limit is enforced **per session**: one session can
never evict another session's rows, and eviction is always newest-survives. Caps are applied inside
the write transaction (journal rows, chunk rows, fetch cache), so a long-lived project cannot grow
the `.db`/WAL without bound and a single huge result cannot fill the disk.

| material | limit | eviction |
| --- | --- | --- |
| journal (`events`) | 2,000 rows / session | oldest `seq` first |
| journal bytes | 32 MiB / session (`bytes` column sum) | oldest `seq` first |
| journal age | 7 days / session | rows older than the TTL |
| chunks | 4,000 rows / session | oldest `(indexed_at, id)` first |
| chunk text | 64 MiB / session (`LENGTH(title)+LENGTH(content)`) | oldest `(indexed_at, id)` first |
| fetch cache | 256 rows / 64 MiB project-wide (raw pages are not session material) | oldest `fetched_at` first |

`ctx_doctor` and `ctx_stats` report the active limits plus this session's stored rows/bytes and an
`overLimit` flag (a session written by an older build reads over-limit until its next write trims
it). Manual `ctx_purge` semantics are unchanged; the automatic trims only drop material the caps
exclude.

## 3. Default plan / laziness / errors

- Default plan: enabled. Disable with `disabledBuiltinExtensions: ["context-mode"]`; without the
  feature the `ctx_*` tools are absent and `builtin:context-mode` reports the unknown/not-compiled
  error.
- Constructing the plugin allocates tool objects and shared state only: no JS runtime thread and no
  SQLite open. The runtime starts on first execute; the DB opens on first index/search/resume/
  stats/purge/fetch. `on_shutdown` stops the runtime and closes the DB.
- Errors are typed tool errors (`isError: true`): missing/empty `code`, unsupported `language`,
  missing `path`, invalid `ctx_index` `content`/`path`/`url` combination, path outside cwd,
  unsupported file extension, empty KB search, purge without `confirm`, `ctx_batch` without
  `commands` or `queries`, `ctx_resume` without a session or with an unknown `seq`, JavaScript
  stack, timeout. A failed batch command or script is reported (and indexed) without aborting
  siblings.

## 4. Deliberate deviations from upstream

The tool surface above is a superset of upstream v2 (which has no `ctx_execute_file`,
`ctx_batch_execute` or `ctx_fetch_and_index`; those stay as explicit legacy aliases). Other
differences are deliberate and user-visible:

- **PostToolUse is index-only.** Results over `TOOL_RESULT_INDEX_THRESHOLD_BYTES` (20 KiB, upstream
  triggers at 4 KiB) are indexed and journaled, but the payload the model sees is not replaced with
  selected chunks. This port always returns the tool's real output; the model retrieves the indexed
  copy with `ctx_search`. No content is ever silently dropped.
- **`ctx_execute` runs JavaScript/TypeScript only.** The sandbox is the embedded QuickJS + node
  compatibility suite; there is no embedded Python (deliberately removed) and shell work belongs to
  `ctx_batch` or the `bash` tool. Upstream's `python`/`powershell` execution languages therefore
  return the unsupported-language error here.
- **`ctx_search` replies with the JSON `{queries:[{query,results}]}` shape** advertised by
  `ctx_index`, not upstream's plain-text hit lines. It additionally accepts `limit` and the
  `query` shorthand; the scope/offset/sort/after semantics match.
- **`ctx_purge` keeps a `confirm: true` gate** on top of upstream's `source`/`session`/
  `project: true` scopes; omitting every scope still means "the whole chunk store" (legacy
  behavior) rather than upstream's no-scope error.
- **No automatic resume hint on `session_start`.** After a compaction the model calls `ctx_resume`
  itself (or the host can surface the block); this plugin never injects a message into the
  transcript from the hook.
- **Hook errors are recorded, never fatal.** Routing/index failures land in the doctor counters and
  the last-error field, mirroring upstream's "hooks always exit 0" policy.
