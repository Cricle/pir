# `context-mode` builtin — think-in-code execution + FTS5 knowledge base

`builtin:context-mode` is the native Rust port of the `context-mode` npm plugin. The model writes
Python that runs in the embedded RustPython sandbox; only stdout enters the transcript. Raw file
bytes, fetched pages and indexed sections stay in the sandbox/SQLite and are recalled by topic with
`ctx_search`.

- Implementation: `src/plugins/builtin/context_mode/{mod,exec,index,fetch}.rs`; feature
  `context-mode` (implies `python`); entry `src/plugins/builtin.rs`
  (`name: "context-mode"`, `replaceable: true`, default-enabled).
- Tests: module unit tests; `tests/suites/` context-mode cases; real-play review
  `.play/reviews/PIR-BIN-context-mode.md`.

## 1. Tools (9)

All tools are native builtins; no npm/@anthropic/cloud dependency. Schemas are exact in the source
`parameters()` implementations; the table lists the user-facing contract.

| tool | key parameters | behavior |
| --- | --- | --- |
| `ctx_execute` | `code` (required), `timeout` (default 30s, clamp 120), `intent` | Fresh Python namespace per call; stdout/stderr capped 1 MiB each; uncaught exception → traceback error. If `intent` is set and stdout > 5 KiB, output is indexed and only matching section titles + snippets return. |
| `ctx_execute_file` | `path` (required), `code` (required), `timeout`, `intent` | Loads one file into `FILE_CONTENT`; path must resolve inside the session cwd (`..`, symlink escapes, non-files, >32 MiB rejected). Same output caps/intent behavior. |
| `ctx_batch_execute` | `scripts` (1-16, each `code` + optional `label`), `queries` (≤32), `source`, `timeout` | Runs scripts sequentially, one transaction per script; each stdout auto-indexed as `<source>#<label>`; optional queries return top sections in the same round trip. A failing script does not abort the batch (`errorPreview`). |
| `ctx_index` | `content` XOR `path`, `source`, `contentType` | Indexes markdown (heading split, fenced code kept) or text (~2 KiB chunks). Directories: depth 5, 200 files, 4096 entries/dir, 2048 dirs, skip node_modules/.git/dist/build/.next/coverage/.venv/__pycache__, no symlinks, root .gitignore respected. |
| `ctx_search` | `queries` or `query`, `source`, `contentType`, `limit` (default 3, clamp 1..20) | BM25 over the FTS5 KB, title hits weighted 5x, porter stemming; deduplicated chunks; `{queries:[{query,results:[...]}]}`. Empty KB → error telling you to index first. |
| `ctx_fetch_and_index` | URL + source/content type | Fetches, converts to text, indexes; raw page never echoed. |
| `ctx_stats` | none | Process counters + knowledge-base statistics. |
| `ctx_purge` | `confirm: true` required | Deletes indexed chunks. |
| `ctx_doctor` | none | Read-only local diagnostics: platform, Python status/spawn count, DB path/size/WAL, FTS5 availability/schema version, chunk/source counts, hook switches/thresholds/counters/last error. Never starts the Python worker or creates the DB; absent KB reads as zero with `exists=false`. |

Intentionally absent: upstream `ctx_upgrade` (npm package updater) and `ctx_insight` (vendor
hosted analytics) have no local meaning; `ctx_doctor` is the local diagnostic equivalent. The
sandbox is embedded RustPython (stdlib only, no C extensions), not the user's native `python3`.

## 2. Hooks

`events()` declares exactly three, all default-on:

- **`session_start`** — records session id/cwd for the source label; never opens the DB.
- **`tool_call`** (PreToolUse routing) — blocks `curl`/`wget`/PowerShell `Invoke-WebRequest`/`iwr`
  with guidance to `ctx_fetch_and_index`/`ctx_execute`; blocks simple `cat`/`head`/`tail`/`less`
  or `read`/`read_file` of a single non-image file larger than `ROUTING_FILE_THRESHOLD_BYTES`
  (64 KiB) with guidance to `ctx_execute_file`. Only unambiguous single-file commands are
  inspected (no pipelines/redirections/globs); `ctx_*` calls and images are untouched. The block
  uses the standard `{block:true, reason}` merge, so the model sees an error and the tool never
  runs.
- **`tool_result`** (PostToolUse auto-index) — a finalized text result larger than
  `TOOL_RESULT_INDEX_THRESHOLD_BYTES` (20 KiB) is indexed under `session:<session id>` (fallback
  `tool:<name>`), bounded to `TOOL_RESULT_INDEX_MAX_BYTES` (1 MiB). The payload is never mutated.

Switches: `PIR_CONTEXT_MODE_ROUTING=0` disables blocking; `PIR_CONTEXT_MODE_INDEX_RESULTS=0`
disables auto-indexing (`false`/`off`/`no` also accepted).

## 3. Default plan / laziness / errors

- Default plan: enabled. Disable with `disabledBuiltinExtensions: ["context-mode"]`; without the
  feature the `ctx_*` tools are absent and `builtin:context-mode` reports the unknown/not-compiled
  error.
- Constructing the plugin allocates tool objects and shared state only: no Python worker/VM and no
  SQLite open. The VM starts on first execute; the DB opens on first index/search/stats/purge/
  fetch. `on_shutdown` closes the DB.
- Errors are typed tool errors (`isError: true`): missing/empty `code`, missing `path`, invalid
  `ctx_index` `content`/`path` combination, path outside cwd, unsupported file extension, empty KB
  search, purge without `confirm`, Python traceback. A failed batch script is reported in
  `errorPreview` without aborting siblings.
