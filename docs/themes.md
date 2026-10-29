# Themes

Custom themes are JSON files in `<agentDir>/themes` (and package
contributions, see [packages.md](packages.md)). Files are loaded at startup
before the theme resolves; invalid files are reported and skipped; name
collisions resolve first-wins.

## Shape

```json
{
  "name": "my-theme",
  "colors": {
    "accent": "oklch(0.72 0.13 180)",
    "error": "#ff5555",
    "toolTitle": "hsl(210 80% 70%)"
  }
}
```

Colors accept `oklch()`, `okhsl()`, hex, and plain names; unknown fields are
rejected (fail-closed, matching upstream's schema). The sixteen semantic
roles (accent, error, toolTitle, toolOutput, muted, ...) come from
upstream's theme schema.

## Selection and export

- `settings.theme` selects the theme at startup; OSC 11 auto-detect picks a
  dark/light default when the terminal reports its background (env-gated).
- Extensions can switch themes with `ctx.ui.setTheme(name)`.
- `/theme` renders the current theme; export blocks support copying the
  active palette.

The theme registry is global (one per process); extension `getAllThemes`
returns every registered entry with its source file.
