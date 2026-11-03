# TUI

pir's interactive TUI is a ratatui/crossterm port of upstream's pi-tui components: multiline editor with
kill-ring/undo, streaming transcript (markdown + diff + tool renderers), dialogs, selectors, autocomplete, and
alt-screen mode.

## Layout

- **Editor**: multiline input, `!` prefix enters bash mode, kill-ring (ctrl+u/ctrl+k/ctrl+y, alt+y rotation), undo,
  `@file` autocomplete for paths, `/` autocomplete for commands, history (`up`/`down`), page scrolling.
- **Transcript**: assistant messages stream with markdown rendering; tool calls render as cards (title line + live
  output for shell tools + status tail `(exit N)` / `Took Xs`); compaction and notices render as bordered entries.
- **Alt-screen** (`/altscreen` or settings): scrollable viewport with search (`/`), selection copy, and a
  scrollbar; upstream scroll geometry (maxScrollTop clamps).

## Mouse & selection

Mouse capture is enabled only in fullscreen alt-screen mode (upstream `EnableMouseCapture`: button + motion
tracking, SGR encoding, `?1000h/?1002h/?1003h/?1006h`); regular mode uses the terminal scrollback.

On Windows the console can drop the record carrying a mouse report's `ESC`, so `[<Cb;Cx;CyM|m` arrives as per-byte
key records. `StdinGate::claim_console_mouse` (default on Windows) holds plausible prefixes for the 50 ms sequence
window, ignores the paired key-up record, and emits `Event::Mouse`; a run that cannot become a report (`[x`, a
terminating byte before the third parameter, a shape outside the button grammar, window expiry) is released as
typed input, so no byte is swallowed or invented. Only Windows pays. Locked by
`scripts/tui_conpty/scenarios/s34_console_mouse_no_typing.py`.

What works:

- **Wheel** scrolls the transcript viewport: one notch moves `fullscreenWheelScrollLines` (`"auto"` accelerates a
  fast spin up to six lines, a number is fixed, clamped 1..=100); Alt+wheel moves 5×. A wheel over the dock still
  scrolls the transcript (upstream primary-scrollable fallback); with a dialog/selector overlay focused only the
  wheel is routed.
- **Drag selection**: press left and drag; selection copies on release (`fullscreenCopyOnSelect`, default on) with
  a `Copied!`/`Copy failed` flash. Double-click has no special behavior. Selection covers everything painted —
  transcript and dock rows (editor, footer, open dialog/selector) — via `TuiCore::screen_row_to_buffer_row`.
  Copied text matches on-screen cells: ANSI styling stripped, columns counted over grapheme clusters, wide CJK
  cells kept/dropped whole.
- **Scrollbar** (`fullscreenScrollbar`): `"always"` reserves the transcript's last column and paints track + thumb;
  `"hidden"` never paints; `"auto"` (default) shows the transient bar when the transcript overflows *and* a scroll
  gesture happened in the last second (1000 ms fade). The thumb moves with the viewport (wheel, page keys, search
  jumps).
- **Search** (ctrl+f) highlights matches; the box renders at the top of the transcript. Windowed search caches the
  ANSI-stripped corpus per row keyed by content, so only changed rows are re-parsed and an unchanged frame does no
  corpus work (`SearchFrameCache` in `altscreen_search.rs`).

Typing a complete `[<Cb;Cx;CyM` shape is claimed as mouse (the price of ESC-less support); the ESC-prefixed
spelling (`PIR_TUI_VT_INPUT=1`, Unix terminals) keeps the byte-decoder path.

Mouse behaviors beyond wheel/selection:

- **Scrollbar scrubbing**: a left press on a visible track grabs it, jumps to the pointer, scrubs on move and ends
  on release; overlay focus blocks the family (wheel excepted); a press on a hidden `"auto"` bar reveals it and
  falls through to selection.
- **Hover-active thumb**: `█` is hover-only; hover keeps `"auto"` alive without a timer; leaving starts a fresh
  linger.
- **"↓ Jump to latest message"**: painted on the clip's last row when scrolled away or pinned by search; a click
  returns to the bottom and resumes follow.
- **Search box**: three-row top-right overlay with hover-underlined clickable prev/next (`↑ Shift+Enter · ↓
  Enter`, bare arrows when narrow).
- **Windows right-click paste**: a bare secondary press outside VS Code returns `AltMouseRoute::PasteClipboard`;
  the clipboard is fed through the gate as one bracketed paste into the focused component.

The `tui.altScreen.*` chords (`bottom: ctrl+end`, `top: ctrl+home`, `searchNext: enter|ctrl+g`, `searchPrevious:
shift+enter|ctrl+shift+g`, `searchClose: escape`) live in `ALT_SCREEN_KEYBINDINGS` in
`src/tui/altscreen_search.rs`, upstream-verbatim; label rendering (`key_display_text`) resolves config layer →
that table → the `tui` registry, so settings-registered ids win.

**Legacy X10 reports** (`ESC[M` + 3 bytes) are decoded like upstream's `parse_csi_normal_mouse` and fed through
`decode_input_sequence`. The **ESC-less** X10 tail is deliberately not claimed: `[M` + 3 bytes is indistinguishable
from typing, and the Windows console only drops the ESC whose loss matters in SGR mode.

**OSC-8 hyperlinks** are clickable: `osc8_link_at_column` finds the link at a column; the URL is captured on left
press, cleared by any drag, and opened only for a click (release cell == press cell; no modifier gating), consuming
the gesture (`AltMouseRoute::OpenUrl` → `open_browser`, the helper the MCP OAuth flow uses). The markdown
renderer emits the run when `global_hyperlinks()` is on (pinned by `terminal.hyperlinks` at TUI start), else `text
(url)`; wrapped links stay clickable and a link clipped mid-run closes before the ellipsis.

Still different from upstream: in the windowed transcript (huge sessions) the band stays bounded, so search needing
older history runs a **bounded backward walk** — probing resident unmaterialized entries, then paging older
history off disk (the `LoadOlder` read scrolling up uses) until it finds a matching page, reaches session start, or
hits its bound (16 pages / 4 MiB). The box shows `…` while a walk is live; "No matches" only when the whole
session was searched, "No match so far" when the bound stopped it; `n/m` counts matches brought into the band. A
new keystroke supersedes the walk, Esc aborts it, and a late page reply still loads as ordinary history but cannot
complete the walk (`altscreen_search::SearchWalk`). Prompt navigation stays over the materialized band; on Windows
a late reply-shaped run (`[?61;…c`) still reaches the editor (`stdin_buffer` docs).

## Tool cards

Each tool call renders via its renderer: `bash`/`powershell` (`$ cmd` / `PS> cmd`), `read` (path + line range),
`edit`/`write` (path + diff), `grep`, `find`, `ls`, MCP (`mcp__server__tool`), and extension renderers. Shell
output streams live (throttled rolling snapshots, upstream `emitOutputUpdate`). A background call (`background:
true` on any tool) keeps its card running past the turn; the status line carries `bg: N` and the card flips to real
output when the call settles.

Extension renderer, widget, footer and header factory callbacks run on the TUI main thread with async `ctx.ui`
suppressed (`has_ui=false`): `ctx.ui.select/confirm/input/editor` resolve `undefined` immediately instead of
queueing a dialog whose only drainer is the blocked renderer thread. Synchronous UI (`notify`, `setStatus`,
`setTitle`, editor text) keeps working.

## Modes

- Steering: send messages while a run executes (queued, applied between tool batches).
- Esc aborts the current run; `/quit` exits; ctrl+c aborts (double-press exits when idle). Esc does not cancel
  background tool calls — they end with the session (switch or exit).
- `/resume`, `/tree`, `/fork` (defaults to newest user message), `/export` (HTML), `/session` (usage/cost).
  `/resume` lists the session directory once and filters the current cwd in memory (Tab switches to full list); it
  does not rescan files per scope.
- Session tabs: `/sessions` opens a picker of open tabs and the cwd's persisted sessions; `/sessions
  new|next|prev|close|<index>|<path>` acts directly; ctrl+tab / ctrl+shift+tab cycle tabs. Only the active tab's
  session is live in the worker (switching reuses the `/resume` attach); a fresh tab's stem title becomes the first
  prompt's preview on its first run.
- Paged (huge) sessions: `/tree` and `/fork` list the whole history without widening — the skipped prefix streams
  one entry at a time and the transcript never becomes resident. The walk lives in
  `SessionManager::for_each_tree_row`; `tree_entries_streamed` maps row facts
  (`kind_hint`/`preview`/`tool_name`/labels) to the picker. Acting on a prefix row loads the file at that point;
  the session stays paged until a choice needs an entry.

## Terminal integration

OSC 11 background auto-detect for theme defaults; OSC 7501 program status (opt-in, `PI_PROGRAM_STATUS=1`);
bracketed paste; optional hardware cursor (`PI_HARDWARE_CURSOR`).

The footer's git branch comes from `.git/HEAD` read directly — polled once per second, walking parent directories
and following `gitdir:` indirection used by worktrees/submodules. `git rev-parse --abbrev-ref HEAD` is a one-shot
fallback only when the direct read cannot answer (bare setups, unusual `GIT_DIR` layouts).

`terminal.hyperlinks` and `terminal.trueColor` are pinned at TUI start (`set_global_hyperlinks` +
`set_color_mode_override`). pir has no settings-reload hook yet, so a later change applies on the next TUI start.
