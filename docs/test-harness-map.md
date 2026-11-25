# Test harness map

How `tests/*.rs` becomes cargo targets, and how to run one suite. The
per-file → harness table is **not** kept here: it goes stale on every test
addition. Ask the script instead:

```bash
scripts/which_harness.sh <suite|module|tests/path.rs>   # harness + ready-to-paste nextest command
```

## TL;DR

- `tests/*.rs` and `tests/suites/*.rs` are **not** one cargo target each.
  `autotests = false` disables cargo auto-discovery; instead the 8 generated
  `tests/zz_merged_N.rs` harnesses pull files in with
  `#[path = "..."] mod ...;` (`scripts/gen_merged_tests.ps1`).
- Run one suite with a nextest filter, e.g.
  `cargo nextest run -p pir-sdk --test zz_merged_3 -E 'test(<module>::)'`.
- Merged binaries share **one process** across their tests, so env vars and
  process-global statics can interact. Run the full suite under **nextest**
  (process-per-test), not bare `cargo test`.

## Why N harnesses costs N link steps

Each `[[test]]` target (including each `zz_merged_N.rs`) is its own crate,
statically linked against the whole `pir-sdk` (QuickJS, SWC, …). Naming `N`
harnesses means `N` link steps, and link time — not test execution — dominates
the build. That is why the 348 files under `tests/` are folded into 8 merged
harnesses plus 8 explicit targets: 326 hand-written root files are pulled into
the harnesses (320 compiled as-is + the 6 originals whose `zz_shadow_*` copies
are compiled instead), and 16 `[[test]]` entries total produce those 8 + 8
binaries — 326 + 16 + 6 shadow copies = 348 files on disk. Before adding a
`[[test]]` entry, check whether the tests can join an existing harness: every
extra name multiplies the link phase.

## Targets (`Cargo.toml` `[[test]]`)

| target | path | kind | root files pulled in |
|---|---|---|---|
| `zz_merged_1` | `tests/zz_merged_1.rs` | merged harness | 64 |
| `zz_merged_2` | `tests/zz_merged_2.rs` | merged harness | 64 |
| `zz_merged_3` | `tests/zz_merged_3.rs` | merged harness | 64 |
| `zz_merged_4` | `tests/zz_merged_4.rs` | merged harness | 63 |
| `zz_merged_5` | `tests/zz_merged_5.rs` | merged harness | 52 |
| `zz_merged_6` | `tests/zz_merged_6.rs` | merged harness | 2 |
| `zz_merged_7` | `tests/zz_merged_7.rs` | merged harness | 2 |
| `zz_merged_8` | `tests/zz_merged_8.rs` | merged harness | 9 (+6 shadow) |
| `g_durable_integration` | `tests/g_durable_integration.rs` | standalone | 3 suites |
| `worker_threads_real` | `tests/worker_threads_real.rs` | standalone | 0 |
| `print_native_session` | `tests/print_native_session.rs` | standalone | 0 |
| `resume_on_error` | `tests/resume_on_error.rs` | standalone | 0 |
| `feature_off_surface` | `tests/feature_off_surface.rs` | standalone, `cli` | 0 |
| `sdk_consumer_surface` | `tests/sdk_consumer_surface.rs` | standalone, `sdk` | 0 |
| `sdk_consumer_embed` | `tests/sdk_consumer_embed.rs` | standalone, `sdk` | 0 |
| `rpc_export_off` | `tests/rpc_export_off.rs` | standalone, `rpc` | 0 |

`tests/suites/*.rs` are not targets: each is included by a
`tests/*.rs` wrapper, which is in turn included by a merged harness or named as
a standalone target. The durable suites now live in the `pir-durable` crate
(`crates/pir-durable/tests/`: the `durable` target folds 42 `suites/durable_*.rs`
plus `pi_format_import`, and `durable_storage` / `durable_trace_propagation` /
`durable_memcopy_bench` are their own targets). Only the three integration
suites that need the real `SessionManager` / extension runtime stay here, folded
into `g_durable_integration`.

## Shadowed allocator sources

A test binary can hold at most one `#[global_allocator]`. Six counting-allocator
tests are therefore compiled from `tests/zz_shadow_*.rs` — byte-identical copies
with the allocator declaration stripped, since `zz_merged_8` installs the shared
`dhat::Alloc`:

| source | built as |
|---|---|
| `tests/compaction_peak_attribution.rs` | `tests/zz_shadow_compaction_peak_attribution.rs` |
| `tests/lane_e_streamed_peak.rs` | `tests/zz_shadow_lane_e_streamed_peak.rs` |
| `tests/lane_e_widen_attribution.rs` | `tests/zz_shadow_lane_e_widen_attribution.rs` |
| `tests/sdk_heap_profile.rs` | `tests/zz_shadow_sdk_heap_profile.rs` |
| `tests/session_projection_heap.rs` | `tests/zz_shadow_session_projection_heap.rs` |
| `tests/startup_heap_profile.rs` | `tests/zz_shadow_startup_heap_profile.rs` |

The originals stay on disk unmodified; only the shadow copies are compiled.

## Composite sources

`tests/fragments/sdk_round13_on_event.rs` is pulled into `tests/sdk_extended.rs`
with `include!`, so its tests compile into that file's harness and it is not a
target of its own.

## Regenerating

`powershell -NoProfile -File scripts/gen_merged_tests.ps1` rewrites the
harnesses **and** the `[[test]]` block in `Cargo.toml`; `-Check` diffs the
on-disk files against what the script would write, read-only. Do not run either
form as part of a docs pass — regeneration reshuffles which bin each root file
lands in. `-Check` is not part of the gate battery or CI.
