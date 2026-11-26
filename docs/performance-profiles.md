# Performance profile

`pir` has an opt-in performance profile. The default is **balanced**, which
keeps the historical tool loadout, unlimited tool-result text, compaction
thresholds, and turn limits unchanged.

> **The balanced default tool loadout is a documented SUPERSET of upstream pi.**
> With no `--tools` / `settings.defaultTools`, a full build declares **24
> tools** (nine registration-active core tools plus 15 default-active builtin
> tools), where upstream pi's implicit set is four (`read`, `bash`, `edit`,
> `write`). The extra five core tools are pir capabilities (in-process
> grep/find, `ls`, the scheduler-owned background controls) and the builtins
> are the default-on extension layer; the declaration costs ~+33 KB of
> `tools[]` JSON (~+8.3k tokens) on the first request.
> [`FEATURES.md`](FEATURES.md) §3.1 is the canonical list, rationale and
> measurement. `efficient` (below) and `--tools read,bash,edit,write` both
> select the upstream four; `--no-extensions` leaves the core nine; no tool
> becomes unreachable either way (the registry is never filtered by the
> selection).

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
| `toolResultMaxBytes` | 24 KiB |
| `read`/`bash` output cap | 1,000 lines / 24 KiB — applied by `--perf-profile efficient` or `PIR_PERF_PROFILE=efficient`; a settings-file `performance.profile` does not reach these caps (`PIR_TOOL_OUTPUT_MAX_*` overrides) |
| `softTurnBudget` | 24 turns |
| `hardTurnBudget` | 48 turns |
| `maxTotalTokens` | 250,000 |
| `subagentMaxTurns` | 100 |
| `subagentMaxTotalTokens` | 100,000 |
| `contextFilesMaxBytes` | 64 KiB |
| bash command timeout | 30s default |
| broad-search guard | **on by default in every profile**; reject `find /`, recursive grep/rg over `/tmp`/home, and `cd /tmp` + project search/test |

The profile also changes the four pir-only `<rules>` bullets: `efficient` keeps
the three that mirror hard gates (read-before-edit, literal paths,
broad-search) and omits only the advisory final self-check
("Before finishing, re-read the task specification…"). That bullet has no
matching tool/agent gate and upstream `buildRules` has no equivalent. `balanced`
keeps all four. The omission is implemented as prompt rule options, not as a
fixture-specific or tool-specific exception; the read gate itself stays active
(both profiles use `EditPartial`: a partial read may edit with a warning, while
a never-read or stale file is still refused and `write` still requires a full,
unchanged read). Set `PIR_READ_BEFORE_EDIT=edit-warn` to also allow a stale
edit with a warning, or `enforce` for the old strict behavior.

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

`subagentMaxTurns` / `subagentMaxTotalTokens` bound every child run the
`subagent` tool starts (single, chain, parallel, inline and process executors).
They do not replace the per-run budgets: a child stops at whichever limit is
lower, and a subagent budget stop is reported as `Subagent token budget
exceeded` / a turn-cap error instead of a successful result. `PIR_SUBAGENT_MAX_TURNS`
and `PIR_SUBAGENT_MAX_TOTAL_TOKENS` override the settings values; `0` removes
the corresponding cap.

Use the CLI flag or the process-local switch when a settings file is not
desired:

```bash
pir --perf-profile efficient --mode json --print ...
PIR_PERF_PROFILE=efficient pir --mode json --print ...
```
