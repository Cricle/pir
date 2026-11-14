# TUI

pir's interactive TUI is a crossterm port of upstream's pi-tui components (there is no `ratatui` dependency; the
frame is a hand-rolled diff painter over crossterm): multiline editor with kill-ring/undo, streaming transcript
(markdown + diff + tool renderers), dialogs, selectors, autocomplete, and alt-screen mode.

## Layout

- **Editor**: multiline input, `!` prefix enters bash mode, kill-ring (ctrl+u/ctrl+k/ctrl+y, alt+y rotation), undo,
  `@file` autocomplete for paths, `/` autocomplete for commands, history (`up`/`down`), page scrolling.
- **Transcript**: assistant messages stream with markdown rendering; tool calls render as cards (title line + live
  output for shell tools + status tail `(exit N)` / `Took Xs`); compaction and notices render as bordered entries.
- **Alt-screen** (default fullscreen `tuiMode`; cycle the `tui-mode` row in `/settings`): scrollable viewport
  with search (`ctrl+f`, not `/`), selection copy, and a scrollbar; upstream scroll geometry (maxScrollTop clamps).
- **Image preview**: `/image <path>` renders a local PNG/JPEG/GIF/WebP with the resolved inline-image protocol when
  there is one (`terminal.images` or detection, including `PI_IMAGE_PROTOCOL`; Kitty transcodes non-PNG payloads),
  otherwise as a half-block truecolor preview (up to 20
  rows, `terminal.imageWidthCells`, default 60 cols). `terminal.showImages: false` (or a slim build without the
  `image-protocol` feature) renders the upstream text fallback `[Image: <path> [<mime>] <WxH>]`, with the read or
  decode error appended. A relative path resolves against the active session cwd.

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
- **Scrollbar** (`fullscreenScrollbar`, fullscreen alt-screen only — regular mode keeps its terminal scrollback and
  never reserves a column or paints a bar, `"always"` included): `"always"` reserves the transcript's last column
  and paints track + thumb; `"hidden"` never paints; `"auto"` (default) shows the transient bar when the transcript
  overflows *and* a scroll gesture happened in the last second (1000 ms fade). The thumb moves with the viewport
  (wheel, page keys, search jumps).
- **Search** (fullscreen only; `ctrl+f` on native Windows/WSL, `ctrl+shift+f` elsewhere — `tui.altScreen.search`)
  highlights matches; the box renders at the top of the transcript. Windowed search caches the
  ANSI-stripped corpus per row keyed by content, so only changed rows are re-parsed and an unchanged frame does no
  corpus work (`SearchFrameCache` in `altscreen_search.rs`). Expanded shell-tool output previews read the spilled
  file as a read-only `memmap2::Mmap` (`tui/spill_preview.rs`, `session`-gated; empty/failed-map/non-UTF-8 or
  >8 MiB fallback bodies make the preview unavailable and the in-memory tail renders as before — a 58 MB spill
  previews in ~1.0 ms vs ~59 ms for a whole-file read).

Typing a complete `[<Cb;Cx;CyM` shape is claimed as mouse (the price of ESC-less support); the ESC-prefixed
spelling (`PIR_TUI_VT_INPUT` on, the Windows default; Unix terminals always) keeps the byte-decoder path.

**Fast typed bursts become one paste.** The Windows console delivers a fast run of keys as per-character records in
one drain window; a run of `CONSOLE_BURST_MIN_CHARS` (8) or more printable keys (or any run containing a line feed)
is rewritten as one `Event::Paste` so the editor inserts it once instead of re-running autocomplete per character.
The coalescing is skipped while a selector or an open transcript search owns the focus, and while a dialog that does
not accept text is open, so those surfaces keep per-key semantics. An open search box consumes a paste chunk by
expanding it into its query (`paste_into_search`, the upstream focused-component `handleInput` behavior) instead of
swallowing it — PIR-TUI-023 fixed the dropped-burst bug.

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

The `tui.altScreen.*` chords (`bottom: ctrl+end`, `top: ctrl+home`, `search: ctrl+f` on native Windows/WSL else
`ctrl+shift+f`, `searchNext: enter|ctrl+g`, `searchPrevious: shift+enter|ctrl+shift+g`, `searchClose: escape`) live
in `ALT_SCREEN_KEYBINDINGS` in `src/tui/altscreen_search.rs`, upstream-verbatim (the platform-branched search chords
resolve through `config/keybindings.rs` first). Routing honors a settings override for every id; the painted labels
(`key_display_text`) resolve config layer → that table → the `tui` registry and show the default spelling, not a
settings override (the label seam has no live map).

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
- `/resume`, `/tree`, `/fork` (defaults to newest user message), `/clone`, `/export` (HTML), `/session`
  (usage/cost). `/fork` and `/clone` report upstream-verbatim status text (`Forked to new session` /
  `Cloned to new session`, PIR-TUI-019) and seed the composer with the forked-away user message.
  `/resume` lists the session directory once and filters the current cwd in memory (Tab switches to full list); it
  does not rescan files per scope. `/export` renders the session with the active theme's colors (upstream
  `themeName`) and needs a persisted session file: an ephemeral `--no-session` run keeps its transcript in
  `AgentSession.history` and has no `SessionManager` to render, so `/export` and RPC `export_html` report
  `cannot export an in-memory session (--no-session)` instead (upstream exports its live in-memory manager).
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
bracketed paste; optional hardware cursor (`PI_HARDWARE_CURSOR` / the `Show hardware cursor` settings row).

`showHardwareCursor`, `terminal.showTerminalProgress` and `terminal.clearOnShrink` are read at startup and also
apply live from `/settings` (PIR-TUI-017/018): the cursor cell and the OSC 9;4 progress gate move on the next
frame/run, and `clearOnShrink` gates the main-screen diff painter's full repaint on a frame shorter than the
tallest one painted since the last clearing render (default false, upstream `SETTINGS_DEFAULTS` — the default is
unchanged). The alt-screen/windowed renderer clamps every frame to the visible rows, so `clearOnShrink` has no
visible effect there — the setting is main-screen only, matching the separate upstream alt-screen renderer.

The footer's git branch comes from `.git/HEAD` read directly — polled every 5 s (`GIT_BRANCH_POLL_INTERVAL`,
`src/tui/terminal.rs:4769`), walking parent directories and following `gitdir:` indirection used by
worktrees/submodules. `git rev-parse --abbrev-ref HEAD` is a one-shot
fallback only when the direct read cannot answer (bare setups, unusual `GIT_DIR` layouts).

`terminal.hyperlinks` and `terminal.trueColor` are pinned at TUI start (`set_global_hyperlinks` +
`set_color_mode_override`). pir has no settings-reload hook yet, so a later change applies on the next TUI start.

## Intentional deviations from upstream

These are deliberate and must not be "fixed" back by a parity pass. Each names the
upstream behavior, pir's behavior and the reason.

### 1. PIR-TUI-027 — full repaints never erase terminal scrollback (`3J`)

Upstream `packages/tui/src/tui-main-screen.ts` `fullRender(true)` emits
`ESC[2J ESC[H ESC[3J` ("clear screen, home, then clear scrollback"), and
`TuiCore::invalidate_frame` used to mirror that byte sequence exactly.

pir diverges: the main-screen full repaint clears the **screen only**
(`ESC[2J ESC[H`) and never emits `ESC[3J`. That covers every caller:

- the `clearOnShrink` gate (`src/tui/core.rs`, diverges from upstream
  `tui-main-screen.ts`'s `fullRender(true)`),
- a regular-mode transcript window slide
  (`TuiCore::invalidate_baseline_keep_scrollback`, PIR-TUI-027),
- writer-drop keyframes (`force_screen_clear`, which already worked this way).

`TuiCore::invalidate_frame` still emits `3J`; it is reserved for the cases where
the surface genuinely left and re-entered the main screen (`/resume` picker,
external editor, SIGCONT rebuild), where a stale scrollback above the frame would
be actively wrong (a duplicated startup header, per PIR-TUI-018).

Reason (PROTOCOL v8, "性能优化边界" §1/§5c): on the main screen the terminal's
scrollback **is** the user's transcript history. A bounded transcript window
rebase, a cosmetic shrink repaint, or a dropped write frame must not discard
rows the user already scrolled past — that is "trading user-visible information
for a cheaper repaint", which the protocol forbids. Upstream pays that cost;
pir does not.

### 2. PIR-TUI-027 — the bounded transcript window announces its truncation

pir materializes at most `TRANSCRIPT_WINDOW_MAX_ENTRIES` (512) /
`TRANSCRIPT_WINDOW_MAX_BYTES` (4 MiB) entries of a huge session
(`src/tui/terminal.rs`; upstream has no such cap). Whenever the band starts at a
non-zero absolute index, `ChatBlockCache` paints one lead row above the first
materialized entry:

```
... (<N> earlier entries, <load-key> to load)
```

`N` is the window's absolute start (the exact number of entries not on screen).
`<load-key>` is `tui.altScreen.top` (`Ctrl+Home`) in the fullscreen surface; the
regular-mode surface has no viewport binding, so the row says
`scroll up to load` instead. Scrolling to the band top keeps extending the window
one `TRANSCRIPT_WINDOW_STEP_ENTRIES` chunk per frame, and once the resident
prefix is fully materialized the paged load-more streams older pages off disk
(`LoadOlder`), so entry 1 is reachable. Before this change the truncation was
silent, which read to users as "the window history was reset".

Reason: PROTOCOL v8 §5(a)/(b) — a tail-only window is allowed, but it must
(a) tell the user how much was omitted and (b) keep the omitted content
reachable.

### 3. PIR-TUI-027 — the window extension only drives itself while it moves

The bounded transcript window extends one `TRANSCRIPT_WINDOW_STEP_ENTRIES`
chunk per frame, and a frame only happens while the interaction loop is dirty.
To let `Ctrl+Home` walk all the way to entry 0 without the user pressing the
key repeatedly, the loop keeps itself dirty while the scrolled view is still
near the band top **and entries remain above it**.

That self-driving condition is gated on the band having ACTUALLY moved during
the frame that just rendered (`transcript_window.start()` strictly below its
frame-start value). Dropping that gate reintroduces a permanent repaint loop:
`advance_window` refuses to extend once the band is over
`TRANSCRIPT_WINDOW_MAX_BYTES` (4 MiB), so at the top of a fat session the
"near the band top" answer stays true forever while the extension can never
happen. Measured before the gate: 97-99 % CPU sustained and a 19 MB write log
with nothing changing on screen; after: 14 % → 0 % and settling to single
digits once the materialization the user asked for is done
(`scripts/tui_conpty/round68_tui027_spin_probe.py`, PIR-TUI-028).

The byte cap is not negotiable to make the jump one-shot: it is the same
memory bound that makes windowing worth having. What the user gets instead is
the truncation lead row, which always states how many entries are still above
the band, so "is there more" is never ambiguous.
