# Keybindings

pir resolves keybindings from `<agentDir>/keybindings.json` plus the settings `keybindings` map (settings win)
over the built-in registry (upstream's `tui.*` namespace, 32 ids plus the `app.*` groups).

## Settings shape

```json
{
  "keybindings": {
    "tui.input.submit": "enter",
    "app.editor.external": "ctrl+g",
    "tui.select.up": ["up", "k"]
  }
}
```

Values are a chord string or an array of chord strings (`ctrl+`/`alt+`/`shift+` prefixes; the settings schema
accepts `string | string[]`). Unknown ids are silently ignored (upstream behavior); an explicit empty list (`[]`)
unbinds an id's default. Extensions register extra chords with `pi.registerShortcut` — those take precedence
over built-in actions — but the registry ids themselves are not extension-rebindable (the pi-tui shim's
`setKeybindings` only drives that shim's own components).

Id groups: `tui.*` covers submit/newline and tab, editor movement and kill-ring, selector navigation,
autocomplete accept and the alt-screen search/scroll family; `app.*` covers interrupt/clear/exit, thinking,
model cycling/selection, message follow-up/dequeue/copy, clipboard paste, the external editor, and the
`models.*`/`tree.*`/`session.*` families. `tui.altScreen.*` ids resolve like the rest (settings override →
platform-resolved default → the upstream-verbatim table in `src/tui/altscreen_search.rs`). `/hotkeys` renders
the full live table (registry rows overridden by settings).
