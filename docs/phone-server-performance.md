# pi-phone server performance

`pir-phone-server` is a metadata/relay service. It does not persist or cache
transcripts, snapshots, tool output, or images. The only in-memory status cache
is the latest small `server/status` metadata frame per live session.

## Environment

- WSL2 Debian, Linux x86_64, Rust 1.99.1
- `--release` build
- Server and load harness in one process; loopback TCP, no TLS/proxy
- 10,000 sessions / 100 phone clients / 500 live events for the fan-out test
- 1,000,000 sessions for the catalog/memory test
- RSS from `/proc/self/status` (`VmRSS`/`VmHWM`); because the harness shares
  the process, values include the load generator and are upper bounds

Run:

```bash
bash scripts/phone_server_load.sh \
  --sessions 10000 --clients 100 --events 500 --iterations 20

bash scripts/phone_server_load.sh \
  --sessions 1000000 --clients 10 --events 50 --iterations 20
```

## 10,000 sessions / 100 clients

| metric | result |
|---|---:|
| catalog p50 / p95 / p99 | **0.40 / 0.64 / 0.85 ms** |
| fan-out, 100 clients × 500 events | **50,000 msgs in 56.2 ms** |
| fan-out throughput | **~889k msgs/s** |
| reconnect + agent resync p50 / p95 / p99 | **2.52 / 3.28 / 3.28 ms** |
| RSS start / seeded / fan-out | **3.3 / 27.7 / 53.3 MiB** |
| peak RSS | **53.3 MiB** |
| status cache / transcript cache | **0 B / 0 B** |
| process CPU for full run | **0.41 s** |

## 1,000,000 sessions

| metric | result |
|---|---:|
| catalog p50 / p95 / p99 | **0.42 / 0.45 / 0.47 ms** |
| RSS start / seeded / fan-out | **3.3 / 608.8 / 617.1 MiB** |
| peak RSS | **617.1 MiB** |
| process CPU for seed + test | **11.2 s** |
| per-session RSS (seeded) | **~0.61 MiB / 1k sessions** |

The 1M run stays below 1 GiB. It seeds metadata in 10k-row chunks; no
transcript is involved. A 1M-live-session workload is a different problem:
this result covers a 1M-row metadata catalog plus a live relay subset.

Legacy scenario harness (`scripts/phone_server_bench.sh --iterations 5`):

| scenario | p50 | p95 |
|---|---:|---:|
| 10k sessions, first 50-row catalog page | 0.04 ms | 2.61 ms |
| 50 phones × 100 live events | 47.6 ms | 48.0 ms |
| 400 KiB snapshot delivery | 1.52 ms | 2.15 ms |
| client reconnect + agent resync | 3.45 ms | 4.05 ms |

## What was optimized

- Compact `bincode` metadata rows keep 1M sessions at ~0.6 GiB RSS instead of
  the JSON-row baseline near 1 GiB.
- Broadcasts serialize one `Arc<str>` line and clone the `Arc` per subscriber;
  no per-receiver `String` copy.
- Only the latest status metadata line is cached. Snapshot/transcript frames
  are forwarded and dropped, then a reconnecting client triggers an agent
  `resync`.
- Status frames are diffed against the live descriptor. Unchanged frames do
  not write redb or fan out a catalog.
- The unfiltered catalog walks the redb updated-at B-tree in reverse with a
  live-row offset, so soft-deleted rows cannot create page gaps.
- `/api/health` uses redb table length and registry counters (`O(1)`); it does
  not scan 1M sessions.
- Static assets use `Bytes::from_static`; gzip is enabled for HTTP responses.
- `/api/metrics` exposes RSS/VmHWM, relay counters, cache bytes and registry
  counts.

## Interpretation

The useful regression signals are catalog p95/p99, fan-out throughput,
reconnect p95, RSS after seeding, and RSS delta during fan-out. The 1M catalog
result is metadata-only; active live sessions and their subscription maps have
separate memory costs. Fan-out numbers are in-process loopback numbers and are
not a cellular-network estimate.
