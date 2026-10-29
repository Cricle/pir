# PGO (profile-guided optimization) for pir

Status: **researched and verified on the local Windows GNU toolchain** (2026-10-29).
The `stable-x86_64-pc-windows-gnu` toolchain installed here cannot generate
profiles: `rustup` does not ship `libprofiler_builtins` for that target.
Profile *use* works locally; profile *generation* needs either the MSVC
toolchain, a PGO-capable CI runner, or the experimental self-built runtime
described below.

No `pir` release build has been run with PGO yet. This document defines the
exact commands, workload, cache policy and risks; the foreman runs the build.

---

## 1. Verified toolchain facts

Measured with `RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu` and
`C:\wp\empty\tools\mingw\mingw64\bin` on `PATH`:

| Item | Value |
|---|---|
| `rustc` | 1.99.1 (58cf8f9c3 2026-10-13) |
| host | `x86_64-pc-windows-gnu` |
| rustc LLVM | **23.1.1** |
| `cargo` | 1.99.1 |
| `llvm-profdata` (rustup `llvm-tools` component) | **23.1.1-rust-1.99.1-stable** — matches rustc |
| `rust-lld` | LLD 23.1.1, same `rust-lang/llvm-project` commit as rustc |
| MinGW gcc / ld | gcc 16.2.0 (ucrt, posix-seh) / GNU ld 2.47.20260726 |
| `libprofiler_builtins` for `x86_64-pc-windows-gnu` | **absent** |
| `libprofiler_builtins` for `x86_64-pc-windows-msvc` | present in the MSVC toolchain |
| `libprofiler_builtins` for `x86_64-unknown-linux-gnu` | present in the GNU toolchain's cross std |

`rustup component list | grep llvm-tools` after
`rustup component add llvm-tools-x86_64-pc-windows-gnu`:

```
llvm-tools-x86_64-pc-windows-gnu (installed)
```

Evidence logs: `logs/lane-pgo/toolchain.txt`, `logs/lane-pgo/gnu-pgo-blocker.log`,
`logs/lane-pgo/hello-pgo-run.log`.

### The GNU blocker

```
$ rustc --target x86_64-pc-windows-gnu -C opt-level=3 -C codegen-units=1 \
    -C linker=rust-lld -Cprofile-generate=<dir> hello.rs -o hello-gen.exe
error[E0463]: can't find crate for `profiler_builtins`
  = note: the compiler may have been built without the profiler runtime
```

This is a target-distribution gap, not a linker problem. Once a runtime is
supplied (see §3), `rust-lld` links instrumented binaries and they run
correctly. `-Cprofile-use` does **not** need the runtime and works on the
stock GNU toolchain.

---

## 2. Hello-world PGO experiment

Source: `logs/lane-pgo/tmp/pgo-hello/hello.rs` — a hello-world that also runs
a branch-heavy 60M-iteration loop (`~6%` rare branch), so the run time is
measurable. `logs/lane-pgo/tmp/pgo-hello/run-hello-pgo.sh` reproduces the
whole flow.

Workflow:

```bash
# 1. instrumented build (GNU experimental: -Zno-profiler-runtime + runtime)
RUSTC_BOOTSTRAP=1 rustc --target x86_64-pc-windows-gnu \
  -C opt-level=3 -C codegen-units=1 -C linker=rust-lld -C strip=symbols \
  -Cprofile-generate="$PWD/profraw" \
  -Zno-profiler-runtime -C link-arg="$PWD/profrt-gnu/libprofiler_rt.a" \
  -Cllvm-args=-pgo-warn-missing-function hello.rs -o hello-gen.exe

# 2. run the workload (3 runs; %p keeps one file per process)
LLVM_PROFILE_FILE="$PWD/profraw/run-%p.profraw" ./hello-gen.exe 1000000
LLVM_PROFILE_FILE="$PWD/profraw/run-%p.profraw" ./hello-gen.exe 60000000
LLVM_PROFILE_FILE="$PWD/profraw/run-%p.profraw" ./hello-gen.exe 60000000

# 3. merge
llvm-profdata merge -o merged.profdata profraw/

# 4. profile-use build (no runtime needed on GNU)
rustc --target x86_64-pc-windows-gnu \
  -C opt-level=3 -C codegen-units=1 -C linker=rust-lld -C strip=symbols \
  -Cprofile-use="$PWD/merged.profdata" \
  -Cllvm-args=-pgo-warn-missing-function hello.rs -o hello-use.exe
```

Measured result (60,000,000 iterations, 9 alternating runs, first 2 dropped,
median; `logs/lane-pgo/hello-pgo-run.log`):

| Build | Binary size | Median | Delta |
|---|---:|---:|---:|
| plain `-O3`, CGU=1, no LTO, baseline | 873,472 B | 125.5 ms | — |
| same + `-Cprofile-use` | 872,960 B | 114.3 ms | **+9.80 % faster**, −0.06 % size |
| instrumented build (reference) | 935,424 B | — | +7.1 % size |
| plain `-O3`, fat LTO, baseline | 262,144 B | 117.2 ms | — |
| fat LTO + `-Cprofile-use` | 263,680 B | 117.1 ms | **+0.07 % (noise)**, +0.59 % size |
| fat-LTO instrumented build (reference) | 325,120 B | — | +24 % size |

The merged profile had 11 functions / 63 blocks / 491,562,608 total counts
(2,872 bytes). Three `.profraw` files were produced with `%p`; without
`LLVM_PROFILE_FILE`, three sequential runs overwrote **one** file
(`default_<id>_0.profraw`) — set the pattern explicitly.

Interpretation: PGO is functional end-to-end on the GNU target. The speedup
depends on the build: on a tight loop, fat LTO already removes most of the
headroom the profile would exploit. pir's profile-use build is fat LTO, so
expect modest gains; measure the real binary, do not extrapolate from the
non-LTO 9.8 %. The fat-LTO delta is within run-to-run noise: repeated
self-tests gave −0.72 % to +0.37 %. Treat the self-test as a pipeline check,
not a benchmark.

Startup-only runs (n=0, 15 runs): 3.9 ms baseline vs 3.9 ms PGO — unchanged,
as expected; PGO does not help process startup.

---

## 3. GNU experimental runtime (research only)

The self-built runtime is **not part of the repository and not a supported
build dependency**. It exists because `rust-src` does not ship compiler-rt
sources either; the lane downloaded the `compiler-rt/lib/profile` sources from
the exact `rust-lang/llvm-project` commit reported by `rust-lld`
(`1b9c0d5...`), compiled them with MinGW gcc/g++, and linked them with
`-Zno-profiler-runtime` and `-C link-arg=libprofiler_rt.a`.

One COFF/GCC detail: the upstream Windows runtime places one-byte
`NamesStart`/`CountersStart` sentinels in `.lprfn$A` / `.lprfc$A`; GCC emits
4/8-byte padded sections, so `&NamesStart + 1` points at padding and
`llvm-profdata` reports `symbol name is empty`. The research archive patches
the sentinels to 4/8 bytes and returns `+4/+8`. This is fragile and must not
be used as a production recipe.

**Recommendation:** use MSVC or Linux CI for real profile generation. Use the
GNU runtime only to validate the toolchain or to unblock a local experiment.
The helper script accepts `-ProfilerRuntime <lib.a>` for that path.

---

## 4. pir release profile and PGO interaction

`Cargo.toml [profile.release]`:

```toml
opt-level = 3
lto = "fat"
codegen-units = 1
strip = true
# panic = "abort" deliberately not set: unwinding is load-bearing.
```

Consequences:

- **Generate and use must use the same profile.** Both builds run
  `cargo ... --release`, so `opt-level`, fat LTO, CGU=1 and unwinding come from
  the same `Cargo.toml`. Do not override `CARGO_PROFILE_RELEASE_LTO` or
  `CARGO_PROFILE_RELEASE_CODEGEN_UNITS` between the two builds. `panic=abort`
  changes the CFG and invalidates profiles; keep the current setting.
- **`strip = true` is harmless for PGO**: stripping happens after codegen;
  LLVM needs names/hashes at compile time, not in the final binary.
- **Fat LTO + CGU=1 + instrumentation is the memory peak.** Instrumented
  objects are larger; a single CGU and a fat-LTO link can need several GB per
  rustc/link process. Run profile generation alone, not next to tests or
  another release build. If the machine OOMs, lower to `lto = "thin"` for
  *both* generation and use (and record that choice), rather than mixing.
- **`-Cllvm-args=-pgo-warn-missing-function`** on the use build reports
  functions that have no profile record. Expect new/changed code to appear
  there; the warning is coverage telemetry, not a hard error. A large missing
  list means the workload does not cover that area or the profile is stale.
- **Instrument the whole graph, not only the bin crate.** `cargo rustc
  --release --bin pir -- <flags>` applies the extra flags only to the named
  target; pir's code lives in the library crate as well, so most functions
  would be left uninstrumented. Use `RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS`
  with `cargo build --release --bin pir` so the lib, bin, dependencies and
  build scripts all see the same `-Cprofile-*` flags. Changing `RUSTFLAGS`
  rebuilds every crate; use the same target dir only if you accept that cost,
  or separate target dirs for generate and use. `CARGO_PROFILE_RELEASE_*`
  only overrides Cargo profile settings; it cannot inject `-Cprofile-*`.
  Trade-off: whole-graph instrumentation includes non-generic dependency code
  (better coverage) but is slower and produces larger instrumented objects.
  This was validated with a temporary Cargo project containing a lib, a bin
  and a `build.rs` (GNU + self-built runtime).

---

## 5. Workload recommendation

The profile should exercise the release `pir.exe`, not test binaries.

| Workload | Command | Covers | Reproducible? |
|---|---|---|---|
| CLI startup | `pir --version` | argv/clap, early init, version | yes, trivial |
| Core agent loop + tools + sessions + compaction | `python scripts/soak/soak_headless.py --pir <instrumented> --turns 100 --interval 10 --log <dir>` | `--mode json --print` agent loop, text and bash/read tool turns, session persistence, compaction, JSON events | **yes** — hermetic local OpenAI-compatible mock, no key, no network |
| TUI startup/render (optional) | `python scripts/tui_conpty/run_battery.py --pir <instrumented> --only s01*` | TUI init, rendering, key handling, quit/restore | yes (ConPTY + `pyte`; hermetic mock) |
| Direct `pir print` with a real provider (optional) | `pir print --mode json "<prompt>"` | provider HTTP/SSE, model streaming | no — network/model dependent; use only as a supplement |

For all workloads set `PI_OFFLINE=1` (and `PIR_NO_UPDATE_CHECK=1`) so a
profile run cannot touch update/catalog endpoints; the mock provider itself is
local. The helper script sets both. The optional TUI workload needs Python
`pyte` and writes its own `scripts/tui_conpty/out/` (see that directory's
README).

Recommended set for a release profile:

1. `pir --version` (one run) — cheap startup coverage.
2. `soak_headless.py --turns 100 --interval 10` — the primary workload.
3. Optionally one TUI scenario (`s01*`) and a `pir print` run against the
   team's staging gateway.
4. Repeat the whole set 2–3 times and merge all `.profraw` files; branch
   weights average out and the profile is less sensitive to a single run.

`soak_headless.py` drives `pir --mode json --print`, so it subsumes a single
`pir print` mock turn; there is no separate mock server to maintain. The
`print` workload in the helper script is for a real gateway/model (it needs a
provider configuration), and is optional for PGO.

`g_agent` / `g_ai` tests are **not** a valid profile source for the release
binary: they are separate test executables built with the dev/test profile and
the test harness; their crate hashes and CFGs do not match
`target-pgo/release/pir.exe`. They are useful as functional gates, not as
PGO input. If PGO for the test suite itself is ever wanted, build the test
binaries instrumented with the same profile settings and collect separately.

---

## 6. Build flows

### 6.1 Local GNU (experimental; whole-graph)

```powershell
$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-gnu'
$env:CARGO_TARGET_DIR = '<repo>\target-pgo'
$env:CARGO_INCREMENTAL = '0'
$env:RUSTC_BOOTSTRAP = '1'
$sep = [char]0x1f
$env:CARGO_ENCODED_RUSTFLAGS = @(
  '-C', 'profile-generate=<repo>\target-pgo\pgo\profiles',
  '-Zno-profiler-runtime',
  '-C', 'link-arg=<path>\libprofiler_rt.a'
) -join $sep
cargo build --release --bin pir

$env:LLVM_PROFILE_FILE = '<repo>\target-pgo\pgo\profiles\pir-%p-%m.profraw'
# run the workloads here

llvm-profdata merge -o <repo>\target-pgo\pgo\profiles\merged.profdata `
  <repo>\target-pgo\pgo\profiles

$env:CARGO_ENCODED_RUSTFLAGS = @(
  '-C', 'profile-use=<repo>\target-pgo\pgo\profiles\merged.profdata',
  '-C', 'llvm-args=-pgo-warn-missing-function'
) -join $sep
cargo build --release --bin pir
```

`target-pgo\release\pir.exe` is the profile-use binary. The helper script does
this flow with `-Generate -Execute` / `-Use -Execute`. Whole-graph
instrumentation also instruments build scripts and proc-macro executables;
with the GNU experimental runtime the link-arg is applied to those builds too
(validated on a temp lib+bin+build.rs project). If a proc-macro DLL refuses
the static runtime, use MSVC/Linux CI for the real profile.

### 6.2 CI (MSVC and Linux; recommended for release artifacts)

Whole-graph instrumentation, in a dedicated target dir, on a PGO-capable
runner:

```bash
export RUSTFLAGS="-Cprofile-generate=$PWD/target-pgo/profiles -Cllvm-args=-pgo-warn-missing-function"
cargo build --release --locked --bin pir
# run smoke/soak/TUI workloads with LLVM_PROFILE_FILE=%p-%m.profraw
llvm-profdata merge -o target-pgo/profiles/merged.profdata target-pgo/profiles

export RUSTFLAGS="-Cprofile-use=$PWD/target-pgo/profiles/merged.profdata -Cllvm-args=-pgo-warn-missing-function"
cargo build --release --locked --bin pir
```

Notes:

- Use `CARGO_ENCODED_RUSTFLAGS` (flags joined by `\x1f`) when paths may contain
  spaces; `RUSTFLAGS` splits on whitespace.
- Whole-graph `RUSTFLAGS` also instruments build scripts and proc-macro
  executables. Their profiles land under the `-Cprofile-generate` directory as
  `default_*.profraw` (verified in a temp Cargo project with a lib, bin and
  `build.rs`) and are merged with the rest; extra build-script functions are
  harmless. Keep that directory under `target-pgo/`.
- Generate and use must use separate target dirs (or `cargo clean`) because
  every dependency is compiled differently. The helper script reuses
  `target-pgo/` and lets Cargo rebuild when `CARGO_ENCODED_RUSTFLAGS` changes;
  use separate dirs if you need faster switching.
- Windows MSVC needs VS Build Tools / Windows SDK; `windows-latest` has both.
- `x86_64-pc-windows-gnu` is **not** a release target in
  `.github/workflows/release.yml` (release matrix is Linux musl, Windows MSVC,
  macOS universal), so CI PGO does not need the GNU workaround.
- There is no `scripts/build-release.bat`; the local release entry is
  `target/build-release.bat` (GNU toolchain, `CARGO_TARGET_DIR=target`,
  `cargo build --release --bin pir`). The PGO helper is a separate experiment
  and never writes to `target/`.
- Linux musl and macOS PGO were not verified from this Windows host; their
  `profiler_builtins` availability must be checked on the runner
  (`rustup component add llvm-tools` and a `-Cprofile-generate` smoke test).

---

## 7. Reproducibility and caching

- **Do not commit `.profdata` / `.profraw`.** They are binary, large, and
  valid only for one source revision + compiler + target + flag set.
- The profile is keyed by: git source SHA, rustc exact version (1.99.1),
  LLVM version (23.1.1), target triple, Cargo profile settings (fat LTO,
  CGU=1, opt-level=3, unwinding), and workload definition/version. Change any
  of these → regenerate.
- `rust-toolchain.toml` tracks `stable`; a stable bump changes LLVM and
  invalidates profiles. For release PGO pin the exact toolchain
  (`1.99.1-x86_64-pc-windows-msvc`, etc.) in the PGO job.
- CI: store the merged profile as an artifact with a key like
  `pgo-v1-${{ github.sha }}-linux-musl-rustc1.99.1`; restore only an exact
  SHA match. Do not use broad restore-keys for a different source revision:
  stale profiles silently degrade code and produce many
  `pgo-warn-missing-function` warnings. For tag builds, always regenerate
  (the release matrix already builds each tag once).
- Local: keep profiles under `target-pgo/` (not `target/`, not the repo).
  `.gitignore` currently covers `/target/`, `/target-lane-*/`, `/target-alt/`
  and `/target-exp-*/` but **not** `/target-pgo/`; the foreman should add it in
  a separate change (this lane cannot edit `.gitignore`).
- Incremental development (`cargo check`, `cargo test`, debug builds) never
  uses PGO. PGO only applies to the explicit release profile build.

---

## 8. Risks / known issues

| Risk | Detail / mitigation |
|---|---|
| GNU runtime missing | E0463 on stock `stable-x86_64-pc-windows-gnu`; use MSVC CI or the experimental runtime. |
| Build time | Instrumented build is slower and produces larger objects; profile-use build is close to a normal release build. Budget ~2× compile + workload time. |
| Memory | Fat LTO + CGU=1 + instrumentation is the peak; run alone, 16 GB+ recommended. Fall back to thin LTO for both phases if needed. |
| Binary size | Instrumented: +7 % (hello, non-LTO) to +24 % (hello, fat LTO). Final PGO: −0.06 % to +0.59 % in the hello tests. pir's ~67 MB release binary may move ±1–2 %; measure. |
| Missing functions | New/unchanged code not in the profile gets default heuristics; `-pgo-warn-missing-function` lists them. A stale profile can pessimize hot code. |
| LTO interaction | Fat LTO already captures much cross-crate inlining; PGO gains on tight loops can be near zero (hello: +0.07 % with fat LTO vs +9.8 % without). Benchmark before shipping. |
| Profile file collisions | Default `default_<id>_0.profraw` is overwritten by sequential runs; set `LLVM_PROFILE_FILE=...-%p-%m.profraw`. |
| Windows GNU + rust-lld | rust-lld 23.1.1 links instrumented PE/COFF correctly; the missing runtime, not the linker, is the blocker. The self-built runtime needs the COFF sentinel alignment patch. |
| Cross-target profiles | A profile generated on MSVC/Linux is not a supported input for a GNU build; function hashes/CFG can differ. Treat as unsupported; generate and use on the same target. |
| Debug builds | PGO at `opt-level=0` has no meaningful benefit; never wire it into dev/test profiles. |
| BOLT | Not available on Windows (BOLT is ELF/Linux); not a fallback here. |

---

## 9. Helper script

`scripts/pgo/build-pgo.ps1`:

```powershell
# prove the toolchain (hello-world, fat LTO)
.\scripts\pgo\build-pgo.ps1 -SelfTest `
  -ProfilerRuntime .\logs\lane-pgo\tmp\pgo-hello\profrt-gnu\libprofiler_rt.a

# print the plan only (default); artifacts stay under target-pgo/
.\scripts\pgo\build-pgo.ps1 -Generate -Workload soak `
  -ProfilerRuntime C:\path\libprofiler_rt.a

# execute it (build + 100-turn soak + merge)
.\scripts\pgo\build-pgo.ps1 -Generate -Workload soak `
  -ProfilerRuntime C:\path\libprofiler_rt.a -Execute

# profile-use build; copies to target-pgo\pgo\out\pir-pgo.exe
.\scripts\pgo\build-pgo.ps1 -Use -Execute
```

Parameters: `-Generate` / `-Use` / `-SelfTest`, `-PirExe`, `-ProfileDir`,
`-Workload smoke|print|soak|tui|all`, `-OutDir`, `-Toolchain gnu|msvc`,
`-MingwBin`, `-ProfilerRuntime`, `-Model`, `-Execute`. Defaults write only
under `<repo>/target-pgo/`; the script refuses paths under `<repo>/target/`.
`-Generate`/`-Use` are dry-run unless `-Execute` is passed.

---

## 10. Unverified / next steps

- No full `pir` PGO build has been run (lane rule: no pir build). The first
  real run must measure: build wall time, peak RSS, final binary size,
  `pgo-warn-missing-function` count, and an A/B (cold start, print, TUI, soak)
  against the current release binary.
- MSVC PGO was not run locally: the MSVC toolchain has
  `profiler_builtins`, but this host has no VS Build Tools/Windows SDK
  (`kernel32.lib` missing), so MSVC linking fails. CI `windows-latest` is the
  intended environment.
- Linux musl / macOS PGO not verified.
- The experimental GNU runtime is not shippable; treat it as a research
  artifact. Full-graph `RUSTFLAGS` + self-built runtime was validated only on
  a temp lib+bin+`build.rs` project, not on pir's proc-macro dependency graph;
  if a proc-macro DLL rejects the static runtime, use MSVC/Linux CI.
- `.gitignore` handling of `target-pgo/` is a foreman decision (this lane
  cannot edit `.gitignore`).
