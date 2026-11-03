# Keybindings

pir resolves keybindings from the settings `keybindings` map over the built-in registry (upstream's `tui.*`
namespace, 32 ids plus the `app.*` groups).

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

Values are chord strings (`ctrl+`, `alt+`, `shift+` prefixes; comma separates alternatives). Unknown ids are a
settings error; reserved ids (like `tui.input.submit`) cannot be rebound by extensions.

Id groups: `app.*` covers submit/newline, editor movement and kill-ring, message queue (enqueue/dequeue), model
selector, clipboard and page scrolling; `tui.*` covers selector navigation, autocomplete accept, theme handling,
alt-screen search/scroll and program status; `tui.altScreen.*` covers alt-screen cursor/scroll actions (hardcoded
defaults today; see MAPPING.md deviations). `/hotkeys` renders the full live table (registry rows overridden by
settings).
