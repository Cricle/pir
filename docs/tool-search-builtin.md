# `tool-search` builtin — BM25 deferred-tool discovery

`builtin:tool-search` is the native Rust port of upstream `extensions/tool-search/{index,tool}.ts`.
It registers the model-facing `tool_search` tool: a BM25 search over **not-yet-active**
`codemode` / `deferred` tools that loads matches into the session's active set.

- Implementation: `src/plugins/builtin/tool_search.rs`; feature `tool-search`; entry
  `src/plugins/builtin.rs` (`name: "tool-search"`, `replaceable: true`, default-enabled).
- Upstream contract: `toolSearchSchema` (`tool.ts:159-164`); behavior truth is the upstream
  extension source. Tests: module unit tests; `tests/suites/plugins_w11.rs` (`tool_search_next_request`).

## 1. Tool contract

One tool, `tool_search`:

| parameter | type | semantics |
| --- | --- | --- |
| `query` | string, required | Search query. Trimmed; empty/whitespace-only → `query must not be empty`. |
| `limit` | number, optional | Maximum matches. Default `8` (`DEFAULT_TOOL_SEARCH_LIMIT`). Must be a finite integer `> 0`; `0`, negatives, fractions, strings and `null` handling: `null` = default, other invalid values → `limit must be a positive integer`. |

Exposure is `ModelOnly` with `default_active: false`, so the tool exists in the registry but is
not declared unless selected (the MCP `deferred` reach layer auto-activates it) or loaded by
`--tools`/`tool_search` itself.

Success result is upstream-shaped text plus details:

```json
{
  "content": "Loaded 2 tools. They are available from your next call:\n- name: first description line\n...",
  "details": { "loaded": ["name", "..."] },
  "isError": false
}
```

No candidates or no BM25 match returns `No matching tools found.` with `details.loaded = []`
and `isError: false` (not an error). Infrastructure errors (no session environment, no live
registry) return `isError: true`.

## 2. Activation and default plan

- Candidates: tools with `Codemode` or `Deferred` exposure that are not in the current effective
  active set and are not excluded by `--exclude-tools` / a `-name` entry. Candidate order is sorted
  by tool name before ranking, so equal BM25 scores are deterministic.
- The result is written back as `baseline ∪ matches` into `ToolSessionEnv::active_override` under
  one write guard (same-batch searches serialize). The session rebuilds its loadout from that cell
  and `agent_loop` refreshes `context.tools` before every provider request, so matches are
  callable on the very next request of the same run.
- Precedence of the two active-set stores: native cell → JS `setActiveTools` override → host
  selection. A JS override that pruned the run registry is reconciled to the JS set before search,
  so the native cell does not mask it.
- Default plan: enabled (`default_enabled: true`). Disable with
  `disabledBuiltinExtensions: ["tool-search"]`; in a build without the `tool-search` feature the
  tool is absent and `builtin:tool-search` reports the unknown/not-compiled builtin error.

## 3. Events

None. The plugin declares no `events()` and consumes no host notifications; activation is
synchronous inside the tool call. The downstream `tool_call` / `tool_result` events are ordinary
host events for whatever tool is called next, not emitted by this plugin.

## 4. Error paths

| condition | result |
| --- | --- |
| no `ToolSessionEnv` behind the tool (non-session host) | `isError: true`, `tool_search requires a session environment` |
| `query` missing/empty | `isError: true`, `query must not be empty` |
| `limit` non-integer / `<= 0` | `isError: true`, `limit must be a positive integer` |
| no live registry | `isError: true`, `tool_search found no live tool registry` |
| no searchable candidates / zero matches | success, empty `loaded` |

## 5. Boundaries / deviations

- Hosts that drive `agent_loop` without an `AgentSession` (print mode builds `ToolSessionEnv`
  directly) have no resolved active set to read; the exposure defaults are the only baseline.
- Writing the union to the native cell still shadows a later JS `setActiveTools`; this is the
  documented two-store trade-off. Upstream writes its single store.
- The read-modify-write union lives in the tool rather than a separate ranker store; language-port
  shape only, behavior matches upstream `searchAndLoad` for the observable contract.
