# cce-notes

The vault's notes editor — milestones 2, 3 and 6 of the Obsidian-on-cce
plan, built on `cce-vault` (milestone 1). Three panes: files or search on
the left, one note in the middle — shown as rendered Markdown (**reading
view**) or edited with **live preview** (cce-ui's `DocEditor`: markup
hidden except on the caret's lines), toggled with Ctrl+E as in Obsidian;
Ctrl+Shift+E turns the preview off (**source mode**, the same editor) —
and backlinks or the outline on the right. Agents reach the
same vault through its MCP tools.

Read `../cce-vault/CLAUDE.md` first: the index, watcher, writes and the
`markdown` document model this app draws all live there, along with their
invariants. The plan itself is a doc, "Obsidian-style notes on cce:
proposal".

## Layout

| File | What it owns |
| --- | --- |
| `main.rs` | The `Application`: panes, modes, history, switcher + rename prompt, completion, autosave, conflicts, input; hosts the `DocEditor` |
| `reading.rs` | Lays out `cce_vault::markdown::Block`s at a width into draw items + click targets |
| `tree.rs` | The file tree's rows (folders first, case-insensitive, collapsible) |
| `panel.rs` | The scrollable header/title/line list both side panes draw through |
| `side.rs` | What the panes list: backlinks + unlinked mentions, outline, text and tag search |
| `complete.rs` | `[[` completion: the open link at the caret, the shortest link text, the splice |
| `mcp.rs` | MCP tools (`search`, `find_notes`, `read_note`, `backlinks`, `open_note`, `append_daily`, `current_note`) |
| `instance.rs` | Single instance on `/tmp/cce-notes-<WAYLAND_DISPLAY>.sock`; the CLI's commands |

## Behaviour worth knowing before changing it

- **The files are the source of truth; there is no daemon.** The app
  embeds an `Index` and a `VaultWatcher`. A watcher batch that touches the
  open note reloads it silently when the buffer is clean, and raises a
  **conflict** when it is dirty: autosave stops, Ctrl+S writes ours over
  the disk copy, Ctrl+R loads theirs. Never overwrite a dirty conflict
  silently — Obsidian or Dropbox may be writing the same file.
- **Our own writes come back through the watcher.** They are recognised by
  comparing the disk text with `saved` (the last text read or written), not
  by timing.
- **Editing autosaves** 1.5 s after typing stops (`AUTOSAVE_AFTER`,
  polled through `idle_poll_interval` only while dirty), on leaving a note,
  on switching to reading, and in `on_exit`. Dirty is the editor's
  `buf.revision` against `saved_rev` (no per-tick text compare); a save
  of text equal to the disk copy writes nothing.
- **Rename (F2, or a click on the title in the band)** goes through
  `Index::rename`, which rewrites every link to the note across the vault;
  the prompt previews the count from `plan_rename` before Enter. History
  entries follow the rename.
- **`[[` completion** opens while the caret sits in an unclosed `[[` on
  its line (`complete::open_link` over the caret's line, char indices;
  a `|` or `#` ends it). Enter/Tab inserts the shortest link text that
  still resolves (`link_text`) and `]]` as one undo step
  (`DocEditor::edit` replacing the line); Escape shuts it for that link.
  The popup sits at `DocEditor::caret_rect`, which is only current after
  `prepare` for this frame — `paint_note` prepares, then places it.
- **Popups over text:** text draws after every plate, so the completion
  popup is a *hole*: the editor is painted four times, clipped to the
  bands around the popup (clips intersect), and the popup fills the gap.
  The quick switcher instead stops painting what it covers.
- **`load_text` clears the editor's undo history** (`DocEditor::set_text`
  does); without that, Ctrl+Z after switching notes stepped back into the
  previous note's text.
- **Undo reaches the editor through `Application::undo` / `redo`.** The
  runner routes the undo chord to the focused widget, then those hooks,
  and only then `handle_key_input`; the editor is not a registered
  widget, so the hooks forward to it while it holds the keys
  (`editor_focused`).
- **Links:** click in reading and on a rendered link in live preview;
  Ctrl+click on the caret's (raw) line — the editor answers
  `Response::Follow`. Unresolved links fade (a resolver passed at paint,
  `paint_prepared_with`). An unresolved link creates the note at the vault
  root and opens it in the editor, as Obsidian does. Ctrl state comes from `UiContext::ctrl_pressed` (the
  Wayland modifiers event) — a key event's own `ctrl` flag is stale for the
  Ctrl press itself.

## The reading view (`reading.rs`)

cce-ui's text prim draws one run in one style, so a paragraph is laid out
**word by word**: each word measured, wrapped greedily, and consecutive
words of one look merged back into one prim. Things that were bugs once:

- **Measure through the renderer's own shaping entry**
  (`window_runner::get_text_buffer_attrs`) with a FontSystem that loads the
  **same fonts** as the renderer. The app returns `load_system_fonts() =
  true` because the DE sans (Noto Sans) has no bold/italic in the bundle —
  without it `**bold**` fell back to a serif face — and `measure_fs` is
  `create_font_system_with_system_fonts()` to match. Startup cost measured
  at ~330 ms to first map.
- **A word may span styles** (`` `code`, `` or `**bold**.`): `tokens()`
  glues them so punctuation never starts a line on its own.
- **Colours** for links, tags, highlights and callouts are written in sRGB
  and pass through `lin()`; prims take linear colour. `TEXT_FG`/`TEXT_DIM`
  are already linear.
- **Bullets are `Dot`s (`pc.circle`)**, not small rounded rects: the
  squircle corner shape draws a few-px radius as a square.
- **Text cannot be hidden under a plate** (the glyph pass runs after all
  geometry), so the quick switcher simply stops painting the note and tree
  while it is open rather than registering a popover.

This is the app-local first cut of the `MarkdownView` the proposal puts in
cce-ui. Move it there when a second surface (grid note cards, graph hover
previews) needs it, and then shape a whole paragraph as one rich-text
buffer instead of a buffer per word (each word is a `BUFFER_CACHE` entry
today, which a long note can churn).

## Commands

```sh
cce-notes [--vault <dir>] [open] <note>[#heading] [line]   # line is 1-based
cce-notes daily [YYYY-MM-DD]
cce-notes search <query>         # the search pane holding it (#tag works)
```

A second launch forwards its command to the running instance and exits.
Keys come from input.kdl's `cce-notes` domain: `quick_switcher` (ctrl+o),
`toggle_mode` (ctrl+e), `toggle_source` (ctrl+shift+e: live preview ↔
source), `save` (ctrl+s), `reload` (ctrl+r), `back`
(alt+arrowleft), `forward` (alt+arrowright), `toggle_tree` (ctrl+\\),
`toggle_side` (ctrl+]), `search` (ctrl+shift+f), `rename` (f2), `graph`
(ctrl+g: `cce-graph --vault <this vault> --local`, single-instance), `daily`
(alt+d), `quit` (ctrl+q). Mouse back/forward walk the history too.

The instance socket also answers `current` (`ok <vault path>`) straight
from its listener thread, from a value `open_path`/`rename` keep up to
date (`instance::set_current`) — cce-graph's local graph polls it.

## MCP

`claude mcp add --transport http cce-notes http://127.0.0.1:3002/mcp`.
Only the single instance serves it, and only with a vault. Tools run on
the app's event loop against the window's own index, so a write to a note
open and dirty in the window raises the usual conflict. A shadow instance
must move the port (`CCE_NOTES_MCP_PORT=3902`) — the live one holds 3002.

## Testing

`cargo test -p cce-notes` covers layout (with a fixed-width `Measure`), the
tree and the socket commands. Anything visual goes through a shadow at
`--scale 2` with `CCE_FONTS_DIR` set, against a **copy** of the vault
(`CCE_VAULT=<copy>`) — never the live vault, which syncs to other devices.
Fullscreen the window first (`ctl set-mode fullscreen cce-notes`): the
scale-2 shadow output is only 640×360 logical, too narrow for the right
pane (it yields below a 360 px note) — check three-pane layout in a
scale-1 shadow (1280×720) and pointer/caret maths at scale 2.

## Not done yet

In the editor: property values are edited as raw YAML (the table flips
to raw when the caret enters; Obsidian edits in place), tables and
callouts show raw, embeds (`![[…]]`) do not render,
and a fenced block has no language label or copy button. Also heading
completion (`[[Note#`), rendered snippets in the panes (they show
raw lines), search debounce for large vaults (it scans every note per
keystroke), embedded images, the icon (`Icon=cce-notes` has no SVG in
cce-icons yet), and per-note scroll in the history.
