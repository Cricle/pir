# Prompt templates

Prompt templates are markdown files discovered from (in precedence order)
the settings `promptTemplates` paths, `<agentDir>/prompt-templates`,
`<cwd>/.pi/prompt-templates` (trust-gated), installed packages, and
`--prompt-template` CLI paths. Name collisions: first wins with a
diagnostic.

## Format

A markdown file whose body is the template. The file stem (without
extension) is the template name.

## Usage

`/template:<name>` inserts the template body into the editor at the cursor.
`/templates` lists the discovered templates with their source paths.

Templates never join the system prompt — they are editor-side expansions
only (upstream parity). Skills (see [skills.md](skills.md)) are the
system-prompt counterpart.
