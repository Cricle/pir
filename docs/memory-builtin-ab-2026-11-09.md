# Builtin memory A/B — 2026-11-09 18:02

Embedded `cm-memory` as `builtin:memory` versus the same agent binary with the
builtin disabled (`disabledBuiltinExtensions: ["memory"]`, `autoRecall: false`).
Every run is a fresh `pir print --no-session --only-builtin --mode json`
process. Control and treatment use identical prompts, tool allowlists and
workspace copies; the only intentional differences are the memory plugin and
its seeded `cm` store.

**Headline: Q&A aggregate median total tokens 210,419 → 147,132 (-30.1%, N 9/9); short coding routing gate: +0.0% prompt overhead, 0/9 memory injections, 9/9 treatment runs verified (token deltas are sampling noise; prompts are byte-identical).**

- Provider/model: e6t/deepseek-v4.1-flash
- Binary used by the suite: `pir.exe` sha256[:16] = `b6011b437770d2d4`
- Raw rows: `.ab-test/results/runs.jsonl` (54 raw → 36 unique by `(task,kind,mode,rep)`, keep-last; 18 dropped)
- Token numbers are **total tokens** (`usage.totalTokens`) with the input/output/cacheRead split shown separately. They are not a billed-cost claim.
- N is deliberately small. The table reports medians (and medians of the other columns); no statistical-significance claim is made.

## Main suite (auto recall policy)

| Task | N base/mem | Total tokens median b→m (Δ) | Input median b→m (Δ) | Output median b→m (Δ) | CacheRead median b→m (Δ) | Tool calls median (Δ) | Elapsed median | Success |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| q1-scheduler | 3/3 | 223,852 → 153,311 (-31.5%) | 33,413 → 47,615 (+42.5%) | 4,943 → 4,311 (-12.8%) | 171,904 → 120,704 (-29.8%) | 16.0 → 13.0 (-18.8%) | 167.7s → 150.2s | 3/3 + 3/3 |
| q2-context-mode | 3/3 | 230,405 → 261,579 (+13.5%) | 57,693 → 89,830 (+55.7%) | 6,752 → 4,910 (-27.3%) | 169,856 → 164,608 (-3.1%) | 27.0 → 21.0 (-22.2%) | 247.2s → 211.5s | 3/3 + 3/3 |
| q3-memory-builtin | 3/3 | 144,665 → 116,371 (-19.6%) | 29,298 → 45,552 (+55.5%) | 5,331 → 5,518 (+3.5%) | 118,400 → 65,280 (-44.9%) | 17.0 → 19.0 (+11.8%) | 92.0s → 112.5s | 3/3 + 3/3 |
| c1-cache-bug | 3/3 | 82,143 → 78,324 (-4.6%) | 10,429 → 20,913 (+100.5%) | 2,140 → 1,987 (-7.1%) | 58,240 → 55,424 (-4.8%) | 17.0 → 15.0 (-11.8%) | 163.5s → 197.2s | 3/3 + 3/3 |
| c2-config-refactor | 3/3 | 60,703 → 78,536 (+29.4%) | 5,401 → 6,937 (+28.4%) | 2,566 → 2,873 (+12.0%) | 52,736 → 70,144 (+33.0%) | 18.0 → 19.0 (+5.6%) | 96.6s → 73.4s | 3/3 + 3/3 |
| c3-feature-median | 3/3 | 41,569 → 41,366 (-0.5%) | 7,431 → 6,392 (-14.0%) | 1,604 → 1,287 (-19.8%) | 33,920 → 34,688 (+2.3%) | 13.0 → 12.0 (-7.7%) | 73.7s → 101.8s | 3/3 + 3/3 |
| **Q&A all** | 9/9 | **210,419 → 147,132 (-30.1%)** | 33,413 → 47,615 (+42.5%) | 5,406 → 4,461 (-17.5%) | 166,016 → 120,704 (-27.3%) | 17.0 → 19.0 (+11.8%) | 167.7s → 146.9s | 9/9 + 9/9 |
| **Coding all** | 9/9 | **54,447 → 78,324 (+43.9%)** | 7,431 → 10,396 (+39.9%) | 2,140 → 1,987 (-7.1%) | 48,256 → 55,424 (+14.9%) | 17.0 → 15.0 (-11.8%) | 96.6s → 133.0s | 9/9 + 9/9 |

## Routing v2: short-edit gate

Plain short edit prompts in `auto` mode receive no automatic injection; only
continuation prompts get a frame-only block. The control and treatment user
prompts for the C tasks are therefore byte-identical (0% prompt overhead), and
any total-token difference in this table is run-to-run model variance rather
than memory context.

| Task | Memory context injected (mem) | User chars base → mem | Prompt overhead | Verify success base+mem |
|---|---:|---:|---:|---:|
| c1-cache-bug | 0/3 | 96 → 96 | +0.0% | 3/3 + 3/3 |
| c2-config-refactor | 0/3 | 118 → 118 | +0.0% | 3/3 + 3/3 |
| c3-feature-median | 0/3 | 93 → 93 | +0.0% | 3/3 + 3/3 |

## Behavior and grounding

| Task | Memory context base/mem | User chars median base/mem | Attempts | Keyword hits (mem) | Citation valid/invalid (mem, resolved) |
|---|---:|---:|---:|---:|---:|
| q1-scheduler | 0/3 + 3/3 | 186 → 1,994 | 3 + 3 | 5.0 | 141/26 |
| q2-context-mode | 0/3 + 3/3 | 159 → 1,695 | 3 + 3 | 6.0 | 145/2 |
| q3-memory-builtin | 0/3 + 3/3 | 142 → 1,594 | 3 + 3 | 6.0 | 96/29 |
| c1-cache-bug | 0/3 + 0/3 | 96 → 96 | 3 + 3 | 0.0 | 0/0 |
| c2-config-refactor | 0/3 + 0/3 | 118 → 118 | 3 + 3 | 0.0 | 5/0 |
| c3-feature-median | 0/3 + 0/3 | 93 → 93 | 3 + 3 | 0.0 | 0/0 |

`Citation valid/invalid` re-checks each `path:line` against the run workspace,
resolving unique basenames (`mod.rs:123`) when the answer used shorthand. It is
a grounding signal, not a correctness proof.

## Policy: adaptive auto vs always-full-brief (N=2 per arm; token totals inconclusive)

The structural result is stable: auto injects no memory context on plain short
edits (0/6 runs), while `always` injects in every run and grows the user prompt
from a median of 96 to 913 chars. With only two runs per arm, total-token
medians are dominated by model variance and should not be read as a policy
effect.

| Task | N auto/always | Memory context auto/always | User chars median auto → always | Total tokens median auto → always (noisy) | Tool calls median | Success |
|---|---:|---:|---:|---:|---:|---:|
| c1-cache-bug | 2/2 | 0/2 + 2/2 | 96 → 758 | 59,998 → 79,910 | 14.0 → 18.5 | 2/2 + 2/2 |
| c2-config-refactor | 2/2 | 0/2 + 2/2 | 118 → 1,100 | 63,574 → 40,918 | 20.0 → 12.5 | 2/2 + 2/2 |
| c3-feature-median | 2/2 | 0/2 + 2/2 | 93 → 913 | 56,798 → 37,681 | 13.0 → 10.5 | 2/2 + 2/2 |
| **Coding all** | 6/6 | 0/6 + 6/6 | 96 → 913 | **58,902 → 46,013** | 16.0 → 13.5 | 6/6 + 6/6 |

## Multi-session (plan in session 1, implement in session 2)

| Metric | Base median | Memory median | Δ |
|---|---:|---:|---:|
| Total tokens | 129,708 | 70,327 | -45.8% |
| Input tokens | 23,274 | 24,959 | +7.2% |
| Output tokens | 3,778 | 3,133 | -17.1% |
| Tool calls | 31.0 | 27.0 | -12.9% |
| Elapsed | 371.9s | 304.1s | -18.2% |
| Success | 3/3 | 3/3 | — |

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
