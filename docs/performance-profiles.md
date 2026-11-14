# Performance profile

`pir` has an opt-in performance profile. The default is **balanced**, which
keeps the historical tool loadout, unlimited tool-result text, compaction
thresholds, and turn limits unchanged.

> **The balanced default tool loadout is a documented SUPERSET of upstream pi.**
> With no `--tools` / `settings.defaultTools`, a full build declares **22
> tools**: nine registration-active core tools (`background_abort`,
> `background_status`, `bash`, `edit`, `find`, `grep`, `ls`, `read`, `write`)
> plus the 13 default-active builtin-extension tools (`ctx_batch_execute`,
> `ctx_doctor`, `ctx_execute`, `ctx_execute_file`, `ctx_fetch_and_index`,
> `ctx_index`, `ctx_purge`, `ctx_search`, `ctx_stats`, `python`, `todo`,
> `memory`, `subagent`), where upstream pi's implicit set is four (`read`,
> `bash`, `edit`, `write`). The extra core five are pir capabilities
> (in-process grep/find, `ls`, the scheduler-owned background controls) and the
> builtins are the default-on extension layer; the measured declaration is
> ~+30.7 KB of `tools[]` JSON (~+7.7k tokens) on the first request, ~26 KB of
> it the builtins. `efficient` (below) and `--tools read,bash,edit,write` both
> select the upstream four; `--no-extensions` leaves the core nine; no tool
> becomes unreachable either way (the registry is never filtered by the
> selection). See [`docs/FEATURES.md`](FEATURES.md) § 3.1 and the 2026-11-14
> decision brief `.play/live-assist/x10-tool-loadout.md`.

```json
{
  "performance": {
    "profile": "efficient"
  }
}
```

`efficient` is intended for cost/latency-sensitive unattended runs and A/B
measurements. It selects these defaults (each field can be overridden):

| Setting | Efficient default |
|---|---:|
| `toolResultMaxBytes` | 32 KiB |
| `softTurnBudget` | 24 turns |
| `hardTurnBudget` | 48 turns |
| `maxTotalTokens` | 250,000 |
| `contextFilesMaxBytes` | 64 KiB |
| bash command timeout | 30s default |
| broad-search guard | **on by default in every profile**; reject `find /`, recursive grep/rg over `/tmp`/home, and `cd /tmp` + project search/test |

The guard is controlled by `tools.broadSearchGuard` (default `true`). Set it
to `false` only when a workflow intentionally needs whole-filesystem searches:

```json
{"tools":{"broadSearchGuard":false}}
```

In `efficient` mode the implicit tool selection is the core four tools
(`read`, `bash`, `edit`, `write`), matching upstream `pi`. Every other
registered tool remains available through an explicit `defaultTools` or
`--tools` selection; MCP direct tools keep their registration-activation
behavior. A tool result longer than `toolResultMaxBytes` is shortened with a
head/tail marker before it reaches the model; the full payload remains in the
session/UI data.

The soft turn budget only injects a transient convergence instruction into the
remaining requests of the current run. It is never added to the session
transcript, message events, or resumed history. The hard turn budget and
cumulative token budget terminate an incomplete run with an identifiable
`StopReason::Error`, so a truncated run is never reported as a success.

Use the CLI flag or the process-local switch when a settings file is not
desired:

```bash
pir --perf-profile efficient --mode json --print ...
PIR_PERF_PROFILE=efficient pir --mode json --print ...
```
