# Prompt templates

Prompt templates are markdown files discovered from (in precedence order) the settings `promptTemplates` paths,
`<agentDir>/prompt-templates`, `<cwd>/.pi/prompt-templates` (trust-gated), installed packages, and
`--prompt-template` CLI paths. Name collisions: first wins with a diagnostic.

## Format and usage

The body is the template; the file stem is its name. `/template:<name>` inserts the body into the editor at the
cursor; `/templates` lists discovered templates with source paths.

Templates never join the system prompt — editor-side expansions only (upstream parity). Skills (see
[skills.md](skills.md)) are the system-prompt counterpart.
