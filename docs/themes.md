# Themes

Custom themes are JSON files in `<agentDir>/themes` (and package contributions, see [packages.md](packages.md)).
Files load at startup before theme resolution; invalid files are reported and skipped; collisions resolve
first-wins.

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

Colors accept `oklch()`, `okhsl()`, hex, and plain names; unknown fields are rejected (fail-closed, matching
upstream). The sixteen semantic roles (accent, error, toolTitle, toolOutput, muted, ...) come from upstream's
schema.

Selection: `settings.theme` selects the theme at startup; OSC 11 auto-detect picks a dark/light default when the
terminal reports its background.
- Extensions switch themes with `ctx.ui.setTheme(name)`; `/theme` renders the current theme and export blocks copy
  the palette. The theme registry is global; extension `getAllThemes` returns every entry with its source file.
