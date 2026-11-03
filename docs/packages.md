# Packages

`pir install <source>` installs an extension package and records it in settings; entries are consumed at every
startup (upstream `DefaultPackageManager.resolve()` + `collectPackageResources`).

## Sources

- `npm:<name>[@version]` — installed with the configured `npmCommand` into `<agentDir>/npm/node_modules/<name>`
  (project scope: `<cwd>/.pi/npm/...`).
- `git@host:path` / `https://host/path.git` / `git:host/path` — cloned into `<agentDir>/git/<host>/<path>`
  (project scope: `<cwd>/.pi/git/...`).
- A local directory path (recorded relative to the scope base dir).

## Scopes

User scope writes `~/.pi/agent/settings.json`; project scope writes `<cwd>/.pi/settings.json` (trust-gated).
Project entries win over global per source identity. `pir list` shows both; `pir remove <source>` drops the entry
and copy; `pir update [source]` re-clones/refreshes.

## What a package contributes

At startup each recorded package's installed directory is scanned for:

1. A `package.json` `"pi"` manifest:
   ```json
   { "pi": { "extensions": ["ext/*.ts"], "skills": ["skills/"], "prompts": ["prompts/"], "themes": ["theme.json"] } }
   ```
Paths/globs are relative to the package root (`*` per segment, `**` across).
2. Otherwise the convention directories: `extensions/`, `skills/`, `prompts/` (recursive `*.md`), `themes/`
   (recursive `*.json`).

Precedence: package-contributed resources rank below settings/auto paths and above builtins; CLI `-e`/`--skill`
paths still win. Missing installs are skipped with a log line (run `pir install`). Package extensions run under
QuickJS — packages requiring Node built-ins (`node:fs`, ...) cannot load and are reported as warnings, never
blocking startup.

## Virtual SDK modules

Packages importing the pi SDK resolve to built-in shims — no `node_modules` copy needed:

- `@earendil-works/pi-coding-agent`, `@earendil-works/pi-agent-core`, `@earendil-works/pi-tui`,
  `@earendil-works/pi-ai` — the SDK surface ported for the QuickJS runtime.
- `@earendil-works/pi-codemode` (subpaths `/declarations`, `/source`, `/worker`, `/portable`, `/cloudflare`) —
  declarations/renderers ported; sandbox `execute()`/`start()` route to the host's native QuickJS sandbox.
- `@earendil-works/pi-env` — the env client over the built-in protocol-v1 daemon (`pir env-daemon`):
  `Connection`, `RemoteExecutionEnv`, `RemoteWatcher`, terminal helpers, and SSH deploy (system `ssh`). Its
  `ok`/`err`/`FileError`/`ExecutionError`/`StreamDecoder` come from `@earendil-works/pi-durable/env`.
- `@earendil-works/pi-durable` — the durable SDK thin surface. **Real** subpaths: `/tools`
  (read/write/edit/bash/powershell factories), `/env` + `/env/node` (`ExecutionEnv`, `NodeExecutionEnv`),
  `/tool-search`, `/codemode` (tool/extension over the host sandbox), `/storage/memory`, `/storage/jsonl`
  (+`/node`), `/storage/sqlite` (+`/node`, on `node:sqlite`), and the root entry's real half
  (`defineTool`/`defineExtension`/`section`/`wrapTool`, `defineDoc`/`defineDocFamily`, entry/error values,
  `createSession`, `MemoryStorage`). **Throws** (`<api> is not supported by the pir extension runtime`,
  `ERR_NOT_SUPPORTED`): root `Harness`/`createRegistry`/`watchEvents`, `/images`, `/images/node`,
  `/images/cloudflare`, `/testing`, `/storage/sqlite/cloudflare`. Needs no `node_modules` of its own.

Runtime-unsupported faces of these packages follow the three-tier discipline: names exist, calls throw `<api> is
not supported by the pir extension runtime` with a `code`.
