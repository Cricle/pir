# `ab-ar`: reproducible pir vs pi A/B on a pinned gateway

Pins an A/B campaign to **one** provider/model. Two campaigns are committed;
`ar` is the default and keeps every pre-existing command unchanged.

| campaign | provider | baseUrl | api | auth | model | context / maxTokens |
|---|---|---|---|---|---|---|
| `ar` (default) | `ar` | `https://ai.1k0.com/v1` | `openai-completions` | provider default header | `gpt-5` | 1 000 000 / 65 536 |
| `openai` | `openai` | `https://api.openai.com` | `anthropic-messages` | `authHeader: true` (Bearer) | `gpt-5` | 1 000 000 / 65 536 |

- Credential: `models.json` template **`${PIR_AB_API_KEY}`**, resolved by the
  CLI at request time (pir: `src/ai/providers/config_value.rs`). The key is
  never written to disk and never printed by `run_ab` (it reports only
  `PIR_AB_API_KEY: PRESENT`).
- Each campaign is one committed template. `--campaign <name>` selects
  `agent-template-<name>/models.json` (only `ar` and `openai` are registered);
  `--template <path>` selects an explicit file. `provider`, `model id`, `api`,
  `authHeader` and `baseUrl` are **derived from the template**, so adding a
  gateway is a data change. A custom `--template` is validated against the
  selected campaign's `api`/`authHeader` contract and its own `baseUrl`.
- `--model` / `PIR_AB_MODEL`, if set, must match `<provider>/<model>` (or the
  bare model id) exactly. `--endpoint` / `PIR_AB_BASE_URL` rewrites the
  **runtime copies only**; the committed template is never modified.

The runner copies the template into throwaway runtime agent dirs; it does not
touch `~/.pi/agent`.

## Files

| Path | Purpose |
|---|---|
| `agent-template/models.json` | Isolated agent-dir template: only `ar` / `gpt-5`, key from `${PIR_AB_API_KEY}` |
| `agent-template-openai/models.json` | Isolated agent-dir template: only `openai` / `gpt-5` (`anthropic-messages`, Bearer), key from `${PIR_AB_API_KEY}` |
| `run_ab.mjs` | Bootstrap + canary + campaign driver (Node stdlib only) |
| `run_ab.sh` | Thin `exec node run_ab.mjs "$@"` launcher (Git Bash/POSIX) |

The runner orchestrates the standalone harnesses:
`scripts/perf/ab_bird.mjs` (always with `--no-skills`),
`scripts/perf/harder_tasks_campaign.mjs` (for `--task quality`) and, with
`--p3`, `scripts/perf/p3_task_e2e.mjs`.

## What is a quality score and what is not

| Task | Runs | Signal | Quality score? |
|---|---|---|---|
| `probe` / `gen` | `ab_bird` | one provider round trip: wall, startup, TTFT, TTFT(req), RSS | no — latency/throughput |
| `bird` | `ab_bird` | artifact SHAPE (index.html + canvas + script, `node --check`, ≥8 KB, README) | **no — artifact check** |
| `app` | `ab_bird` | the project's OWN `node --test` suite (self-eval) | **no — self-eval check** |
| `quality` | `harder_tasks_campaign.mjs` | passed/total hidden checks, 5 tasks | **yes** |
| `--p3` | `p3_task_e2e.mjs` | pristine fixture `node --test` after the leg | yes (small, 2 fixtures) |

`bird`/`app` are useful for "did it produce something sane, how fast, how
expensive". They cannot rank implementation quality: `bird` only checks shape
and `app` grades the model with tests the model wrote itself. Use
`--task quality` for a quality comparison; every `ab_bird` report carries a
`checkKind` field and a `summary.json` with `qualityScore: false`, and
`ab_bird --quality` refuses with a pointer to the quality harness.

## Environment contract

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `PIR_AB_API_KEY` | **yes** (real runs) | — | Selected campaign's gateway key. Never written to disk, never logged. |
| `PIR_AB_MODEL` | no | campaign model | Must match the selected template's `<provider>/<model>`. |
| `PIR_AB_BASE_URL` | no | template `baseUrl` | Rewrites `baseUrl` in the runtime copies only (committed template unchanged). |
| `PIR_BIN` / `PI_CLI` | no | repo build / npm shim | Binaries the quality campaign runs; `run_ab` sets them for its child. |

Set the key without putting it in shell history or a repo file, e.g. read it
from your secret store into the variable, then run the campaign. `--dry-run`
works without the key, without binaries and without network.

## What the runner bootstraps

Under the artifacts dir (default `$TEMP/pir-ab-ar/<UTC-stamp>-<task>/`; `--out` must be outside the repo):

```
agent-pir/models.json + settings.json   # per-leg isolated agent dirs
agent-pi/models.json  + settings.json   # (template only, no key)
home/.pi/agent/models.json + settings.json
bin/pir-leg.cmd (or .sh)                # pins PIR_AGENT_DIR/PI_CODING_AGENT_DIR
bin/pi-leg.cmd  (or .sh)                #   to the matching leg dir, then execs
canary-before/, canary-after/           # probe pairs (bird/app/quality)
work/<task>-<pir|pi>-r<N>/              # per-run working dirs
events-*.jsonl, stderr-*.log            # raw streams
report.md, report.json, summary.json    # ab_bird report + machine summary
run-meta.json                           # args, env contract, template sha256,
                                        # canary verdicts, exit codes, artifact path
quality/                                # only with --task quality (hidden tests)
p3/                                     # only with --p3
```

`summary.json` is the machine-readable ranking summary (also written by the
harder campaign and by p3): per leg, medians of wall/startup/TTFT/TTFT(req)/
peak RSS over **stall-free runs only**, the dropped-run reasons, and whether
the two legs are comparable. `run-meta.json` embeds the same object.

Why the launchers: `ab_bird` and `p3_task_e2e` used to hand both legs one shared
temp agent dir. Both now create a **per-leg** agent dir, and `run_ab` goes
further: the launchers re-pin each leg to its own persistent dir
(`PIR_AGENT_DIR`, `PI_CODING_AGENT_DIR`), and the runner sets
`HOME`/`USERPROFILE` to `<out>/home`, so the real profile, sessions, extensions
and pi's `~/.agents/skills` are invisible to both legs. The launchers are the
fair path; a standalone `ab_bird`/`p3` run is hermetic per leg but cannot hide
pi's `~/.agents/skills` (that is why `ab_bird` is always passed `--no-skills`).

Fairness: `ab_bird` is always invoked with `--no-skills`; both legs, both
repetitions, same model/gateway/thinking/timeout, executed sequentially and
interleaved (pir, pi, pir, pi, …) by `ab_bird`.

## Commands

All commands are from the repo root. `--pir` defaults to the first existing of
`target-alt/release/pir.exe`, `target/release/pir.exe`,
`target-alt/debug/pir.exe`, `target/debug/pir.exe`; `--pi` defaults to
`%APPDATA%/npm/pi.cmd` on Windows (pass `--pi` elsewhere).

```bash
export PIR_AB_API_KEY=...            # from your secret store

# 0) Validate syntax/args/plan; starts nothing, no key needed
node scripts/perf/ab-ar/run_ab.mjs --dry-run --task probe --runs 3

# 0b) Write the isolated agent dirs/launchers only (no model calls, no key needed)
node scripts/perf/ab-ar/run_ab.mjs --bootstrap-only --out "$TEMP/pir-ab-ar/bootstrap-check"

# 1) Gateway/request probe — cheap; 3 repetitions for a median
bash scripts/perf/ab-ar/run_ab.sh --task probe --runs 3 \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

# 2) Scored artifact tasks (NOT quality scores — see the table above)
bash scripts/perf/ab-ar/run_ab.sh --task bird --runs 2 \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

bash scripts/perf/ab-ar/run_ab.sh --task app --runs 2 \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

# 2b) Quality campaign — 5 hidden-test tasks, passed/total checks
bash scripts/perf/ab-ar/run_ab.sh --task quality --runs 1 \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

# 3) Optional p3 real-task E2E in the same window (second quality signal)
bash scripts/perf/ab-ar/run_ab.sh --task quality --runs 1 --p3 --p3-task both \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

# 4) Timeline decomposition for the bird/app archives (offline, no model calls)
node scripts/perf/ab_timeline.mjs --dir "$TEMP/pir-ab-ar/<stamp>-bird" \
  --json "$TEMP/pir-ab-ar/<stamp>-bird/timeline.json"

# Explicit artifacts dir
bash scripts/perf/ab-ar/run_ab.sh --task probe --runs 5 --out "$TEMP/pir-ab-ar/probe-1" \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"
```

### openai campaign

Same runner, different template. A stale release binary is refused unless it is
a frozen baseline; a **functional** canary may pass `--allow-stale` and must
then be labelled "not a ranking number".

```bash
# No key/network: validate the plan
node scripts/perf/ab-ar/run_ab.mjs --campaign openai --dry-run
node scripts/perf/ab-ar/run_ab.mjs --campaign openai --bootstrap-only --out "$TEMP/pir-ab-ar/openai-boot"

# Functional canary (2 probes; NOT a ranking number when --allow-stale is used)
bash scripts/perf/ab-ar/run_ab.sh --campaign openai --task probe --runs 2 \
  --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"

# Ranked campaign, after a post-settle `cargo build --release`
bash scripts/perf/ab-ar/run_ab.sh --campaign openai --task quality --runs 5
bash scripts/perf/ab-ar/run_ab.sh --campaign openai --task quality --runs 3 --p3
```

`--out` must be empty unless `--force`; `--allow-stale` bypasses the release-mtime gate
(frozen binary only).

Exit codes: `0` ok, `1` config/harness error, `2` dirty gateway window
(refused to score; `--allow-dirty` continues and tags `run-meta.json`),
`3` model/artifact verdict FAIL (the harness ran; inspect `report.md`/`report.json`).

Rescore an existing artifact set without a model call:
`node scripts/perf/ab_bird.mjs --task app --judge <work-dir>` (and `--task bird
--judge <work-dir>`).

## Gateway-stall canary rule

A **dirty window** is declared when a probe leg reports:

- **wall > 8 000 ms**, or
- **request-relative TTFT > 10 000 ms** (`ttftFromRequestMs`, measured from
  `message_update/start`, not from process start),
- a missing measurement, or a timeout (a timed-out leg cannot be ranked).

A probe whose measurements are inside the thresholds but whose own verdict
failed (non-zero exit, tool use, wrong turn count, `stopReason`
error/aborted, empty final text) is **not** a dirty window: it is a model
verdict FAIL (exit 3). The probe prompt is tool-free ("reply exactly `OK`"),
so its wall is startup + one provider round trip. Both thresholds are
configurable with `--canary-wall-ms` and `--canary-ttft-ms`; the values used
are recorded in `run-meta.json`.

Behavior:

- `--task bird|app|quality`: a probe pair runs **before** and **after** the
  campaign. If the pre-canary is dirty the runner refuses to score (exit 2)
  unless `--allow-dirty`; a dirty post-canary also forces exit 2. Both canary
  reports are kept under `canary-before/` and `canary-after/`.
- `--task probe|gen`: the campaign *is* the canary; every leg is checked
  against the same thresholds. A threshold breach is a dirty window (exit 2);
  a clean window whose probe verdict failed is exit 3 (model FAIL), so
  "rerun later" and "the model failed" are distinguishable.
- `--probe-legs both|pir|pi`: a probe repetition may contain one leg. The rule
  is "every leg present must be clean" plus "at least one leg per repetition";
  a one-leg-per-repetition probe is valid and is not treated as dirty.
- `--no-canary` skips the pre/post pairs for bird/app/quality (not
  recommended).

If a window is dirty the only sound response is to **rerun later**; do not
average a stalled window into a score. `run_ab` records the per-run dirty flags
in `report.json` under `runs[].dirty` and mirrors them in `summary.json`.

## How to rank (important)

Do **not** compare raw wall clock from artifact tasks. The gateway drifts
between windows and a single stall (or one 900 s timeout) dominates the mean.

Rank in this order:

1. **Verdict first**: `report.md`/`report.json` per-leg PASS/FAIL (or the
   hidden-test score for `quality`/`p3`). Only compare legs whose runs are
   verdict-equal; a faster failing run is not a win.
2. **Per-request medians** from `--task probe --runs N`: `summary.json` already
   holds, per leg, the median of `wallMs`, `startupMs`, `ttftMs`,
   `ttftFromRequestMs` and `peakRssMb` over stall-free runs (medians, never
   means or a single run). Rank provider latency on `ttftFromRequestMs`;
   `wallMs` is end-to-end and includes CLI startup.
3. **Stall-free wall** for long tasks: runs that timed out or whose TTFT
   exceeded the canary limit are listed under `droppedReasons` and excluded
   from every median. Report both the kept set and what was dropped.
4. **Quality** only from `--task quality` (`harder_tasks_campaign`) or `--p3`:
   score = passed/total hidden checks. `bird`/`app` are artefact checks and
   must not be presented as quality.
5. **Cost/throughput signals** only after 1–4: `usage` totals, `deltas`,
   `peakRssMb`, artifact bytes/lines.

State the model, endpoint, canary verdicts and the exact artifacts dir with
every score. The artifacts dir is printed by the runner as
`[ab-ar] raw artifacts dir: …` and recorded in `run-meta.json`; the ranking
summary is `<out>/summary.json` (for `--task quality` it is
`<out>/quality/summary.json`).

## Notes and troubleshooting

- `401`/`403`: `PIR_AB_API_KEY` is unset/expired. It is read by the CLI from
  the environment; the runner cannot and does not substitute a key on disk.
- `model not found`: keep `--model`/`PIR_AB_MODEL` at the selected campaign's
  model; each template intentionally contains nothing else.
- openai `401`: `apiKey` must stay `${PIR_AB_API_KEY}`; `authHeader: true` sends
  `Authorization: Bearer <key>`. A custom template that omits `authHeader`
  would send the Anthropic `x-api-key` header instead.
- `--dry-run` and `--bootstrap-only` need no key, no binaries and no network.
- `pi CLI not found`: pass `--pi` (e.g. `"$APPDATA/npm/pi.cmd"` on Windows).
- `p3_task_e2e.mjs` has no `--no-skills` flag; fairness there comes from the
  isolated `HOME` (pi's `~/.agents/skills` is under the throwaway home) and the
  per-leg agent dirs. `ab_bird` additionally passes `--no-skills`.
- Model binaries are pinned and recorded: `run-meta.json`/`report.json`/
  `summary.json` carry each leg's realpath, SHA-256 and `--version`; identical
  hashes are rejected and a release `pir` older than `src/`/`Cargo.toml` is
  refused unless `--allow-stale` (this applies to standalone `ab_bird`,
  `harder_tasks_campaign` and `p3_task_e2e` runs too). `ab_bird` additionally
  verifies that each `*-leg` launcher really executes the hashed binary, so a
  report can never attribute a hash to a different build.
- Work dirs are created under the system temp dir and archived to
  `<out>/work/`; `--out` must be empty unless `--force`. Both legs run with
  `--no-session`, and each run gets its own agent dir which is removed
  afterwards (plus its own work dir), so nothing is shared between legs or
  repetitions.
- Agent dirs still contain the `$ENV` template, never the key, so the artifacts
  dir is safe to share. Still treat `stderr-*.log` as potentially sensitive
  (provider error bodies).
- No dependencies: Node stdlib only; the scripts are validated with
  `node --check` and `bash -n`, never committed by the runner.
- Standalone harnesses are dry-runnable and self-describing:
  `node scripts/perf/ab_bird.mjs --dry-run`,
  `node scripts/perf/harder_tasks_campaign.mjs --dry-run`,
  `node scripts/perf/p3_task_e2e.mjs --dry-run`, each with `--help`. A dry run
  resolves no binary, reads no key and makes no network call. Each standalone
  run also creates a fresh agent dir per leg and refuses stale or identical
  binaries; `--agent-dir <dir>` selects the config source dir.
