# Harder offline task suite (quality / intelligence lane)

Five dependency-free tasks that rank **quality and intelligence**, not just
"does it compile". Every task is offline (Node stdlib only, no `npm install`,
no network), objectively scored by hidden tests the agent never gets in its
work directory, and runnable in a few minutes.

This directory is self-contained. The verdict scripts do **not** call the model
gateway; they only execute the agent's work directory in a throwaway copy.

## Layout

```text
scripts/perf/tasks/<task>/
  fixture/      starting files handed to the agent (its whole world)
  prompt.md     exact user-style prompt passed to pir / pi
  hidden/
    tests/      hidden tests (one file per check; never copied into fixture)
    verdict.mjs copies the agent work dir to a temp dir, drops hidden/ in,
                runs the tests and prints {task, passed, failed, total, score, notes}
  reference/    a complete solution used to prove the tests are satisfiable
```

| task | axis | hidden checks | what it ranks |
| --- | --- | --- | --- |
| `retry-policy` | subtle multi-file bug + decoy | 10 | spec-faithful debugging; the visible suite is green and only the repro fails |
| `argparse-spec` | precise spec with edge cases | 22 | thoroughness: quoting, clustering, `--`, negative numbers, defaults |
| `textstats-split` | multi-file refactor with API contract | 12 | architecture/instruction-following: old + new entry points, line budgets, dependency direction |
| `logq` | long-horizon app, 8 requirements | 8 | completeness under partial credit; each requirement is its own test |
| `date-range` | misleading failing test | 8 | judgment: the visible test contradicts the README, and latent bugs must also be fixed |

Every hidden check is one point. `score = passed / total`, so tasks 2–5 give
partial credit and separate a complete solution from a mostly-correct one.
`notes` contains the first failure line for each failed check. The per-test
timeout defaults to 30 s (60 s for `logq`). Measured reference-verdict wall
time on this machine (Windows, process-spawn dominated): `retry-policy` ~13 s,
`argparse-spec` ~26 s, `textstats-split` ~17 s, `logq` ~55 s, `date-range`
~17 s — well inside the few-minutes budget. The agent side is the variable
part.

## Running a task with both CLIs

The CLI contract used by this repo is:

```text
<cli> --mode json --print --model <provider/model> "<prompt>"
```

`pir` and upstream `pi` accept that shape (on Windows upstream is the npm
`.cmd` shim, e.g. `%APPDATA%/npm/pi.cmd`). Keep the agent's work directory
**outside the repo**, in a fresh temp dir, so it cannot discover the hidden
tests by walking up from `cwd`, and give each CLI its own isolated agent dir.

```bash
TASK=retry-policy
MODEL=openai/gpt-5
PROMPT="$(cat scripts/perf/tasks/$TASK/prompt.md)"

WORK="$(mktemp -d)"
cp -R "scripts/perf/tasks/$TASK/fixture/." "$WORK/"

# leg 1: pir
(cd "$WORK" && /abs/path/to/pir --mode json --print --model "$MODEL" "$PROMPT")

# score the resulting work dir (do this before resetting it)
node scripts/perf/tasks/$TASK/hidden/verdict.mjs "$WORK"

# leg 2: upstream pi on a fresh copy of the same fixture
WORK2="$(mktemp -d)"
cp -R "scripts/perf/tasks/$TASK/fixture/." "$WORK2/"
(cd "$WORK2" && /path/to/pi --mode json --print --model "$MODEL" "$PROMPT")
node scripts/perf/tasks/$TASK/hidden/verdict.mjs "$WORK2"
```

Notes:

- Run the two legs in isolated agent dirs (`PIR_AGENT_DIR` /
  `PI_CODING_AGENT_DIR`) and with the same model; otherwise the comparison is
  polluted by session state.
- The verdict prints JSON on stdout and exits 0 once a score exists (even 0).
  Non-zero exit means the verdict itself failed (bad path / no tests).
- Wall-clock and token counts are not measured here. Capture them in the outer
  runner (`/usr/bin/time`, or the `--mode json` event stream). This lane only
  produces the quality score; the existing `scripts/perf/*` harnesses cover
  speed and cost.
- `VERDICT_TIMEOUT_MS` overrides the per-test timeout (defaults: 30 s, logq 60 s).

## Reference vs fixture validation

Each task ships a `reference/` solution. Re-run the calibration:

```bash
for task in retry-policy argparse-spec textstats-split logq date-range; do
  echo "== $task reference"
  node "scripts/perf/tasks/$task/hidden/verdict.mjs" "scripts/perf/tasks/$task/reference"
  echo "== $task fixture"
  node "scripts/perf/tasks/$task/hidden/verdict.mjs" "scripts/perf/tasks/$task/fixture"
done
```

Measured calibration (2026-10-15, Node v24.19.0, Windows):

| task | reference | fixture |
| --- | --- | --- |
| `retry-policy` | 10/10 = 1.00 | 1/10 = 0.10 |
| `argparse-spec` | 22/22 = 1.00 | 0/22 = 0.00 |
| `textstats-split` | 12/12 = 1.00 | 0/12 = 0.00 |
| `logq` | 8/8 = 1.00 | 0/8 = 0.00 |
| `date-range` | 8/8 = 1.00 | 3/8 = 0.375 |

All reference scores are 1.0, so a 0/low fixture score is caused by missing
work, not by an unsatisfiable test. The date-range fixture is intentionally
partly functional: the source already handles ordinary ranges, so its
remaining failures isolate the misleading-test trap and the latent validation
bugs.

## Why these discriminate

- `retry-policy`: the visible `node --test` is green. The only visible signal
  is `node repro.js`, which exposes two symptoms. Hidden checks add total
  attempt counting (`maxRetries` means retries *after* the first attempt), the
  cap, `onRetry` numbering, network-code classification and `attempts`. An
  agent that patches the repro only still fails. `src/retry_legacy.js` is a
  correct-looking decoy.
- `argparse-spec`: 22 independent behavioral checks around a written contract;
  the visible tests cover 6 basics. Quoting/escaping, short clusters, `--`,
  negative-number lookahead, empty `=` values, list defaults and error codes
  separate a careful implementation from a happy-path one.
- `textstats-split`: hidden tests import the old and new entry points, assert
  exact export sets and function identity, enforce line budgets and dependency
  direction, and run a pristine copy of the original suite. A copy-paste split
  into three roughly equal files will fail the budgets/contract even if
  behavior is right.
- `logq`: eight requirements, one hidden test each, so the score is a
  completeness curve. It rewards an agent that plans the whole CLI and
  penalizes one that implements filters and stops before CSV/stats.
- `date-range`: the strongest judgment discriminator. Two visible assertions
  are wrong and two latent bugs are invisible. A model that only makes the
  visible test pass changes correct behavior and fails hidden checks; the
  winning move is to trust the README, fix the tests, and fix the real
  validation/DST bugs. The hidden TZ check runs the code under
  `America/New_York` and `Australia/Lord_Howe`, so local-time arithmetic cannot
  sneak through.

## Integrity notes

- Hidden tests never appear in `fixture/`; `verdict.mjs` copies them into a
  *copy* of the agent work dir after the agent finishes and deletes the temp
  dir in a `finally`.
- The verdict never imports code from the agent directly: each hidden test runs
  in its own child process with `NODE_OPTIONS` cleared.
- Fixtures are CommonJS unless a task says otherwise, so a planted
  `package.json` cannot change how hidden `.mjs` tests parse.
- Agents could in principle read this repo if they can access paths outside
  their work dir. Run the CLIs with `cwd` in a temp directory and do not pass
  absolute repo paths in the prompt.
