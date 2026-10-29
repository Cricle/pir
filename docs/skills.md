# Skills

Skills are `SKILL.md` folders discovered from (in precedence order) the
`--skill` CLI paths, the trusted project scope (its settings `skills` paths,
`<cwd>/.pi/skills`, then the ancestor `.agents/skills` directories from the
cwd outward, up to and including the enclosing git repository root), the user
scope (its settings `skills` paths, `<agentDir>/skills`, then
`~/.agents/skills`), and installed packages. The project scope is
trust-gated, the user scope never is. Name collisions: first
(highest-precedence) wins, with a diagnostic.

## SKILL.md format

```markdown
---
name: release-notes
description: Summarize changes into release notes
---

Instructions for the model, markdown. Referenced files resolve relative to
this folder.
```

Front-matter `name`/`description` are required; validation failures are
reported as structured diagnostics (missing name, bad front-matter) and the
skill is skipped. Symlinked folders are followed.

## Usage

- `<available_skills>` is injected into the system prompt (name +
  description + path), matching upstream's XML block.
- `/skill:<name> [args]` expands the skill body into the prompt with the
  args appended.
- The system-prompt skills section and the `/skill` autocomplete share the
  same discovered set.
