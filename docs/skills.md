# Skills

Skills are `SKILL.md` folders discovered from (in precedence order) `--skill` CLI paths, the trusted project scope
(settings `skills` paths, `<cwd>/.pi/skills`, then ancestor `.agents/skills` from the cwd outward through the
enclosing git repo root), the user scope (settings `skills` paths, `<agentDir>/skills`, then `~/.agents/skills`),
and installed packages. Project scope is trust-gated; user scope never. Name collisions: first wins with a
diagnostic.

## SKILL.md format

```markdown
---
name: release-notes
description: Summarize changes into release notes
---

Instructions for the model, markdown. Referenced files resolve relative to
this folder.
```

Front-matter `name`/`description` are required; validation failures are structured diagnostics (missing name, bad
front-matter) and the skill is skipped. Symlinked folders are followed.

## Usage

`<available_skills>` is injected into the system prompt (name + description + path), matching upstream's XML
block. `/skill:<name> [args]` expands the skill body into the prompt with args appended; the system-prompt section
and `/skill` autocomplete share the discovered set.
