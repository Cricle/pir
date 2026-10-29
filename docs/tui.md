# TUI

pir's interactive TUI is a ratatui/crossterm port of upstream's pi-tui
components: a multiline editor with a kill-ring/undo, a streaming transcript
(markdown + diff + tool renderers), dialogs, selectors, autocomplete, and an
alt-screen mode.

## Layout

- **Editor**: multiline input, `!` prefix enters bash mode, kill-ring
  (ctrl+u/ctrl+k/ctrl+y, alt+y rotation), undo, `@file` autocomplete for
  paths, `/` autocomplete for commands, history (`up`/`down`), page
  scrolling.
- **Transcript**: assistant messages stream with markdown rendering; tool
  calls render as cards (title line + live output for shell tools + status
  tail `(exit N)` / `Took Xs`); compaction and notices render as bordered
  entries.
- **Alt-screen** (`/altscreen` or settings): the transcript becomes a
  scrollable viewport with search (`/`), selection copy, and a scrollbar;
  upstream scroll geometry (maxScrollTop clamps).

## Mouse & selection

Mouse capture is enabled only in the fullscreen alt-screen mode (upstream
`EnableMouseCapture`: button + motion tracking with SGR encoding,
`?1000h/?1002h/?1003h/?1006h`); in regular mode the terminal's own scrollback
handles the wheel.

On Windows the console can drop the record that carries a mouse report's `ESC`,
so the remaining bytes (`[<Cb;Cx;CyM|m`) arrive as per-byte key records. The
stdin gate claims that tail (`StdinGate::claim_console_mouse`, on by default on
Windows): it holds every plausible prefix for the 50 ms sequence window, ignores
the paired key-up record, and emits the same `Event::Mouse`. A run that cannot
become a report (`[x`, a terminating byte before the third parameter, a shape
outside the button grammar, a window expiry) is released as typed input, so the
claim never swallows or invents a byte; only the Windows path pays for it. The
window is the sequence window (not the 10 ms escape window) because the console
enqueues a report in ~4 ms but the records reach the gate one poll at a time —
a `[` held only 10 ms was flushed as typing with the rest following into the
editor (locked by `scripts/tui_conpty/scenarios/s34_console_mouse_no_typing.py`).

What works:

- **Wheel** scrolls the transcript viewport (not keys sent to the terminal):
  one notch moves `fullscreenWheelScrollLines` (`"auto"` accelerates a fast
  spin up to six lines, a number is the fixed count, clamped 1..=100); Alt+
  wheel moves 5×. A wheel over the dock still scrolls the transcript
  (upstream's primary-scrollable fallback); while a dialog/selector overlay
  owns the focus only the wheel is routed, selection gestures stay blocked.
- **Drag selection**: press the left button and drag; the selection is copied
  on release (`fullscreenCopyOnSelect`, default on) with a `Copied!`/`Copy
  failed` flash. Double-click has no special behavior. The selection covers
  *everything painted*: the transcript **and** the dock rows — the editor,
  the footer and an open dialog/selector — because the mapping
  (`TuiCore::screen_row_to_buffer_row`) resolves each screen row through the
  frame's painted layout (scrolled window + dock) into the undecorated line
  buffer. Copied text matches the cells on screen: ANSI styling is stripped,
  columns are counted over grapheme clusters and wide CJK cells are kept or
  dropped whole.
- **Scrollbar** (`fullscreenScrollbar`): `"always"` reserves the transcript's
  last column and paints track + thumb whenever the viewport has rows;
  `"hidden"` never paints; `"auto"` (default) shows the transient bar when the
  transcript overflows *and* a scroll gesture happened in the last second
  (upstream `isScrollbarVisible` + `markScrollbarActivity`, 1000 ms fade). The
  thumb moves with the viewport (wheel, page keys, search jumps).
- **Search** (ctrl+f) highlights matches and reveals them; the search box
  renders at the top of the transcript. The windowed search caches the
  ANSI-stripped corpus per row keyed by the row's content: only rows whose
  rendered line changed are stripped and re-parsed, and an unchanged frame
  does zero corpus work (`SearchFrameCache` in `altscreen_search.rs`).

Two notes on that claim, beyond the paragraph above: a user *typing* a complete
`[<Cb;Cx;CyM` shape is claimed as a mouse event (the price of recognizing the
ESC-less tail), and the ESC-prefixed spelling (`PIR_TUI_VT_INPUT=1`, Unix
terminals) keeps the pre-existing byte-decoder path unchanged.

Mouse behaviors beyond wheel/selection, each anchored to upstream:

- **Scrollbar scrubbing** (`handleScrollbarMouseEvent`): a left press on a
  visible bar's track grabs it, jumps to the pointer, scrubs on move and ends
  on release; overlay focus blocks the family (wheel excepted); a press on a
  hidden `"auto"` bar reveals it and falls through to selection.
- **Hover-active thumb**: the `█` glyph is hover-only, hover keeps the `"auto"`
  bar alive without a timer, leaving starts a fresh linger.
- **"↓ Jump to latest message"**: painted on the clip's last row when scrolled
  away or pinned by search; a click returns to the bottom and resumes follow.
- **Search box**: upstream's three-row top-right overlay with hover-underlined
  clickable prev/next (`↑ Shift+Enter · ↓ Enter`, bare arrows when narrow).
- **Windows right-click paste**: a bare secondary press outside VS Code returns
  `AltMouseRoute::PasteClipboard`; the clipboard is fed through the gate as one
  bracketed paste, landing in the focused component.

The `tui.altScreen.*` chords (`bottom: ctrl+end`, `top: ctrl+home`,
`searchNext: enter|ctrl+g`, `searchPrevious: shift+enter|ctrl+shift+g`,
`searchClose: escape`) live in `ALT_SCREEN_KEYBINDINGS` inside
`src/tui/altscreen_search.rs`, upstream-verbatim: the `tui` registry deliberately
excludes the viewport family (an app-layer concern in pir), and label rendering
(`key_display_text`) resolves config layer → that table → the `tui` registry, so
settings-registered ids still win.

**Legacy X10 reports** (`ESC[M` + 3 bytes) are decoded where upstream decodes them
(`decode_x10_mouse`, a port of crossterm's `parse_csi_normal_mouse`: `Cb = byte −
32`, coordinates `byte − 33`, the shared button/modifier/drag bits, `Cb == 3` as
the release marker, out-of-range payload bytes rejected) and fed through the same
`decode_input_sequence` path, which previously completed such a run and then
dropped it. The **ESC-less** X10 tail is deliberately not claimed: `[M` + 3 bytes
is byte-for-byte indistinguishable from typing, and the Windows console only drops
the ESC whose loss matters in SGR mode (which is what the `[<…` claim exists for).

**OSC-8 hyperlinks** are detected and clickable: `osc8_link_at_column` walks a
rendered line with upstream's `getOsc8LinkAtColumn`/`ansiCodeLength` semantics, the
URL is captured on the left press, cleared by any drag, and opened only for a click
(release cell == press cell; no modifier gating — upstream masks
`event.button & 3`), consuming the gesture exactly like upstream's early return
(`AltMouseRoute::OpenUrl` → `open_browser`, the same `open-browser.ts` helper the
MCP OAuth flow uses). The Rust markdown renderer emits the same run when the
gate is on: `emit_styled_link` wraps the styled text in `osc8_hyperlink` when
`global_hyperlinks()` (the detected capability, pinned by `terminal.hyperlinks`
at TUI start), and falls back to `text (url)` when it is off. Wrapped links stay
clickable, and a link clipped mid-run closes before the ellipsis (upstream
`finalizeTruncatedResult`).

Still different from upstream (honest list): in the windowed transcript (huge
sessions) the band stays bounded, so search that needs older history runs a
**bounded backward walk** — a query with no match in the materialized band
probes the resident entries the window has not materialized, then pages older
history off disk (the same `LoadOlder` read scrolling up uses) until it finds
the newest page holding a match, reaches the session start, or hits its bound
(16 pages / 4 MiB). The box shows `…` for as long as a walk is live — probing the
resident entries, waiting on a page, and completing the reveal all read `…`
(`altscreen_search::result_text`), "No matches"
only when the whole session was searched, and "No match so far" when the bound
stopped the walk (older history unsearched); `n/m` counts exactly the matches
of what the walk brought into the band. A new keystroke supersedes the walk,
Esc aborts it, and a page reply that arrives after either still loads as
ordinary history but can never complete the live walk — see
`altscreen_search::SearchWalk`. Prompt navigation still operates over the
materialized band; and on Windows a late reply-shaped run
(`[?61;…c`) still reaches the editor by design (see `stdin_buffer`'s module
docs).

## Tool cards

Each tool call renders via its renderer: `bash`/`powershell` (`$ cmd` /
`PS> cmd`), `read` (path + line range), `edit`/`write` (path + diff),
`grep`, `find`, `ls`, MCP (`mcp__server__tool`), and extension tools via
their registered renderers. Output streams live for shell tools
(throttled rolling snapshots, upstream `emitOutputUpdate`). A background
call (`background: true` on any tool) keeps its card running past the turn;
the working status line carries a `bg: N` count and the card flips to the
real output in place when the call settles.

Extension renderer, widget, footer and header factory callbacks run on the
TUI main thread with the runtime's async `ctx.ui` suppressed for the
callback's duration (`has_ui=false`): `ctx.ui.select/confirm/input/editor`
resolve `undefined` immediately instead of queueing a dialog whose only
drainer is the blocked renderer thread. Synchronous UI (`notify`,
`setStatus`, `setTitle`, editor text) keeps working.

## Modes

- Steering: send messages while a run executes (queued, applied between
  tool batches).
- Esc aborts the current run; `/quit` exits; ctrl+c aborts (double-press
  exits when idle). Esc deliberately does not cancel background tool calls
  — they end with the session (switch or exit).
- `/resume`, `/tree`, `/fork` (defaults to the newest user message),
  `/export` (HTML), `/session` (usage/cost). `/resume` lists the session
  directory once and filters the current cwd in memory (Tab switches to the
  full list); it does not rescan the files per scope.
- Session tabs: `/sessions` opens a picker of the open tabs and the cwd's
  persisted sessions; `/sessions new|next|prev|close|<index>|<path>` acts
  directly, and ctrl+tab / ctrl+shift+tab cycle the open tabs. Only the active
  tab's session is live in the worker (switching reuses the `/resume` attach);
  a fresh tab's stem title becomes the first prompt's preview on its first run.
- Paged (huge) sessions: `/tree` and `/fork` list the whole history without
  widening — the skipped prefix is streamed one entry at a time and the
  transcript never becomes resident. The `/tree` walk (including the prefix
  streaming) lives in `SessionManager::for_each_tree_row`: it is whole-session
  for both managers, and the TUI's `tree_entries_streamed` only maps each row's
  facts (`kind_hint`/`preview`/`tool_name`/labels) to the picker's shape.
  Acting on a `/tree` row that lives in the prefix (navigate, edit label)
  loads the file at that point; the session stays paged until a choice needs
  an entry.

## Terminal integration

OSC 11 background auto-detect for theme defaults; OSC 7501 program status
(opt-in, `PI_PROGRAM_STATUS=1`); bracketed paste; hardware cursor optional
(`PI_HARDWARE_CURSOR`).

The footer's git branch comes from the checkout's `.git/HEAD` read
directly — polled once per second, walking parent directories and
following the `gitdir:` indirection used by worktrees and submodules.
`git rev-parse --abbrev-ref HEAD` is a one-shot fallback only when the
direct read cannot answer (bare setups, unusual `GIT_DIR` layouts).

`terminal.hyperlinks` and `terminal.trueColor` are pinned at TUI start
(`set_global_hyperlinks` + `set_color_mode_override`). pir has no
settings-reload hook yet, so a later settings change applies on the next
TUI start only.
