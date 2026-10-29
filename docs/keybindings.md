# Keybindings

pir resolves keybindings from the settings `keybindings` map over the
built-in registry (upstream's `tui.*` id namespace, ~52 ids plus the
`app.*` groups).

## Settings shape

```json
{
  "keybindings": {
    "app.submit": "enter",
    "app.editor.external": "ctrl+g",
    "tui.selector.moveUp": "up,k"
  }
}
```

Values are chord strings (`ctrl+`, `alt+`, `shift+` prefixes; comma
separates alternatives). Unknown ids are a settings error; reserved ids
(like `tui.input.submit`) cannot be rebound by extensions.

## Id groups

- `app.*`: submit/newline, editor movement and kill-ring, message queue
  (enqueue/dequeue), model selector, clipboard, page scrolling.
- `tui.*`: selector navigation, autocomplete accept, theme handling,
  alt-screen search/scroll, program status.
- `tui.altScreen.*`: alt-screen cursor/scroll actions (hardcoded defaults
  today; see MAPPING.md deviations).

`/hotkeys` renders the full live table (registry rows overridden by
settings plus app-level hints). The registry ships with defaults matching
upstream; use `/keys` equivalents in the settings editor to inspect.
