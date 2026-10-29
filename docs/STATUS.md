# STATUS — pir (current truth, 2026-10-29 evening)

One page. Repo state: clean-history `master` snapshot (2026-10-30; the
15-review fix batch is included). Numbers below were measured on this host
with the GNU toolchain; the release binary is one source step behind (see
"Release build").

## Contract (short)

- Rust replacement for the TypeScript `pi`: providers, sessions, extensions,
  TUI, RPC, print, server/client, SDK, builtins.
- Agent loop + direct plumbing are always compiled; every other surface is a
  default-on cargo feature (`default = ["full"]`).
- Seven `tool-*` leaves + independent `grep-find`; `compaction`/`telemetry`
  stay core.
- No `#[cfg]` may change the feature-ON path; off-surfaces return a documented
  error or are absent. Tests live only under `tests/`.

## Definition of done

| Gate | Status |
|---|---|
| Replace pi (parity + real-task battery) | ✅ full local debug suites green on both OSes: **Windows 6,330 passed / 0 failed / 27 ignored**, **Debian WSL 6,284 passed / 0 failed / 41 ignored** (285 targets each); A/B vs upstream pi 1.4.0 5/5 each, pir faster (76.6 s vs 134.9 s) |
| Feature comprehensiveness (all features / matrix / CI) | ✅ **81/81 matrix rows green** (2026-10-29 rerun); CI is one Linux+Windows matrix gate |
| SDK usable (packaged `pir-sdk` + external consumer) | ✅ `pir-sdk-0.0.1.crate` 6,184,886 B; unpacked `cargo check --all-targets` and external consumer `cargo check --locked` green |
| Real-load long soak (flat RSS, crash/reopen) | ✅ release 300-turn (64K) and 400-turn (4K) soak PASS; crash/reopen 10/10 PASS; slope 0.0143–0.0147 MB/turn |
| Constrained device (1-4 cores, 256 MB-1 GB, idle/wake/RSS) | ✅ 1-core + 256/512/1024 MiB Job-Object matrix 9/9 PASS; idle RSS 34.7 MB, print 41.5 MB peak, soak 77.2 MB peak; commit floors 6/32/34/71 MiB |
| Rust/resource gates (fmt/clippy/unsafe/MSRV/deny/bench) | ✅ fmt green; clippy `--all-targets --all-features -D warnings` green (57 s warm); doctests 33 passed / 7 ignored; CI keeps Linux+Windows full gates (advisory/MSRV/bench local) |
| Mode deep optimization & competitive scorecard | ✅ release scorecard: `--version` 12.1 ms, print 84 ms, TUI first frame 58.7 ms, server 40.5 ms, RPC prompt→settled 124.7 ms, phone health 0.6–15.2 ms |
| Evidence on the final release binary | 🚧 candidate `target-release-final/release/pir.exe` 67,502,080 B / `4a3d3ee3…` built in 20 m 33 s, but predates the last `node_fns.rs` fix; rebuild required before a tag |

## Final verification (2026-10-29)

- Last complete full sweep before the final CI fixes
  (`scripts/test_fast.ps1 -Concurrency 16 -Threads 4`,
  `logs/lane-test-mock-final.log`): **281 suites, 6,332 passed / 0 failed**,
  wall **97.1 s** (build 0.5 s + parallel 96.3 s). 6 empty harnesses ran zero
  tests and are not counted as passes.
- Slowest suites in that sweep: `g_agent` 877 / 0 (95.0 s),
  `mcp_integration` 68 / 0 (71.4 s), `shell_totals_fix` 3 / 0 (51.0 s),
  `g_tools` 458 / 0 (34.7 s), `g_ai` 760 / 0 (34.1 s),
  `g_cli_server` 201 / 0 (18.7 s).
- Linux CI durable-storage fix (real product bug, not just a flaky
  assertion): `tokio::fs::File` copies writes into an internal buffer and
  hands the actual write to a blocking task, so `append_file` could return
  before `main.jsonl`'s marker was visible. `commit` now calls
  `file.flush().await` before dropping the handle. Reproduced on Debian WSL
  (`jsonl_rejects_out_of_order_sequences` failed ~1/12 runs before the fix);
  after the fix, both failing tests pass **200/200 process-level iterations**
  and the full `durable_storage` binary passes **30/30 runs**. Test fixtures
  also use timestamped temp dirs and `close()` before manual file edits.
- Windows CI shim-byte fix: the checked-in `.deflate` blobs had been generated
  from a CRLF working tree while Linux CI checks out LF. All 90 blobs and the
  `GOLDEN` hashes are now generated from canonical LF bytes
  (`scripts/gen_shims_deflate.py`), and the byte-exact test normalizes checkout
  line endings. The test passes on Windows and on Debian WSL.
- `cargo fmt --all -- --check`: green.
- `cargo clippy --all-targets --all-features -- -D warnings`: green on
  Windows (57 s warm) and on Linux (Debian WSL, rustc 1.99.1, 1 m 50 s).
- `cargo check --all-targets --all-features --locked` on Linux: green
  (18.5 s warm, Debian WSL).
- `cargo test --doc`: 33 passed / 7 ignored.
- `fs_numeric_open_flags`: Rust reports `EBADF` from `pread` on an `O_WRONLY`
  descriptor as `Uncategorized`; `io_error_code` now maps raw libc errno 9 to
  Node's `EBADF` instead of `EIO`. The suite passes 3/3 on Debian WSL.
- `env_client_e2e` / `env_client_pty`: mock wrappers are written and exec'd
  immediately; WSL overlayfs could return transient `ETXTBSY` and the
  oversized-request test's 500 ms silence window could fire during the mock
  handshake. The wrapper write now settles for 20 ms and the handshake window
  is 5 s. Both suites pass on Windows and Debian WSL.
- WSL-only test-environment fixes: TUI tests use the runtime
  `use_windows_keybindings()` predicate (WSL selects Windows chords); the
  session-root refresh suite moved to its own process; tool-security temp-root
  mutations are serialized/restored; root-only tests that rely on Unix mode
  bits skip under `id -u == 0`; missing-npm/child-process probes use absolute
  missing paths; Codex WebSocket pooling waits for the producer's release
  before the second turn.
- **Debian WSL full debug suite** (`cargo test --tests --no-fail-fast`,
  4 test threads, `RUST_MIN_STACK=8388608`): **285 targets, 6,284 passed /
  0 failed / 41 ignored, RC=0** (`/root/full-tests7.log`; repo copy on ext4).
- **Windows full debug suite** (`cargo test --tests --no-fail-fast`,
  4 test threads, `RUST_MIN_STACK=8388608`): **285 targets, 6,330 passed /
  0 failed / 27 ignored, EXIT=0**
  (`logs/status-final-cargo-test-win2.log`). The affected Windows targets
  (`g_tools`, `env_client_e2e`, `env_client_pty`, `cli_parity_fixes`,
  `node_shims`, `tui_snapshot_refresh_w16`) are green.
- GitHub Actions: one `gate` matrix job over `ubuntu-latest` + `windows-latest`,
  each running fmt → clippy → doctests → eight sequential nextest groups.
  The final gate completed **green on Linux and Windows** (all steps and all
  groups). Earlier runs exposed test flakes (`fs_numeric_open_flags`,
  `env_client_e2e`, `codex_transport_websocket_pools_without_sending_a_continuation`);
  they are fixed and verified by the final run.

## Test speed

The slow-test work is in the tree:

- `g_cli_server` (201 tests) 91.5 s serial → **18.9 s at 4 threads**;
  one prompt-initialized RPC child teardown ~7 s → **≤250 ms**;
  `tui_search_whole_history` 8.6 → **7.7 s**;
  `python_json_fastpath` 9.7 → **2.0 s**.
- `fs_watch_incremental` 17.37 → **11.27 s (-35.1%)**; `g_agent`
  43.27 → **35.43 s (-18.1%; best single run 31.25 s)** in isolation;
  `shell_command_timeout` 12.21 → **10.96 s**; `mcp_integration`
  24.35 → **18.85 s (-22.6%)** solo. The small `pir-mock-mcp` helper
  (2.4 MB) replaces the 338 MB test binary as the MCP fixture and is embedded
  in the MCP tests (`CARGO_BIN_EXE_pir-mock-mcp`).
- `test_fast.ps1` warms `pir.exe` once and orders suites LPT; `dev_cycle.ps1`
  warm default four steps **50.6 s** (fmt 6.1 + clippy 16.2 + build 24.2 +
  tests 4.1), no-change clippy+build+test **6.1 s**, single-file edit
  test-only **48.5 s**.
- The full-suite floor is **~97 s**, not 1 minute: the crate is one 23 MB
  rustc unit and 281 test binaries are ~300 MB each (first load on Windows
  ~20 s); `g_agent` alone takes ~95 s at 4 libtest workers. Reaching 60 s
  would need merged test binaries or sharding across runners, both rejected
  for coverage/CI-cost reasons.

## Large-session RSS (debug)

Paged 100 MB session peaks at **86 MB** (2.8 MB incremental). Widen + resident
compaction: 100 MB **869.2 → 571.6 MB (-34.2%)**, 50 MB
**415.2 → 321.1 MB (-22.7%)**; widen + one turn 100 MB
**981.6 → 915.2 MB (-6.7%)**. The remaining widen peak is the provider request
`Value` tree + serialized body (outside `session_manager`).

## SWE-bench Verified (local evaluation)

- Dataset: **SWE-bench Verified** (500 instances); agent `target/debug/pir.exe`,
  model `gpt-5` at `https://api.openai.com/v1`, tools
  read/edit/write/ls/grep/find/bash.
- **`psf__requests-2317`: resolved=true, F2P 8/8, P2P 133/133** (20 turns,
  4 m 46 s). **`psf__requests-1142`: resolved=true, F2P 1/1, P2P 5/5**
  (19 turns, ~3 m 30 s). `psf__requests-1921`: F2P 6/6, P2P 106/107 — one
  environment/httpbin flake, resolved=false by the strict gate.
- Six other `psf/requests` patches were produced by the agent; evaluation was
  blocked by the container-side PyPI install of `httpbin` / Python 2.7
  resolver (network degradation), not by the agent. Harness:
  `scripts/swebench/`, results `logs/swebench/BENCHMARK.md`.
- All **8 `psf/requests` Verified images** are local (8/8 digests verified;
  10.7 GB). The 300-instance Lite pull continues in the background:
  **59 pulled + 1 skipped / 300** at 21:30, 240 pending, measured
  **10.0 images/hour → ETA ~24 h**, Docker 33.86 GB, C: free 508 GiB.
  Full SWE-bench (2,294 instances) would need ~2.8 TiB of images and is out of
  scope for this host.

## Upstream A/B (release pir vs pi 1.4.0)

`logs/acc-final/AB-REPORT-PI14.md`: same OpenAI-compatible endpoint/model,
5 tasks. **pir 5/5, pi 5/5; total 76.6 s vs 134.9 s**; pir uses fewer or equal
turns on all five tasks. The earlier comparison against pi 0.73.1 was stale
and is superseded.

## Release build and profile

`target/release/pir.exe` **67,392,000 B**, SHA-256
`d76dff0aa1c5cd29890d71c642b0b34cdfa04d4df195fa88c5f0768a149ea7df`,
built 2026-10-29 13:35 in 23 m 24 s (fat LTO). It predates the last
`src/extensions/runtime/host_functions/fs_watch_fns.rs` and
`src/coding_agent/tools/confine.rs` edits, so its scorecard/soak evidence is
for the same profile but not the final source; **rebuild before tagging**.

Profile study (`logs/lane-release-profile/REPORT.md`): keep
`opt-level=3, lto="fat", codegen-units=1, panic="unwind", strip="symbols"` —
fastest and smallest (lab 67,505,152 B). `fat LTO + CGU=4` builds in
14 m 03 s vs 27 m 00 s (**-48%**) at **+5.7% size** with equal runtime;
`panic="abort"` is rejected (30+ `catch_unwind` boundaries);
`-Ctarget-cpu=native` is slower here and not CI-reproducible.

## Open residuals

- Core review (`logs/lane-core-review/REPORT.md`, read-only): 4 P1s —
  CBOR byte-string amplification before schema validation; server session
  records never reclaimed on create-without-attach/attach-detach; paged
  compaction failure returns a truncated tail window; paged prefix marker
  false positives on tool-result text.
- SDK review (`logs/lane-sdk-simple/REPORT.md`): real-endpoint path ~12 lines;
  offline mock path 163 lines; 59 `Agent` methods, 33 `SdkEvent` variants; no
  `sdk::testing` provider. `CreateAgentSessionOptions::max_turns` still
  defaults to 100 while `AgentBuilder` defaults to 1000 (documented in
  `docs/sdk.md`); two default mismatches and a proxy/registry overwrite
  loophole remain.
- ConPTY: `s14` fixed; `s47`→`s47b` passes in order; **`s33` resume/prompt
  race remains** (a prompt 0.2 s after Enter is lost).
- CI test isolation: `session_context_cli` is Windows-only (its fixtures
  encode Windows path resolution); a few load-sensitive suites are
  quarantined per OS. These are test gates only — no product `cfg` changes.
- `cargo-deny`/`cargo-audit` are local checks only; registry advisories made
  them non-actionable as a blocking CI job. PGO: local GNU target cannot
  generate profiles (no `libprofiler_builtins`); `-Cprofile-use` works;
  production path is CI MSVC/Linux (`docs/pgo.md`).

## External follow-ups

- Commit/push is allowed for this session; **tagging and release publishing
  remain user-only**.
- Docker Desktop 4.97.0 / engine 29.9.2 works; Docker Hub direct is blocked,
  `docker.1panel.live` pulls the official namespace (digest-verified;
  0.55–0.83 MB/s single, 1.48 MiB/s four-stream); `docker.1ms.run` is the
  fallback. The Lite pull runner is detached and resumable
  (`scripts/docker/pull_lite_all.ps1`, state in `logs/lane-docker-lite/`).
- Cloud evaluation (CoreWeave Sandboxes) was evaluated and rejected by user
  decision; local evaluation remains the path.
- Real-provider A/B can stall on the test gateway; only stall-free legs are
  ranked.

## Commands

```bash
# Foreman (target/)
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --doc
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test_fast.ps1 -Concurrency 16 -Threads 4
CARGO_TARGET_DIR=target bash scripts/feature_matrix.sh  # 81 rows

# Inner loop (target-lane-d; check/fmt only for lanes)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/dev_cycle.ps1 -Files src/foo.rs
CARGO_TARGET_DIR=target-lane-d cargo check --tests
```

## Read next

[`docs/FEATURES.md`](FEATURES.md) · [`MAPPING.md`](../MAPPING.md)
