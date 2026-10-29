# Long-run soak harness (`scripts/soak/`)

Purpose: exercise the objective **"runs for months/years without degradation"**
in bounded, repeatable runs. The harnesses drive the real `pir` binary through
`--mode json --print`, so every turn is a real process/turn/session path, and
they fail loudly on the two degradations that matter in a long run:

* memory/process growth (leak, orphaned tool children);
* a request or turn that stops making progress (hang).

All three drivers are Python 3.7+ **stdlib only** (no pip installs). They reuse
the conventions of the existing probes rather than inventing a new stack:

| existing script | what is reused |
|---|---|
| `scripts/perf/big_session_probe.py` | CIM process-table → process-tree working set; valid v3 JSONL fixture writer; OpenAI-compatible mock SSE shape |
| `scripts/perf/rss_probe.py` | stdout/stderr redirected to files, stdin `DEVNULL` (an unread pipe wedges the child) |
| `scripts/tui_conpty/driver.py` (repo copy: `tui_drive.py`) | `taskkill /T /F` whole-tree kill while the root is alive; bounded per-step waits |
| `scripts/perf/ab_bird.mjs` | the exact argv shape `--mode json --print --model ... <prompts>` and the JSON event fields it parses |

The TUI/ConPTY path is deliberately **not** used: `--mode json` gives a
machine-readable event stream and does not depend on a terminal renderer.

---

## Files

| file | role |
|---|---|
| `soak_common.py` | shared: mock provider, process-tree sampler, JSONL inspection (torn-tail rules), line runner with per-request timeout, fixture writer, stats |
| `soak_headless.py` | N turns in one long-lived process (or auto-batched processes), alternating text/tool turns, context growth, periodic compaction, root-RSS every K turns plus continuous process-tree sampling |
| `soak_session_reopen.py` | write/reuse a large session, run one turn, close, reopen M times; assert flat RSS and clean sessions |
| `soak_crash_reopen.py` | kill `pir` mid-turn (whole tree), reopen, assert durable/session files are readable and the next turn succeeds; repeat with a crash-reopen counter |
| `README.md` | this file |

---

## Default provider (offline mock)

Unless `--model` names a real provider and `--no-mock` is passed, every driver
starts a local OpenAI-compatible mock (`127.0.0.1`, random port) and writes a
temporary agent dir (`<log>/agent` by default) with `models.json` and
`settings.json`. No API key and no network are needed.

The mock decides each response from a marker in the newest user prompt:

* `[[soak:text]]` → a text answer of `--growth-chars` bytes (context growth);
* `[[soak:tool:bash]]` / `[[soak:tool:read]]` → a real `bash`/`read` tool call,
  then a text answer after the tool result;
* the compaction summarizer's fixed system prompt → a summary text.

It reports `usage` derived from the **actual request body size**, so the
threshold-compaction decision follows real transcript growth instead of a
synthetic counter.

> **Mock mode owns `<agent-dir>/models.json` and `settings.json`** (the `mock`
> provider and the soak settings keys are merged in). Use the default
> `<log>/agent`; do not point `--agent-dir` at a real agent dir in mock mode
> unless you accept those writes. For a real provider use
> `--model provider/model --no-mock [--agent-dir ...]`.

---

## 1. `soak_headless.py` — N turns without degradation

```powershell
python scripts/soak/soak_headless.py --turns 300 --interval 10
python scripts/soak/soak_headless.py --pir target-alt/release/pir.exe `
    --turns 1000 --interval 10 --tool-every 4 --growth-chars 4000 `
    --log logs/soak/headless-1k
python scripts/soak/soak_headless.py --model openai/gpt-5 --no-mock `
    --turns 200 --interval 10 --min-tool-calls 0 --min-compactions 0
```

What it does:

* one `pir --mode json --print --session-id <uuid>` process runs all N prompts;
  if the OS command line would exceed ~30 KB it partitions the prompts into
  auto-batches and reopens the same session between them (reported as
  `batchCount`);
* every `--tool-every`-th turn is a tool call, alternating `bash` and `read`;
  the mock's text responses and tool results add real bytes to the transcript
  every turn;
* it records a fast root-RSS point every `--interval` completed turns and
  samples the **whole process tree's** working set and member count
  continuously in a background thread (tool children included; one CIM query
  is ~0.5–1.5 s and is the real throttle), plus a baseline point at turn 0 and
  one explicit tree point after the final `agent_end` of every batch;
* a per-request timeout (`--request-timeout`, default 180 s) kills the tree
  when no JSON event arrives; a total budget (`--timeout`) bounds the run;
* after every batch exits, the driver re-checks every pid the sampler ever
  observed: a surviving member is an orphaned tool child and fails the run
  (POSIX zombies count as dead; the root is reaped before the check).

Pass criteria (defaults):

* `completedTurns == --turns`, every batch exit code 0;
* no request-hang/timeout, no assistant `stopReason` `error|aborted` (stream
  error frames are counted in `counts.error_events`; a retried error is not
  degradation);
* tool errors ≤ `--max-tool-errors` (default 0) and tool calls ≥
  `--min-tool-calls` (mock default: one per tool prompt);
* successful compactions ≥ `--min-compactions`. In mock mode the default is
  derived from **measured context growth** (the session bytes appended,
  `/4` bytes per token): at least one successful compaction per
  `--context-window - --reserve-tokens` tokens that the run actually grew,
  never a fixed small number. The pre-run estimate from
  `--growth-chars`/tool output is reported as `minCompactionsEstimate`, and
  the measured floor (`minCompactionsMeasured`) replaces it whenever the run
  grew. An explicit `--min-compactions` overrides both. Aborted/error
  compactions are **not** counted as success and fail the run
  (`counts.compaction_aborted` is a subset of `compaction_failures`);
* the session file exists and **grew** (`sessionBytesAfter > sessionBytesBefore`),
  its header is the strict **v3** header with the expected id (missing or
  unknown versions fail), every complete line parses, the file ends with a
  newline, and the first stdout line of each batch is the same v3 session
  header with the matching id;
* peak RSS ≤ `--rss-max-mb` (512), least-squares slope ≤ `--rss-slope-mb`
  (0.10 MB/turn), last-quarter median − first-quarter median ≤
  `--rss-growth-mb` (64 MB); the first `--warmup-turns` (default 10) are
  excluded from the slope/growth trend so the startup ramp is not a leak.
  The verdict requires at least `--min-tree-samples` (default 4) process-tree
  samples **and** process-count readings, and at least 3 post-warmup tree
  points (two points are only "first vs last"; the least-squares slope needs a
  real window). A root-only or merged fallback is reported for diagnosis but
  fails the run. `summary.json → rss.criterion` states the exact rule and
  scope used, and the `rss.limits` object records the numeric limits;
* process-tree members ≤ `--max-processes` (64, checked over every sample);
  the process verdict also uses a least-squares slope over the post-warmup
  sample window (the fitted rise across the window must be ≤
  `--process-growth`, default 8) plus last-quarter − first-quarter median
  growth, and requires at least `--min-process-samples` (default 4) post-warmup
  readings. `summary.json → processes.criterion` / `processes.criteria` state
  the rule and each measured-vs-limit pair.

Expected runtime (mock): roughly 0.2–0.8 s/turn plus the background sampler
(one CIM query at a time, ~0.5–1.5 s each on Windows). 200 turns ≈ 2–5 min;
1000 turns ≈ 10–25 min. Real-provider runs are bounded by the provider.

Artifacts in `--log`: `headless.stdout.jsonl` (or `headless-batchNN.*`),
`headless.stderr.txt`, `samples.jsonl`, `summary.json`.

---

## 2. `soak_session_reopen.py` — large session, M reopens

```powershell
python scripts/soak/soak_session_reopen.py --reopens 20 --size-mb 20
python scripts/soak/soak_session_reopen.py --pir target-alt/release/pir.exe `
    --size-mb 50 --reopens 30 --log logs/soak/reopen-50mb
python scripts/soak/soak_session_reopen.py --fixture logs/session.jsonl `
    --reopens 10 --fixture-usage-tokens 1000
```

What it does:

* generates a valid v3 JSONL fixture (default 20 MB, cached under
  `<log>/fixture/` with a sidecar meta file) or copies `--fixture`;
* the generated fixture's last assistant usage is `2 × --context-window`
  tokens, so the first open takes the tail-first/paged path and performs one
  threshold compaction; later opens see the compaction checkpoint and stay
  paged. Pass a low `--fixture-usage-tokens` to exercise the full/widening path
  instead. `0` is honored literally (full-load path); it is not treated as
  "unset";
* for each reopen: fresh process → one turn → clean exit → validate the
  session file; peak process-tree RSS is sampled in the background while the
  turn runs, and at least one tree sample is attempted after `agent_end` (a
  root-only peak is never relabeled as tree). If the child exits before a CIM
  query can finish, the post-`agent_end` check is recorded as
  `postAgentEndSkipped` and the *tree* peak is still mandatory
  (`--min-tree-samples`, default 1);
* `--fixture-usage-tokens` also applies to `--fixture`: the copied fixture's
  assistant `totalTokens` is set to exactly that value (in place when
  `--fixture-in-place` is given) instead of being silently ignored;
* every reopen's size is chained to the previous one: `beforeBytes` must equal
  the previous `afterBytes`, the appended region must parse and contain both
  the user prompt and the assistant reply (a lone repair newline or a
  bookkeeping entry is not recovery);
* each reopen's stdout first line must be the strict v3 session header with
  the fixture's id.

Pass criteria:

* every reopen exits 0, emits `agent_end` and an assistant message, and its
  JSON header line names the fixture's session id with `version: 3`;
* every session inspection is clean: **strict v3** header version (missing or
  unknown versions fail), header id unchanged, no malformed complete line,
  trailing newline present, and the file strictly grew (after bytes > before
  bytes) with a continuous size chain across reopens;
* RSS peak per reopen uses process-tree samples only and includes a
  post-`agent_end` tree point (or a recorded skip): slope ≤
  `--rss-slope-mb` (1.0 MB/reopen), last-quarter median − first-quarter
  median ≤ `--rss-growth-mb` (24 MB), absolute tree peak ≤ `--rss-max-mb`
  (1024) checked both per reopen and run-wide; the first `--warmup-reopens`
  (default 1, the one that may compact) are excluded from the trend; the
  run-level verdict requires ≥ 4 reopen peaks (`--min-process-samples` sets
  the process trend minimum, default 4);
* process tree ≤ `--max-processes` (64, checked over every sample); the
  process verdict uses the least-squares slope over the post-warmup reopen
  window (fitted rise ≤ `--process-growth`, default 8) plus quarterly-median
  growth, with the criterion and measured/limit pairs in `summary.json →
  processes`;
* compaction count ≥ `--min-compactions` (default 1 when the measured newest
  assistant usage exceeds `--context-window - --reserve-tokens`, else 0).
  Only successful `compaction_end` frames are counted; aborted/error frames
  are not successes.

Expected runtime: fixture generation ~5–15 s for 20 MB; the first reopen can
take longer (paged compaction may issue many summary calls), later reopens are
~0.5–2 s each plus sampling. 20 reopens ≈ 1–3 min.

Artifacts: `reopen-NNN.stdout.jsonl`, `reopen-NNN.stderr.txt`,
`iterations.jsonl`, `summary.json`.

---

## 3. `soak_crash_reopen.py` — kill mid-turn, reopen, repeat

```powershell
python scripts/soak/soak_crash_reopen.py --iterations 10
python scripts/soak/soak_crash_reopen.py --pir target-alt/release/pir.exe `
    --iterations 30 --slow-delay-ms 5000 --kill-min 0.05 --kill-max 1.0 `
    --log logs/soak/crash-30
python scripts/soak/soak_crash_reopen.py --session logs/soak/reopen-20mb/fixture/soak-session.jsonl `
    --iterations 5
```

What it does per iteration:

1. starts `pir --mode json --print --session <file>` with a prompt; a
   background tree sampler records the members while the root is alive;
2. waits until `turn_start` / `message_start` proves the turn is running,
   sleeps a random interval in `[--kill-min, --kill-max]`, then kills the whole
   process tree (`taskkill /T /F` on Windows, `SIGKILL` to the process group on
   POSIX);
3. **captures a crash baseline before each iteration** (bytes, lines, parsed
   records, malformed count, header version/id) and compares the crashed file
   against it;
4. inspects the session file (and any `durable/*.jsonl` under the agent dir):
   the header must be strictly v3 and parse, every COMPLETE line must parse,
   and only the final non-newline-terminated line may be torn; the crashed
   process must not have exited 0, and every pid the sampler saw must be gone
   after the kill (orphaned tool children and POSIX zombies are reported,
   zombies as dead);
5. reopens the same session with a normal turn and requires exit 0, `agent_end`
   and an assistant message;
6. re-inspects: the appended region (everything after the pre-recovery size,
   minus the one repair newline) must contain at least one **parsed** session
   record — specifically the recovery's assistant message — and parse
   line-by-line; an empty append, a lone repair newline, or an unparsable tail
   is a failure. The file must not shrink below the pre-recovery size, the
   header id must be unchanged (v3), the file must end with a newline and have
   no torn tail, and the documented repair newline must be present when the
   crashed file lacked a trailing newline. `compaction_end` frames are counted
   only when successful; failures/aborts are reported and fail the recovery.

The mock makes the crashed turn slow (`--slow-delay-ms`, default 5000 ms) so
the kill lands while the process is waiting on the provider; the recovery turn
runs at full speed. Compaction runs before `turn_start`, and its summary calls
are delayed by the same setting: with a large `--session`, lower
`--slow-delay-ms` or raise `--startup-timeout`. `--iterations` is the
crash-reopen counter reported as `crashReopens` / `midTurnKills` in
`summary.json`.

Pass criteria: every iteration confirms a mid-turn kill (the process must not
exit 0 before the kill), no new malformed complete line, all session/durable
files readable with a strict v3 header, no process left alive from the killed
tree, every recovery turn exits 0 with an assistant message plus a parseable
appended record, the final file is newline-terminated with no torn tail, and
the crash-reopen counter equals `--iterations`. Every iteration record carries
its `crashBaseline`, `killLeftovers`, appended-region parse/role counts, and
header versions for review.

Expected runtime: ~1–4 s/iteration with the mock; 10 iterations ≈ 30–60 s,
30 ≈ 2–4 min.

Artifacts: `crash-NNN.stdout.jsonl`, `recovery-NNN.stdout.jsonl`, matching
`.stderr.txt` files, `iterations.jsonl`, `summary.json`.

---

## Foreman commands (exact)

Run from the repo root (`C:\Users\XPENG_USER\Desktop\github\pir`).

### Syntax/compile gate

```powershell
python -m py_compile scripts/soak/soak_common.py scripts/soak/soak_headless.py scripts/soak/soak_session_reopen.py scripts/soak/soak_crash_reopen.py
```

### Quick gate (~5–8 min)

```powershell
python scripts/soak/soak_headless.py --pir target-alt/release/pir.exe --turns 100 --interval 10 --log logs/soak/gate-headless
python scripts/soak/soak_session_reopen.py --pir target-alt/release/pir.exe --reopens 8 --size-mb 20 --log logs/soak/gate-reopen
python scripts/soak/soak_crash_reopen.py --pir target-alt/release/pir.exe --iterations 5 --log logs/soak/gate-crash
```

### Full campaign (~30–60 min)

```powershell
python scripts/soak/soak_headless.py --pir target-alt/release/pir.exe --turns 1000 --interval 10 --log logs/soak/full-headless
python scripts/soak/soak_session_reopen.py --pir target-alt/release/pir.exe --reopens 50 --size-mb 50 --log logs/soak/full-reopen
python scripts/soak/soak_crash_reopen.py --pir target-alt/release/pir.exe --iterations 30 --log logs/soak/full-crash
```

Each process prints a `=== ... ===` verdict block and writes
`<log>/summary.json`. **Exit codes: 0 = PASS, 1 = soak verdict FAIL, 2 =
setup/harness error.** The foreman should treat any non-zero exit as a gate
failure and attach `summary.json` plus the newest `*.stderr.txt`.

### What to inspect on failure

* `summary.json → reasons` — every failed criterion, in plain English;
* `summary.json → iterations` / `samples` — per-reopen peaks, per-sample RSS,
  child counts, kill timings;
* `*.stdout.jsonl` — the exact `--mode json` event stream up to the failure
  (a killed run still leaves everything it emitted);
* `*.stderr.txt` — provider/tool errors and panic messages.

---

## Adaptation notes (what changed vs. the referenced drivers)

* `rss_probe.py` samples `tasklist` for one pid. A soak must include tool
  children, so the tree sampler uses one CIM query (`Get-CimInstance
  Win32_Process`) and sums the root's descendants — the same instrument as
  `big_session_probe.py`. `tasklist` alone reports only the shim/release
  process and misses the MSYS/bash children.
* `tui_drive.py` uses a ConPTY because the TUI needs a terminal. The soak
  drives `--mode json --print` instead; its process-tree kill
  (`taskkill /T /F`, root verified alive first) and bounded waits are reused.
* `ab_bird.mjs` parses `message_update` frames but only keeps metrics. The soak
  keeps the full stdout stream on disk and counts `agent_start/agent_end`,
  `tool_execution_*`, `compaction_*`, `message_end` and `error` frames.
* The `pir print` subcommand takes a single prompt, but a long-lived soak needs
  several turns in one process. The harness uses the equivalent global
  `--mode json --print` entry point (the argv shape `ab_bird.mjs` already
  uses), whose positional messages become one prompt each
  (`print.rs::build_print_prompts` → `additional_messages`). This is what makes
  the N-turn single-process soak possible; when the OS command line would
  overflow, the driver transparently switches to batched processes over the
  same session.
* `big_session_probe.py`'s mock writes a fixed `"OK"` and a fixed usage; the
  soak mock chooses text/tool/summary from prompt markers and derives usage
  from the actual request size so threshold compaction is driven by real
  context growth.
* The fixture writer is the same v3 JSONL shape as
  `big_session_probe.make_session`, but the assistant `totalTokens` is
  parameterized (`--fixture-usage-tokens`) so the same generator can test both
  the paged/compaction path and the full-load path. The override is honored
  literally (`0` is not "unset"), and with `--fixture` the copied file's
  assistant usage is rewritten (`soak_common.rewrite_fixture_usage`).
* Crash durability uses the reader's documented contract: a kill may leave one
  torn final line; opening repairs only the missing newline. The harness
  therefore allows a torn tail (and a repaired torn line from an earlier
  iteration) but fails on any other unparsable complete line, on an empty or
  unparsable recovery append, and on a tail that is still torn after recovery.
* Header validation is strict everywhere: the session file and the first
  stdout JSON line must both carry `type: "session"`, `version: 3` and the
  expected id. A missing or unknown version is named explicitly in `reasons`
  (never silently accepted as v3).

## Caveats

* Windows needs PowerShell for the process-tree query; the harness degrades to
  `None` samples and now **fails** the verdict ("insufficient tree RSS
  samples" / "no process samples") instead of reporting a silent pass. POSIX
  uses `/proc` and excludes zombie members.
* Real-provider mode cannot force tool calls or compaction; use
  `--min-tool-calls 0 --min-compactions 0` (the defaults when `--no-mock` is
  used) unless the model/fixture is known to exercise them.
* The headless default derives the required compaction count from the context
  window, reserve/keep-recent tokens and the **measured** context growth; an
  explicit `--min-compactions` always overrides it. Flat-context runs
  (`--growth-chars 0 --tool-every 0`) derive 0. Real-provider runs keep the
  measured floor at 0 unless `--min-compactions` is explicit, because the
  real model's window is not the `--context-window` value.
* `--min-tree-samples` / `--min-process-samples` default to 4 for the headless
  run and 4 run-level reopen peaks. A deliberately short smoke run may produce
  fewer samples; the harness fails instead of silently passing, so lower the
  flags explicitly for smoke runs. The slope itself needs ≥ 3 post-warmup
  tree points, so a smoke run shorter than that cannot produce a slope verdict
  — use `--warmup-turns 0` and enough turns for the sampler to land ≥ 3
  points.
* A crash test with a very fast real provider may complete before the random
  kill; raise `--slow-delay-ms` (mock) or lower `--kill-max`. The driver fails
  the iteration rather than pretending it killed a live turn. The kill is
  never delayed by a synchronous process query: the tree members are recorded
  by a background sampler, so a slow CIM query cannot turn the crash into a
  completed turn.
* `--session-in-place` in the crash driver mutates the given file; without it
  the file is copied into the log dir first.
* With `--no-mock`, the reopen fixture's default high usage triggers a
  paged compaction whose summary requests go to the REAL provider (many calls
  for a large fixture). Use `--fixture-usage-tokens 0` (full-load path) and/or
  a small `--size-mb` to avoid surprise API cost. With `--fixture`,
  `--fixture-in-place` rewrites that file's assistant usage when the override
  is given.
* A low `--fixture-usage-tokens` forces the full/widening load path. That path
  is intentionally expensive (`scripts/perf/big_session_probe.py` measured
  ~1.8 GB peak for a 100 MB session); use the default paged fixture for the
  flat-RSS assertion, or raise `--rss-max-mb`/reduce `--size-mb` when testing
  the full load.
