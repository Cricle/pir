# STATUS — pir (current truth, 2026-10-30)

Clean-history `master` snapshot; the 15-review fix batch is included. Numbers
below are from this host with the GNU toolchain.

## Contract

- Rust replacement for TypeScript `pi`: providers, sessions, extensions, TUI,
  RPC, print, server/client, SDK, builtins.
- Agent loop + direct plumbing always compile; other surfaces are default-on
  features (`default = ["full"]`). Seven `tool-*` leaves + `grep-find`;
  `compaction`/`telemetry` stay core.
- No `#[cfg]` may change the feature-ON path; off-surfaces return a documented
  error or are absent. Tests live under `tests/`.

## Definition of done

| Gate | Status |
|---|---|
| Replace pi | ✅ Windows **6,364/0** across 290 harnesses (6 empty; 83.5 s parallel); Debian WSL **6,318/0/41** after three load/env flakes re-ran green; A/B vs pi 1.4.0: 5/5 both, **76.6 s vs 134.9 s** |
| Features / matrix / CI | ✅ **81/81** matrix rows green; CI is one Linux+Windows matrix gate, final run green |
| SDK | ✅ `pir-sdk-0.0.1.crate` 6,184,886 B; packaged/unpacked checks and external consumer green |
| Soak | ✅ release 300-turn (64K) + 400-turn (4K); crash/reopen 10/10; slope 0.0143–0.0147 MB/turn |
| Constrained device | ✅ 1-core × 256/512/1024 MiB matrix 9/9; idle 34.7 MB, print 41.5 MB, soak 77.2 MB peak |
| Rust/resource gates | ✅ fmt, clippy all-targets/all-features, doctests 33/7; Linux checks in Debian WSL |
| Scorecard (release) | ✅ version 12.1 ms, print 84 ms, TUI 58.7 ms, server 40.5 ms, RPC 124.7 ms |
| Final release binary | 🚧 candidate `target-release-final/release/pir.exe` 67,502,080 B (`4a3d3ee3…`), predates the last source fix; rebuild before tag |

## Verification

- Full debug suites: Windows `test_fast.ps1 -Concurrency 12 -Threads 4
  -NoBuild` **6,364 passed / 0 failed** across 290 harnesses (6 empty; 83.5 s
  parallel, 84.2 s wall). Debian WSL (`RUST_MIN_STACK=8 MiB`, 4 threads, 294
  targets): full sweep **6,315 passed / 3 failed / 41 ignored**; the three
  were environment/load flakes (`forbidden_files` needs a git worktree,
  `spill_sweep` and `python_plugin abort` are real-timer tests). With the
  worktree initialized and the three targets re-run alone: **6,318 passed /
  0 failed / 41 ignored**.
- `cargo fmt --all -- --check`, Linux+Windows clippy (`-D warnings`), doctests
  (33 passed / 7 ignored) and Linux `cargo check --all-targets` green.
- CI: Linux and Windows `gate` matrix green, including fmt, clippy, doctests
  and all eight nextest groups.
- Key fixes in the final batch: durable JSONL `append_file` now flushes Tokio
  File's internal write before return; shim blobs are canonical LF; Linux
  `EBADF` maps to Node's code; Codex WS pooling waits for `Lease::release`;
  CBOR byte strings are rejected before `serde_json` expansion; RPC client
  Drop/teardown, server detach registry cleanup, and the WSL/CI test fixtures
  were hardened.
- Startup bootstrap (pi 1.4 parity): a fresh `HOME`/`PIR_AGENT_DIR` with no
  `~/.pi` now creates `auth.json` and `models-store.json` as `{}` with the
  upstream modes (0600 files in a 0700 agent dir); `settings.json`,
  `models.json` and the sessions tree stay on-demand. Six regression tests in
  `tests/agent_bootstrap.rs` cover layout, preservation, overrides, IO failure
  and the `--list-models` repro.
- TUI/context-mode fixes: thinking selectors now offer only the active
  model's supported levels (so `medium` cannot clamp to `high`); explicit
  model/thinking picks apply **and** persist `defaultProvider`/`defaultModel`/
  `defaultThinkingLevel`; `thinking_level_changed` refreshes both footer rows;
  autocomplete accept closes the popup, and Tab no longer gets coalesced into
  a literal-tab paste; `/image` and all dynamic command providers are listed
  and refreshed on reload; `ctx_fetch_and_index` uses the same
  `httpProxy`/`PI_HTTP_PROXY`/`HTTPS_PROXY` resolution as provider traffic
  (Python HTTPS remains a documented embedded-interpreter residual).
- Zero-feature minimal agent (`pir::minimal` / `pir-minimal`): builds with
  `--no-default-features`, runs one OpenAI-compatible `/chat/completions`
  provider, one confined `read_file` tool and an in-memory transcript; mock
  SSE end-to-end and tool loop are covered by `tests/minimal_profile.rs`.

## Test speed

`g_cli_server` 91.5 → 18.9 s at 4 threads; `g_agent` 43.3 → 35.4 s;
`mcp_integration` 24.4 → 18.9 s; `fs_watch_incremental` 17.4 → 11.3 s.
`dev_cycle.ps1` warm four-step loop is **50.6 s**; no-change rerun 6.1 s.
The full debug suite now runs in **83.5 s parallel / 84.2 s wall**
(`test_fast.ps1 -Concurrency 12 -Threads 4 -NoBuild`, 6,364 tests): the old
18-minute stall was not a test sleep but `Start-Process` blocking 10–30 s per
cold ~300 MB test image under Defender; the runner now launches through
`cmd.exe` (non-blocking) and records each child's exit code. `cargo nextest`'s
discovery pass still pays that per-binary cold-start cost, so local full runs
use `test_fast.ps1`; the longest suites are `http_shims` 25.2 s, `g_agent`
24.9 s, `g_ai` 17.5 s.
`pir-mock-mcp` (2.4 MB) replaces the old 338 MB MCP fixture.

## Large-session RSS (debug)

Paged 100 MB session: 86 MB peak (2.8 MB incremental). Widen + resident
compaction: 100 MB **869.2 → 571.6 MB (-34.2%)**, 50 MB
**415.2 → 321.1 MB (-22.7%)**.

The 2026-10-30 debug-start pass removed the construction-time transcript
copies and the remaining hot-path allocations:

- projection build on a large session: **16.1 → 7.8 MB**, 48k → 18k
  allocations, **21.5 → 9.3 ms** (-51% / -62% / -57%);
- `AgentSession::new` on 40,001 messages: **130 → 56 ms** build time and
  **238.2 → 129.7 MB** peak RSS (-57% / -45.5%);
- paged open adopts the tail page's `Arc<FileEntry>` handles (no per-entry
  deep clone); `/tree` and partial session-list publishes no longer make
  transcript/summary copies;
- catalog/startup: `--list-models` -14.9%, `print` -14.5%, TUI first frame
  -22.2%, TUI peak working set **68.5 → 63.6 MB** (openrouter);
- compaction: conversation serialize **3.0x**, split **10.6x**, prepare
  **1.56x** faster (debug);
- plugin host (1000 synthetic plugins): event-interest lookup -97.5%,
  `turn_end` emit -52%, `message_update` -80%, tool backend lookup -99%,
  resolved tools -98%, resolved commands -90%; indexes live with the host
  snapshot and are replaced on rebuild;
- context-mode hot path: routing -52–71% (`git status` 3.1→1.5 µs), tool-result
  weight -14–38% with zero-copy single-block handling, HTML→text 512 KiB
  -26%; FTS/`ctx_search` unchanged because bm25 dominates.

TUI refresh research: idle CPU is already event-driven (16 ms active poll is
dirty-gated; 1 s idle). The remaining cost is that every dirty frame rebuilds
the whole materialized band/window instead of the visible viewport:
`TuiCore::frame_lines`/`ChatContainer::render_into` scan all content,
`Rows::push` compares whole strings, and `TranscriptWindow::sync` rescans up
to 512 entries' JSON payloads. The proposed fast path is viewport-only render
plus a window byte-budget cache invalidated by entry revision, and skipping
`set_entries` when the window and revision are unchanged (not yet landed).

## SWE-bench Verified (local)

`psf__requests-2317` resolved=true (F2P 8/8, P2P 133/133);
`psf__requests-1142` resolved=true (F2P 1/1, P2P 5/5);
`psf__requests-1921` F2P 6/6, P2P 106/107 (one httpbin env flake). Six more
patches were produced; evaluation was blocked by container-side PyPI/httpbin/
Python 2.7 environment issues. All 8 `psf/requests` images are local
(10.7 GB). Lite pull: 59 pulled + 1 skipped / 300, ETA ~24 h.

## Release

`target/release/pir.exe` 67,392,000 B, SHA-256
`d76dff0a…149ea7df`, built in 23 m 24 s (fat LTO). Profile study recommends
keeping `opt-level=3, lto="fat", codegen-units=1, panic="unwind",
strip="symbols"`; fat LTO + CGU4 builds 48% faster at +5.7% size.

## Open residuals

- Core review: the 15-review P1 batch is fixed and covered by regressions
  (CBOR byte strings, RPC teardown, paged fallback/marker scanning, server
  detach). Remaining P2s: extension provider/MCP rollback on a failed install,
  MCP `ConnectionClosed` retry classification, server session-registry
  TTL/ownership, and the ConPTY `s33` resume race.
- SDK: raw `CreateAgentSessionOptions` max-turns default is 100 while
  `AgentBuilder` is 1000; proxy/registry overwrite loophole remains.
- context-mode: the embedded RustPython build has no `_ssl` (the
  `rustpython-stdlib` `ssl` feature would pull OpenSSL), so HTTPS from
  `ctx_execute` Python scripts fails; `ctx_fetch_and_index` is the supported
  HTTPS path and now honors the configured proxy.
- `session_context_cli` is Windows-only; a few load-sensitive CI suites are
  quarantined per OS (test gates only).
- `cargo-deny`/`cargo-audit` are local checks; PGO profile generation is not
  available on the local GNU target (`-Cprofile-use` works; CI MSVC/Linux is
  the production path).

## Commands

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --doc
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test_fast.ps1 -Concurrency 16 -Threads 4
CARGO_TARGET_DIR=target bash scripts/feature_matrix.sh
```

[`docs/FEATURES.md`](FEATURES.md) · [`MAPPING.md`](../MAPPING.md)
