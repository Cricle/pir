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

## Isolating a test from the rest of the binary

Prefer, in this order:

1. **Pass the directory/path as an argument.** Almost every seam that reads
   `PIR_AGENT_DIR`, `PI_PACKAGE_DIR` or a home directory also has an explicit
   form (`*_in` / `*_with` / `*_at` / a `with_agent_dir` field). A test that
   names its own directory cannot race anything: no lock, no guard.
2. **Take a scratch directory and delete it.** `tests/common::scratch_agent_dir`
   returns a `TempAgentDir` removed on drop. Every scratch path a test builds
   by hand should come from `tests/common::unique_temp_path(label)`: the name
   mixes the pid (concurrent test **binaries**) with a process-wide atomic
   counter (parallel tests **inside one merged binary**). A
   `{label}-{pid}-{nanos}` name is NOT unique — `SystemTime::now()` on Windows
   advances in ~15.6 ms system-tick steps, so two threads reading it in the
   same tick get identical nanoseconds, and a `remove_dir_all` +
   `create_dir_all` fixture then deletes its sibling's live directory (audit
   finding TEST-4).
3. **If the property IS the process environment** (variable precedence, a
   default-built session following the env, a child process inheriting it, a
   `HOME`-anchored scan), gate the test on having a private process and pin the
   variable with `tests/common::ProcessEnvPin`:

   ```rust
   if crate::common::skip_without_nextest("suite::test") { return; }
   let _pin = crate::common::ProcessEnvPin::agent_dir(dir.path());
   ```

   `skip_without_nextest` (and its `_home` / `_temp` / `_package` twins) pass
   when `NEXTEST_RUN_ID` — nextest's per-run identifier — is set, and print a
   `SKIP` line naming the escape hatch (`PIR_ALLOW_SHARED_PROCESS_*`) otherwise.
   `ProcessEnvPin` restores every touched variable on drop, panic included.

   There is deliberately **no lock**: a lock can only serialize one file
   against itself, while every other suite in the merged binary stays free to
   re-point the same variable (audit finding TEST-2). The gate is the whole
   mechanism — a test that cannot get a private process reports `SKIP` instead
   of passing on a value it may have raced for.

4. **If the test reads a process-global counter/registry or is timing-bound,
   gate it the same way.** The gate is not only for environment variables: an
   `AtomicUsize` test seam (`entry_appended_wire_builds`), the process-wide
   worker/child tables, and assertions that a second call returns "immediately"
   are equally shared. When the assertion cannot hold while a sibling suite is
   running, it needs a private process — and `cargo nextest` gives every test
   one. Do not paper over it with a lock or a retry loop:

   ```rust
   if crate::common::skip_without_nextest("suite::test") { return; }
   ```

   Gated so far: the six `session_context_snapshot_cache::model_catalog_push_*`
   (credential fingerprint reads every provider key variable),
   `rss_a_entry_append_materialization` (global wire-build counter),
   `sdk_extended::low_level_create_agent_session_…`,
   `mcp_integration::readiness_cache_backs_off_…`, plus the environment-pinning
   suites listed above.

## Never `sleep` to wait for something

`sleep` is not synchronisation. A fixed delay is either too short (flaky on a
loaded machine, and *more* likely to fail when the whole suite runs in
parallel) or too long (slow). It also hides what the test actually depends on.
Wait for the **event** instead:

| Waiting for | Do this |
|---|---|
| a file a producer writes/replaces | `tests/common::wait_for_file(path, deadline, check)` — blocks on a `notify` directory watch (the crate behind `fs.watch`), re-running `check` per event. No polling. |
| another tokio task (same runtime) | `tokio::task::yield_now().await` in a loop bounded by a deadline. Never `std::thread::yield_now` on a `current_thread` runtime: it parks the only driver and the task you are waiting for never runs. |
| another **OS** thread (worker, dispatcher, process) | `std::thread::yield_now()` in a deadline-bounded loop, or a channel the producer signals. `tokio` yields do not help: the producer is not a tokio task. |
| a process to disappear / a port to free | retry with `std::thread::yield_now()` under a deadline. The OS has no "pid gone" event; the deadline is the failure bound. |
| an external process to observe something | have the producer signal it — write a marker file (`wait_for_file`), send on a channel, or set a counter the test polls under a deadline. |
| a TTL/debounce to elapse | **reconfigure it to zero**, don't wait it out: `set_dialog_ttl(Duration::ZERO)` makes the next sweep expire the entry immediately. Only wait when the elapsed duration IS the contract, and then assert on the bound. |

A deadline is a *failure bound*, not a delay — it only fires when the producer
never delivers, and its panic message names what was being awaited. If you
catch yourself writing `for _ in 0..N { sleep(..) }`, the loop is modelling an
event: name the event and wait on it.

Sleeping **inside a fixture** (a JS tool that must look slow, a handler that
must not complete inline) is different: model it with a gate the test opens
(a channel/semaphore) or a `yield`, so the ordering stays deterministic
instead of depending on how fast the machine is.

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
| `rpc_export_off` | `tests/rpc_export_off.rs` | standalone; needs `rpc` **and not** `export-html`, so run it as `--no-default-features --features rpc` (default features pull `tui` → `export-html` and the suite compiles to zero tests) | 0 |

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
