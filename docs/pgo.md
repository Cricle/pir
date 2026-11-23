# PGO (profile-guided optimization) for pir

PGO is an **opt-in, single-engine acceleration**: it recompiles the same source
with execution counts from a representative workload so LLVM and GCC can lay
out branches, inline, and propagate constants using real branch frequencies.
The default `cargo build` is unchanged; every artifact below lives under
`target/pgo/`, which `.gitignore` already excludes (`/target/`).

Two independent profiles are collected because `pir` is two compilers' output
in one process:

| Part | Compiler | Instrumentation | Profile files | Consumption |
|---|---|---|---|---|
| QuickJS C runtime (`rquickjs-sys`: `quickjs.c`, `libregexp.c`, `dtoa.c`, `libunicode.c`), bundled SQLite | MinGW GCC | `-fprofile-generate=<dir>` | `.gcda` (gcov) | `-fprofile-use=<dir>` |
| Rust host and crates (`pir`, `pir-node`, `pir-sdk`, tokio, serde, …) | rustc/LLVM | `-Cprofile-generate=<dir>` | `.profraw` (LLVM instrprof) | `-Cprofile-use=<merged.profdata>` |

The formats are not interchangeable: `llvm-profdata` cannot read gcov data and
GCC cannot read `.profraw`. The pipeline merges only the Rust side (a single
`.profdata` is required by `-Cprofile-use`); GCC reads its directory in place
and accumulates counts across workload runs.

## Files

| Path | Purpose |
|---|---|
| `scripts/pgo/pgo.sh` | Git Bash pipeline: stage0..stage4, dry-run, per-stage selection |
| `scripts/pgo/pgo.ps1` | Windows PowerShell 5.1 equivalent |
| `scripts/pgo/workload.js` | dependency-free JS training workload (runs on `pir js` and Node) |
| `scripts/pgo/compare.sh` | A/B: normal `release-fast` vs the PGO binary over `tests/perf_parity` |
| `scripts/pgo/build-pgo.ps1` | older, exploratory helper; kept for its rustc self-test |

> The repository ignores `*.js` (Node artifacts are normally forbidden). The
> training workload is intentionally a `.js` file so it runs on `pir js` with
> Node semantics; when landing this directory, add it explicitly:
> `git add -f scripts/pgo/workload.js`. The other files are tracked normally.

Artifacts (never committed):

```
target/pgo/
  rust-prof/                  # *.profraw from the instrumented runs
  c-prof/                     # GCC .gcda tree (mirrors the object paths)
  product/release-fast/pir.exe # instrumented first, then profile-use
  out/
    workload-*.log            # one log per training run
    merged.profdata           # Rust profile, merged by llvm-profdata
    pir-pgo.exe               # stable copy of the profile-use binary
    profile-manifest.txt      # toolchain/CPU/commit/flags/hashes
    agent/                    # PIR_AGENT_DIR for the runs
  ab/                         # compare.sh logs and ratio table
  baseline/                   # compare.sh baseline target dir (no PGO)
```

## Quick start

Git Bash:

```bash
scripts/pgo/pgo.sh --dry-run            # print every command and env var
scripts/pgo/pgo.sh                      # stage0..stage4
scripts/pgo/compare.sh                  # baseline vs PGO, 51-op ratio table
```

PowerShell:

```powershell
.\scripts\pgo\pgo.ps1 -DryRun
.\scripts\pgo\pgo.ps1 -Stage all
```

A single stage can be rerun while debugging (`scripts/pgo/pgo.sh stage2`,
`.\scripts\pgo\pgo.ps1 -Stage stage2`). Stage 2 and 3 assume stage 1 already
produced `target/pgo/product/release-fast/pir.exe`.

Pipeline knobs (both scripts):

| Variable / switch | Default | Effect |
|---|---|---|
| `PIR_PGO_TOOLCHAIN` / `-Toolchain` | `stable-x86_64-pc-windows-gnu` | rustup toolchain and target |
| `PIR_PGO_RUNS` / `-Runs` | `3` | workload repetitions in stage 2 |
| `PIR_PGO_WORKLOAD_MS` / `-WorkloadMs` | `3000` | minimum wall time per workload run |
| `PIR_PGO_EXTRA_JS` | unset | optional extra JS entry (real plugin fixture) run once in stage 2 |
| `PIR_PGO_PROFILER_RT` / `-ProfilerRuntime` | unset | `libprofiler_rt.a` for windows-gnu |
| `PIR_PGO_EXTRA_RUSTFLAGS` | unset | extra linker/codegen flags for stages 1 and 4 |
| `PIR_PGO_SKIP_RUST=1` / `-SkipRust` | off | C-only PGO |
| `PIR_PGO_SKIP_C=1` / `-SkipC` | off | Rust-only PGO |
| `PIR_PGO_KEEP_PRODUCT=1` / `-KeepProduct` | off | keep `product/` across stage0 |
| `MINGW_BIN` / `-MingwBin` | `../tools/mingw/mingw64/bin` | MinGW GCC directory |

## Stages

### stage0 — clean

Removes `target/pgo/{rust-prof,c-prof,out}` (and `product/` unless
`PIR_PGO_KEEP_PRODUCT=1`) and recreates the output/agent directories. Profiles
are cheap to throw away and stale counts are worse than no counts.

### stage1 — instrumented build

Exact build command (identical in both scripts):

```text
cargo +stable-x86_64-pc-windows-gnu build \
  --profile release-fast --bin pir --features extensions
```

`release-fast` (inherits release: opt-level 3, `strip`, no fat LTO, 16 CGUs)
is used deliberately: PGO iterations stay in minutes instead of the shipped
`release` profile's serial fat-LTO link. Do not change `Cargo.toml` or the
`release`/`release-spill` profiles for this pipeline; environment variables do
the work.

Environment for stage 1:

| Variable | Value / why |
|---|---|
| `RUSTFLAGS` | `-Cprofile-generate=<abs>/target/pgo/rust-prof -Cllvm-args=-pgo-warn-missing-function` plus, on windows-gnu, `-Clink-arg=-L<gcov dir> -Clink-arg=-lgcov` |
| `RUSTC_BOOTSTRAP` | `1` only when using `-Zno-profiler-runtime` with a custom `libprofiler_rt.a` |
| `CFLAGS` | `-fprofile-generate=<abs>/target/pgo/c-prof -O3` |
| `CFLAGS_x86_64_pc_windows_gnu` | same as `CFLAGS` (see “CFLAGS clobbering”) |
| `CFLAGS_x86_64-pc-windows-gnu` | same, passed through `env` because bash cannot export a name containing `-` |
| `LDFLAGS` | `-fprofile-generate=<abs>/target/pgo/c-prof` (documentation/compat; see link note) |
| `CARGO_TARGET_DIR` | `<abs>/target/pgo/product` |

On windows-gnu the GCC runtime must be linked into the final Rust binary:
instrumented C objects reference `__gcov_*`, and rustc (rust-lld), not gcc,
performs the final link. The script adds `-Clink-arg=-L<gcc lib dir>` and
`-Clink-arg=-lgcov` itself. `LDFLAGS` is still exported because other C build
scripts may read it, but `cc` 1.4 does not use it for the final link.

**CFLAGS clobbering (important).** `rquickjs-sys`'s `build.rs` overwrites the
process `CFLAGS` on Windows (`-DWIN32_LEAN_AND_MEAN -std=c11` before calling
`cc`). A plain `CFLAGS=...` therefore never reaches the QuickJS compile. `cc`
also reads target-specific `CFLAGS_<target>` last, and that name survives.
Both scripts set `CFLAGS_x86_64_pc_windows_gnu` (and the hyphenated form via
`env`) for exactly this reason. If stage 2 reports zero `.gcda`, check this
first.

### stage2 — workload

Runs `scripts/pgo/workload.js` `PIR_PGO_RUNS` times (default **3**), each for
at least `PIR_PGO_WORKLOAD_MS` (default **3000 ms**, adaptive rounds):

| Phase | ~budget | Covers |
|---|---:|---|
| json | 480 ms | `JSON.stringify/parse` of small/deep/400-row payloads |
| object | 480 ms | property read/write, class methods, closures, arrays, `Map`/`Set`, `EventEmitter.emit` |
| string | 360 ms | concatenation, `slice`, `split`, regex `match`/`replace` on long UTF-8 text |
| fs | 360 ms | `readFileSync` 4 KiB/64 KiB, `statSync`, `readdirSync`, periodic `writeFileSync` |
| crypto | 360 ms | SHA-256 64 KiB, HMAC, `randomBytes`, `randomUUID`, periodic PBKDF2 |
| zlib | 360 ms | deflate/inflate 64 KiB, gzip/gunzip |
| sqlite | 360 ms | `node:sqlite`/`sqlite` `DatabaseSync` in-memory: prepared insert/select/get/delete |
| url + path | 210 ms | `URL`, `URLSearchParams`, legacy `url.parse/format/resolve`, `path.*` |
| plugin-surface | 120 ms | tool descriptor registration, async tool body shape |
| stream | async | 32 × `Readable → Transform → Writable` pipelines |
| plugin-async | async | 200 awaited tool executions returning content blocks |

Each run prints `PGO_PHASE …` lines and ends with one `PGO_WORKLOAD_OK` line;
stage 2 fails if a run is missing it, or if no `.profraw`/`.gcda` appeared.
The final summary lists the modules actually touched.

Training runs use a fresh `PIR_AGENT_DIR` and `PIR_PGO_TMP` under
`target/pgo/out/`, so they never read or write `~/.pi`.

**Plugin loading.** The repository has no standalone on-disk plugin fixture:
the tests build extension entries from inline Rust strings and load them
through the Rust extension API, which is not reachable from `pir js` without a
provider. Stage 2 therefore trains with real JS execution (`pir js`) that
includes the plugin-shaped registration/async-tool phases. If you have a real
entry file, set `PIR_PGO_EXTRA_JS=<path>` and stage 2 runs it once under the
same instrumented binary, adding its counts to both profiles.

### stage3 — merge

Rust:

```bash
PROFDATA="$(rustup run stable-x86_64-pc-windows-gnu rustc --print sysroot)/lib/rustlib/x86_64-pc-windows-gnu/bin/llvm-profdata.exe"
"$PROFDATA" merge -o target/pgo/out/merged.profdata target/pgo/rust-prof/*.profraw
"$PROFDATA" show --detailed-summary target/pgo/out/merged.profdata
```

The tool must match rustc's LLVM (this tree: rustc 1.99.1 / LLVM 23.1.1). It
ships with the GNU toolchain here; otherwise
`rustup component add llvm-tools-x86_64-pc-windows-gnu`. Stage 3 fails on zero
inputs or an empty output.

C: nothing to merge. GCC accumulates into the same `.gcda` files across runs
and `-fprofile-use=<c-prof>` reads that directory. `gcov-tool` is only used to
print its version when present.

**Why stages 1 and 4 share `CARGO_TARGET_DIR`.** GCC derives the `.gcda` path
from the *object file's absolute path* (the tree under `c-prof/` mirrors it).
Compiling the instrumented and profile-use objects in different target dirs
would change `OUT_DIR` hashes and GCC would silently look for different names,
emitting `-Wmissing-profile`. Keeping both builds in
`target/pgo/product` also keeps the normal `target/` untouched. Changing the
checkout path invalidates the C profile for the same reason.

### stage4 — profile-use build

```text
RUSTFLAGS="-Cprofile-use=<abs>/target/pgo/out/merged.profdata -Cllvm-args=-pgo-warn-missing-function"
CFLAGS="-fprofile-use=<abs>/target/pgo/c-prof -fprofile-correction -O3"
CFLAGS_x86_64_pc_windows_gnu=<same>
CARGO_TARGET_DIR=<abs>/target/pgo/product
cargo +stable-x86_64-pc-windows-gnu build --profile release-fast --bin pir --features extensions
```

`-fprofile-correction` tolerates counters that cannot be merged exactly (for
example, different branch thread schedules). The result is copied to
`target/pgo/out/pir-pgo.exe` and a manifest is written next to it. The
instrumented binary in `product/` is replaced; rerun stage 1 to train again.

## Toolchain notes (read before running on a new machine)

- **windows-gnu has no `libprofiler_builtins`.** The stock rustup GNU target
  ships the LLVM profile *reader* (`llvm-profdata`) but not the profile
  *runtime*. `-Cprofile-generate` then fails with
  `error[E0463]: can't find crate for profiler_builtins`. Options:
  1. Run the pipeline with MSVC, which ships `profiler_builtins`:
     `PIR_PGO_TOOLCHAIN=stable-x86_64-pc-windows-msvc PIR_PGO_SKIP_C=1 scripts/pgo/pgo.sh`.
     `cl.exe` has no GCC `-fprofile-generate`, so only Rust is profiled.
  2. Stay on GNU and pass `PIR_PGO_PROFILER_RT=C:/path/libprofiler_rt.a`
     (`-ProfilerRuntime` in PowerShell), built from `compiler-builtins` for
     this exact rustc/target. The scripts add `-Zno-profiler-runtime` and set
     `RUSTC_BOOTSTRAP=1`.
  3. C-only training: `PIR_PGO_SKIP_RUST=1` / `-SkipRust`. Rust runs
     unprofiled, QuickJS keeps its gcov profile.
- **MSVC without the Windows SDK.** This checkout's default MSVC toolchain is
  incomplete locally (missing `kernel32.lib`), which is why the default here is
  `stable-x86_64-pc-windows-gnu` plus MinGW GCC. On a foreman machine with VS
  Build Tools + Windows SDK, MSVC is the simpler route *for Rust*; C/QuickJS
  PGO still needs the GNU route.
- **MinGW GCC.** Use the same GCC major version for training and use (the
  profile format is version-specific). The local tree uses GCC 16.2.0 from
  `../tools/mingw/mingw64/bin`. Keep that `bin` directory on `PATH` for stage
  2 as well: `libgcov`/`libgcc_s_seh` DLLs must resolve at runtime.
- **llvm-profdata version.** A mismatch with rustc's LLVM rejects or
  misinterprets `.profraw`. The scripts print both versions in the manifest.
  MSVC also needs the component: `rustup component add llvm-tools-x86_64-pc-windows-msvc`.
- **Linker fallbacks.** If rust-lld cannot resolve a GCC runtime symbol, add
  extra flags through `PIR_PGO_EXTRA_RUSTFLAGS` (applied to stages 1 and 4),
  for example `-Clink-arg=-lmingwex -Clink-arg=-lgcc`, and re-run stage 1.
- **CPU/model.** LLVM PGO data contains branch/call counts, not a CPU model,
  but tuning is still only meaningful for the machine class that produced the
  workload (cache sizes and branch behaviour change which paths are hot).
  Regenerate on the target machine class; never ship a profile built on a
  different CPU family or compiler.

## Profile validity

A profile is bound to **all** of the following; any change invalidates it:

- rustc version, target triple, LLVM version (`llvm-profdata` must match);
- GCC version (gcov format);
- source commit and crate feature set (`--features extensions`);
- codegen flags: profile (`release-fast`), `opt-level`, `lto`, `codegen-units`,
  panic/abort settings, RUSTFLAGS;
- the checkout path and `CARGO_TARGET_DIR` for the C half (object paths are
  encoded in `.gcda` names);
- the workload mix: a profile trained only on microbenchmarks can pessimize
  plugin paths it never saw (`-pgo-warn-missing-function` lists them).

The manifest in `target/pgo/out/profile-manifest.txt` records the commit,
dirty state, CPU, compiler/LLVM versions, both flag sets, workload runs, file
counts, and the PGO binary hash. Upgrading rustc or GCC means rerun stage0.

Do **not** commit `.profraw`, `.profdata`, or `.gcda`; they are large,
machine-specific, and useless without the exact toolchain. `target/pgo/` is
already ignored by `.gitignore`.

## Expected gains and limits

PGO typically buys **5–20 %** for a bytecode interpreter plus host bridge:

- **Higher (roughly 10–25 %)**: QuickJS dispatch/opcode loop, JSON
  stringify/parse, regexp matching, property/atoms, zlib/crypto inner loops —
  branch layout and inlining improve where the workload spends most time.
- **Smaller (2–8 %)**: syscall/FFI-bound work (`fs` reads, SQLite calls) where
  the time is not in our code; PGO cannot remove the kernel or the allocator.
- **Neutral or noisy**: process startup, cold error paths, rarely executed
  shim code; a small binary-size change either way is normal.
- `release-fast` (no fat LTO) is a proxy for the shipped `release` profile
  (fat LTO, 1 CGU). PGO gains and code layout can differ after LTO; validate
  a candidate with the shipped profile separately before release decisions.

PGO is not a JIT: it does not remove interpreter dispatch or add inline caches.
It is also workload-sensitive; an unrepresentative training mix can make some
operations slower. Treat the A/B table as the decision, not the pipeline's
exit code.

## A/B: normal `release-fast` vs PGO

`scripts/pgo/compare.sh` runs the same ignored test twice on the same machine:

```text
cargo +stable-x86_64-pc-windows-gnu test --profile release-fast \
  --features extensions --test perf_parity -- --ignored --nocapture
```

- baseline: `target/pgo/baseline`, profile flags cleared;
- PGO: `target/pgo/product`, `-Cprofile-use` + `-fprofile-use` so cargo reuses
  the stage-4 binary (`CARGO_BIN_EXE_pir` points at it);
- both: `PIR_PERF_RUNS=3` (the harness takes the minimum of three runs per
  operation), `PIR_PERF_THRESHOLD=0` (report only), `PIR_PERF_ONLY` passed
  through.

`tests/perf_parity` measures 51 operations against a real Node and prints
`pir ns/op`, `node ns/op`, `pir/node`. `compare.sh` joins the two runs and
prints per-operation absolute times plus `speedup = baseline pir ns / PGO pir
ns`, a geomean, and improved/regressed counts. It also shows each run's
`pir/node` ratio so node drift is visible. A geomean gate is opt-in:
`PIR_PGO_AB_MIN_SPEEDUP=1.05`.

Rules for a usable measurement:

- same machine, AC power, no other CPU-heavy work, fixed performance plan;
- alternate baseline/PGO if you run them manually; `compare.sh` uses an
  isolated dir per side but still expects a quiet machine;
- look at the distribution, not one op: ±2–3 % is noise, regressions confined
  to I/O ops are usually layout noise;
- never compare a PGO number with a stale profile or a different compiler.

Manual A/B with the env vars:

```bash
# baseline
CARGO_TARGET_DIR=target/pgo/baseline PIR_PERF_RUNS=3 PIR_PERF_ONLY=crypto \
  cargo +stable-x86_64-pc-windows-gnu test --profile release-fast \
  --features extensions --test perf_parity -- --ignored --nocapture
# PGO (same flags as stage4); R=Windows path so native rustc sees it
R=$(cygpath -m "$PWD")
RUSTFLAGS="-Cprofile-use=$R/target/pgo/out/merged.profdata -Cllvm-args=-pgo-warn-missing-function" \
CFLAGS="-fprofile-use=$R/target/pgo/c-prof -fprofile-correction -O3" \
CFLAGS_x86_64_pc_windows_gnu="$CFLAGS" \
CARGO_TARGET_DIR=target/pgo/product PIR_PERF_RUNS=3 \
  cargo +stable-x86_64-pc-windows-gnu test --profile release-fast \
  --features extensions --test perf_parity -- --ignored --nocapture
```

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `error[E0463]: can't find crate for profiler_builtins` | GNU target lacks the runtime. Use MSVC, `PIR_PGO_PROFILER_RT`, or `PIR_PGO_SKIP_RUST=1`. |
| `rust-lld: undefined symbol: __gcov_*` | `libgcov` was not linked. Ensure MinGW GCC was found and re-run stage 1; the script adds `-lgcov` when C profiling is on. |
| zero `.gcda` | `CFLAGS` was clobbered by `rquickjs-sys`; `CFLAGS_x86_64_pc_windows_gnu` must be set (the scripts do this). Also check the process exited normally (gcov dumps at exit). |
| `-Wmissing-profile` in stage 4 | C objects landed at different paths than training. Keep `CARGO_TARGET_DIR` identical in stages 1 and 4 and do not move the checkout. |
| zero `.profraw` | The instrumented binary was not the one executed, or it crashed. Check `target/pgo/out/workload-*.log`; stage 2 fails explicitly. |
| `llvm-profdata` version mismatch | Install `llvm-tools-<target>` for the same rustup toolchain or use the sysroot copy the script picks. |
| workload fails `< 3000 ms` | A phase threw or exited early; the log names the phase. Increase `PIR_PGO_WORKLOAD_MS` only after fixing the error. |
| PGO slower | Recheck workload representativeness and that the profile was generated before the current source; run `compare.sh` again on a quiet machine. |
| repo path has a space | `RUSTFLAGS`/`CFLAGS` are whitespace-separated, so profile paths with spaces cannot be expressed. Move the checkout to a space-free path (the scripts fail fast). |

## Non-goals

- No Cargo profile, feature, asset, test, or `src/` change: the pipeline is
  external and reversible by deleting `target/pgo/`.
- No BOLT/LTO tuning, no cross-machine profile portability, no profile
  committing.
- No plugin-API change: extensions keep loading exactly as before; PGO only
  changes generated code.
