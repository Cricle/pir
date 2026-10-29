# AB-RUNBOOK — pir vs pi A/B (2026-10-21)

Verified against the harness sources by the foreman (`run_ab.mjs --dry-run`
passes with no key, no binaries and no network). The `ar` gateway key is read
only from `PIR_AB_API_KEY`; it is never written to disk or printed.

## 0. Environment contract

| Variable | Required | Meaning |
|---|---|---|
| `PIR_AB_API_KEY` | **yes for a real run** | `ar` gateway key (`https://ai.1k0.com/v1`) |
| `PIR_AB_MODEL` | no | default `ar/gpt-5` (stay pinned) |
| `PIR_AB_BASE_URL` | no | rewrites the runtime copies only |
| `PIR_BIN` / `PI_CLI` | no | binaries; `run_ab` sets them for its child |

Set the key without shell history or a repo file, e.g. load it from your
secret store into the variable. **No key ⇒ only `--dry-run` is legal.**

## 1. Pre-flight (no key, no network)

```powershell
node scripts/perf/ab-ar/run_ab.mjs --dry-run
```

Expected: `PIR_AB_API_KEY: MISSING (required for a real run)`, binaries listed
as "existence checked only on a real run", and the exact child commands
printed. This proves the runner no longer resolves binaries too early.

## 2. Real campaign

```powershell
# canary first (2 runs); wall > 8 s or request-relative TTFT > 10 s ⇒ dirty window
node scripts/perf/ab-ar/run_ab.mjs --task probe --runs 2

# artifact/latency (NOT a quality score)
node scripts/perf/ab-ar/run_ab.mjs --task bird --runs 3
node scripts/perf/ab-ar/run_ab.mjs --task app  --runs 3

# quality (the only ranked quality signal)
node scripts/perf/ab-ar/run_ab.mjs --task quality --runs 5

# small end-to-end fixtures
node scripts/perf/ab-ar/run_ab.mjs --task quality --p3 --runs 3
```

## 3. Interpretation rules

- Rank **only stall-free legs**. The shared gateway has intermittent 20–137 s
  stalls that hit both CLIs; a dirty run is registered, never ranked.
- Compare **per-request medians** (TTFT request-relative, wall) and
  **process-tree RSS**, not raw wall clock.
- `bird` is an artifact-shape check (`checkKind`, `qualityScore: false`);
  `app` grades the model with tests the model wrote (self-eval). Neither ranks
  implementation quality — the harness refuses `ab_bird --quality` by design.
- `p3` / `quality` are the quality numbers.
- Exit code 3 means verdict FAIL (not a harness error).

## 4. Analysis

```powershell
node scripts/perf/ab_timeline.mjs      # per-turn provider/stream/tools/idle + stall classification
node scripts/perf/ab_absolute.mjs      # all legs on one absolute timeline
```

## 5. Binary discipline

The runner records binary path/sha/version and **refuses an identical or stale
binary**. The release binary must be rebuilt (`cargo build --release`) after
the code under test, or the run is rejected. `target-alt\release\pir.exe` is
currently the 2026-10-19 build.
