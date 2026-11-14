# Prompt templates

Prompt templates are markdown files that become `/` commands. They are discovered from (in precedence order) the
`--prompt-template` CLI paths, the trusted project scope (settings `prompts` paths, then `<cwd>/.pi/prompts`), the
user scope (global settings `prompts` paths, then `<agentDir>/prompts`), and installed packages. Project scope is
trust-gated; `--no-prompt-templates` disables the settings, the automatic directories and the
installed-package prompts (an explicitly passed `--prompt-template` path still loads). Name collisions: first
wins with a diagnostic.

Relative settings entries resolve against `<cwd>/.pi` (project) or `<agentDir>` (global). The automatic
directories load direct `.md` children only; settings entries may name a file or a directory, and package filters
select nested markdown files.

## Format and usage

The body is the template; the file stem is its name, so `review.md` becomes `/review`. Front-matter
`description` appears in completion (default: the first non-empty line, truncated); `argument-hint` is optional.

`/review concurrency` expands the template before the text enters the agent; arguments follow shell-like quoting.
Supported substitutions: `$1`, `$2`, … (positional arguments), `$@` / `$ARGUMENTS` (all arguments),
`${1:-default}` / `${@:-default}` / `${ARGUMENTS:-default}` (value or default), `${@:N}` (arguments from position
N) and `${@:N:L}` (L arguments from position N). Run `/reload` after adding or changing a template in an active
session.

Templates never join the system prompt — editor-side expansions only (upstream parity). Skills (see
[skills.md](skills.md)) are the system-prompt counterpart.
