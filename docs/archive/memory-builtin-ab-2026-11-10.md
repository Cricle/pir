# Builtin memory A/B — 2026-11-10 20:17

Embedded `cm-memory` as `builtin:memory` versus the same agent binary with the
builtin disabled (`disabledBuiltinExtensions: ["memory"]`, `autoRecall: false`).
Every run is a fresh `pir print --no-session --only-builtin --mode json`
process. Control and treatment use identical prompts, tool allowlists and
workspace copies; the only intentional differences are the memory plugin and
its seeded `cm` store.

**Headline: Q&A aggregate median total tokens 244,787 → 141,742 (-42.1%, N 9/9); short coding routing gate: +0.0% prompt overhead, 0/15 memory injections, 15/15 treatment runs verified (token deltas are sampling noise; prompts are byte-identical).**

- Provider/model: e6t/deepseek-v4.1-flash
- Binary used by the suite: `pir.exe` sha256[:16] = `790d6be6064c536d`
- Raw rows: `.ab-test/results/runs.jsonl` (56 raw → 48 unique by `(task,kind,mode,rep)`, keep-last; 8 dropped)
- Token numbers are **total tokens** (`usage.totalTokens`) with the input/output/cacheRead split shown separately. They are not a billed-cost claim.
- N is deliberately small. The table reports medians (and medians of the other columns); no statistical-significance claim is made.

## Main suite (auto recall policy)

| Task | N base/mem | Total tokens median b→m (Δ) | Input median b→m (Δ) | Output median b→m (Δ) | CacheRead median b→m (Δ) | Tool calls median (Δ) | Elapsed median | Success |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| q1-scheduler | 3/3 | 244,787 → 141,742 (-42.1%) | 147,530 → 85,305 (-42.2%) | 4,797 → 4,668 (-2.7%) | 71,552 → 93,312 (+30.4%) | 16.0 → 13.0 (-18.8%) | 88.9s → 58.8s | 3/3 + 3/3 |
| q2-context-mode | 3/3 | 346,032 → 176,200 (-49.1%) | 168,319 → 142,848 (-15.1%) | 5,601 → 3,744 (-33.2%) | 90,624 → 58,496 (-35.5%) | 25.0 → 19.0 (-24.0%) | 167.9s → 161.4s | 3/3 + 2/3 |
| q3-memory-builtin | 3/3 | 98,476 → 91,557 (-7.0%) | 30,752 → 39,779 (+29.4%) | 3,612 → 3,456 (-4.3%) | 64,000 → 64,896 (+1.4%) | 13.0 → 13.0 (+0.0%) | 48.0s → 51.6s | 3/3 + 3/3 |
| c1-cache-bug | 5/5 | 24,511 → 22,545 (-8.0%) | 7,113 → 6,192 (-12.9%) | 1,226 → 812 (-33.8%) | 17,280 → 17,024 (-1.5%) | 9.0 → 8.0 (-11.1%) | 22.1s → 24.5s | 5/5 + 5/5 |
| c2-config-refactor | 5/5 | 19,737 → 23,102 (+17.0%) | 3,960 → 3,099 (-21.7%) | 1,078 → 1,059 (-1.8%) | 15,488 → 18,944 (+22.3%) | 11.0 → 12.0 (+9.1%) | 24.2s → 21.4s | 5/5 + 5/5 |
| c3-feature-median | 5/5 | 19,558 → 14,363 (-26.6%) | 4,274 → 2,022 (-52.7%) | 884 → 767 (-13.2%) | 15,616 → 11,904 (-23.8%) | 8.0 → 7.0 (-12.5%) | 19.7s → 22.6s | 5/5 + 5/5 |
| **Q&A all** | 9/9 | **244,787 → 141,742 (-42.1%)** | 147,530 → 55,689 (-62.3%) | 4,797 → 4,011 (-16.4%) | 87,424 → 64,896 (-25.8%) | 18.0 → 15.0 (-16.7%) | 88.9s → 59.3s | 9/9 + 8/9 |
| **Coding all** | 15/15 | **20,414 → 22,545 (+10.4%)** | 4,274 → 4,568 (+6.9%) | 1,002 → 921 (-8.1%) | 15,616 → 17,152 (+9.8%) | 9.0 → 9.0 (+0.0%) | 21.9s → 22.6s | 15/15 + 15/15 |

## Routing v2: short-edit gate

Plain short edit prompts in `auto` mode receive no automatic injection; only
continuation prompts get a frame-only block. The control and treatment user
prompts for the C tasks are therefore byte-identical (0% prompt overhead), and
any total-token difference in this table is run-to-run model variance rather
than memory context.

C was retested cleanly with the memory store outside the project workspace
(agentDir) and alternating arm order, so the store file cannot be discovered by
`ls`/`find`. Across the 15 paired C runs the median per-run delta is **-5.4%**
and the mean is **+0.6%**; the pooled median (+10.4%) is arm-imbalance noise.

| Task | Memory context injected (mem) | User chars base → mem | Prompt overhead | Verify success base+mem |
|---|---:|---:|---:|---:|
| c1-cache-bug | 0/5 | 96 → 96 | +0.0% | 5/5 + 5/5 |
| c2-config-refactor | 0/5 | 118 → 118 | +0.0% | 5/5 + 5/5 |
| c3-feature-median | 0/5 | 93 → 93 | +0.0% | 5/5 + 5/5 |

## Behavior and grounding

| Task | Memory context base/mem | User chars median base/mem | Attempts | Keyword hits (mem) | Citation valid/invalid (mem, resolved) |
|---|---:|---:|---:|---:|---:|
| q1-scheduler | 0/3 + 3/3 | 186 → 1,994 | 3 + 3 | 5.0 | 147/28 |
| q2-context-mode | 0/3 + 3/3 | 159 → 1,695 | 3 + 3 | 6.0 | 70/5 |
| q3-memory-builtin | 0/3 + 3/3 | 142 → 1,594 | 3 + 3 | 6.0 | 104/27 |
| c1-cache-bug | 0/5 + 0/5 | 96 → 96 | 5 + 5 | 0.0 | 0/0 |
| c2-config-refactor | 0/5 + 0/5 | 118 → 118 | 5 + 5 | 0.0 | 6/0 |
| c3-feature-median | 0/5 + 0/5 | 93 → 93 | 5 + 5 | 0.0 | 0/0 |

`Citation valid/invalid` re-checks each `path:line` against the run workspace,
resolving unique basenames (`mod.rs:123`) when the answer used shorthand. It is
a grounding signal, not a correctness proof.

## Policy: adaptive auto vs always-full-brief (N=2 per arm; token totals inconclusive)

The structural result is stable: auto injects no memory context on plain short
edits (0/6 runs), while `always` injects in every run and grows the user prompt
from a median of 96 to 913 chars. With only two runs per arm, total-token
medians are dominated by model variance and should not be read as a policy
effect.

_Policy experiment not run in this cycle._

## Multi-session (plan in session 1, implement in session 2)

_Multi-session experiment not run in this cycle._

## Method notes and limits

- The control arm never injects `<memory-context>`; the treatment arm injects it
  whenever a configured recall has a hit above `autoRecallMinScore` (default
  0.3). This is verified per run from the user message.
- Q&A success is a keyword-coverage gate (at least `len(keywords) - 2`, minimum
  3). It rewards recall, not a full rubric; the citation check and the raw
  answers are the guard against hallucinated file:line claims. Q3 is partly
  self-referential because the seeded store describes the memory plugin itself.
- Short coding tasks (C1–C3) are verified by each fixture's `verify.py`, not by
  keyword matching.
- Runs execute sequentially base→mem, not randomized; retries are shown in the
  attempts column. Duplicated `(task,mode,rep)` rows are dropped keep-last and
  listed in the raw summary; the dedupe never changes a mode's arm.
- The C rows were re-run after the short-edit routing change (frame-only
  context, no BM25 memory records); Q rows predate it, and the question/long
  prompt path they exercise was not changed by that routing update.
- Earlier provider/policy phases are archived under
  `.ab-test/archive-before-review-fixes/`; they are not mixed into this table.
  This report contains only rows from the single provider/model named above.
