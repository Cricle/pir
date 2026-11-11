# Performance profile

`pir` has an opt-in performance profile. The default is **balanced**, which
keeps the historical tool loadout, unlimited tool-result text, compaction
thresholds, and turn limits unchanged.

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
