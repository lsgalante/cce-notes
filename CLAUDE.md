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
| `images.rs` | Embedded images: link text → vault path → decode thread → upload; the lookup the reading view and the editor share |
| `paste.rs` | Ctrl+V and drops of a picture or image files: what the clipboard / drop offers, storing into the attachment folder, where the embeds go |
| `instance.rs` | The CLI's commands, on the single-instance socket `/tmp/cce-notes-<WAYLAND_DISPLAY>.sock` (claim, forward and listener are `cce_ui::ipc::instance`) |

## Behaviour worth knowing before changing it

- **The files are the source of truth; there is no daemon.** The app
  embeds an `Index` and a `VaultWatcher`. A watcher batch that touches the
  open note reloads it silently when the buffer is clean, and raises a
  **conflict** when it is dirty: autosave stops, Ctrl+S writes ours over
  the disk copy, Ctrl+R loads theirs (`reload`). Never overwrite a dirty
  conflict silently — Obsidian or Dropbox may be writing the same file.
  `save_now` enforces it for every caller: in a conflict only a forced
  save (Ctrl+S) writes, so leaving for reading view or renaming is
  refused with the hint instead of winning for us. Quitting in a conflict
  keeps ours as `<name> (conflict <time>).md` beside the note
  (`write_conflict_copy`) — there is no close prompt to ask with.
- **A note deleted or moved on disk is a conflict too** (`gone`):
  `write_text` creates missing files, so an ordinary save would bring back
  a note deleted elsewhere or duplicate one Obsidian renamed. Only Ctrl+S
  writes it again; Ctrl+R closes it; its return (a sync's delete then
  write) clears it.
- **Opening the note already open never reloads it** — a tree click, a
  search hit or backlink in it, a self-link, cce-graph, `cce-notes <it>`
  only go to the line. Reloading over the buffer is Ctrl+R alone.
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

## Embedded images (`images.rs`)

A paragraph that is one `![[pic.png]]` / `![](pic.png)` draws as the
picture in reading and in live preview (cce-ui's `markdown::layout_with`
and `DocEditor::set_images`; Obsidian's `|300` / `|300x200` sizes
honoured, too-wide images scaled to the column). An image embed inside a
sentence flows with the text in reading view (its line grows to fit) and
shows in a row below its line in live preview — the editor's rows are one
height, so a picture cannot sit inside one. Note embeds (`![[Note]]`)
still show as links.

- **Asked for while painting, loaded after.** A lookup the cache cannot
  answer is only recorded; `Images::pump`, at the end of `display_list`,
  resolves it with `Index::resolve_text` from the open note and decodes on
  a thread. The decode returns as `Message::ImageDecoded` (which wakes an
  idle loop) and is uploaded there; then the reading layout and the
  editor's line layouts are dropped so the link becomes the picture.
- **A link that resolves to an image already decoded still needs a
  relayout** — it drew as a link this frame. `pump` says so and the app
  sends itself `Message::ImagesReady`. Without it, every vault change (all
  links resolve afresh) turned loaded images back into links.
- **Ids die with the renderer.** `renderer_init` forgets them all on the
  second and later renderer (`seen_renderer`). Verified with
  `CCE_UI_FAULT_RECONNECT` at scale 2 against a control built without it:
  the control's images went blank, these stayed.
- **Pasting (Ctrl+V, `paste.rs`)** reads the clipboard with `wl-paste`
  under `PASTE_TIMEOUT` (`output_within`): the image-or-text decision runs
  on the UI thread, so an owner that never answers must not freeze it.
  A picture on the clipboard is saved
  as `Pasted image <YYYYMMDDHHMMSS>.<ext>`, and copied image files under
  their own names, in Obsidian's attachment folder for the note
  (`cce_vault::attachments`); each `![[…]]` goes on its own line at the
  caret. Copied files are checked first (a file manager offers their paths
  as `text/plain` too); a picture is taken only when no `text/plain` is
  offered, so text copied with an image rendering (browsers, office apps)
  still pastes as text. The link is the bare name unless another file of
  that name would win it. Shadow-test with `cce-shadow run wl-copy --type
  image/png < x.png` — the shadow has its own clipboard.
- **Dropping** (`drop_mimes` / `handle_drop`) takes the same two kinds,
  pixels first (`image/png`… — a browser's dragged picture), then
  `text/uri-list` (a file manager). The embed goes after the line under
  the pointer, never mid-line; a drop on reading view switches to editing
  and adds it at the end. Shadow-test with a GTK4 drag source and
  `ccectl pointer-press` / `pointer-move-to` / `pointer-release`; offer
  pixels with `Gdk.ContentProvider.new_for_bytes("image/png", …)` — a
  texture `new_for_value` from Python advertises only GTK's private type.
- **A line that is only an embed is its own block** in the parsed document
  (`cce_vault::markdown`), even inside a paragraph, so reading view and
  live preview agree on what draws as a picture.
- Rasters over 2048 px are scaled down on decode (reported at their own
  size, so sizing is unchanged); SVGs show at their intrinsic size,
  rasterised at twice it. Formats: png, jpeg, gif (first frame), webp,
  bmp, svg — not avif. The 32 most recently drawn stay decoded across
  notes. At most `MAX_DECODES` (4, fewer on a smaller CPU) decode at once
  (`decode_slots`): a raster decodes at full size before it shrinks, and a
  note's embeds all ask at once.

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
cce-notes [show]                 # just bring the instance up
```

`daily`, `search` and `show` are verbs, not note names: a note called one
of them opens with an explicit `open` (`cce-notes open show`).

A second launch forwards its command to the running instance and exits —
unless it names a `--vault` the instance does not show (it asks `vault`
first): then it runs as a window of its own, without the socket or MCP.
Keys come from input.kdl's `cce-notes` domain: `quick_switcher` (ctrl+o),
`toggle_mode` (ctrl+e), `toggle_source` (ctrl+shift+e: live preview ↔
source), `save` (ctrl+s), `reload` (ctrl+r), `back`
(alt+arrowleft), `forward` (alt+arrowright), `toggle_tree` (ctrl+\\),
`toggle_side` (ctrl+]), `search` (ctrl+shift+f), `rename` (f2), `graph`
(ctrl+g: `cce-graph --vault <this vault> --local`, single-instance), `daily`
(alt+d), `quit` (ctrl+q). Mouse back/forward walk the history too.

On the socket, `open <target>` takes everything after the verb as the
target — a note may be called "Chapter 3" — and a line follows a tab
(`open <target>\t<line>`). An older sender's `open <note> <line>` still
works: `open_target` reads the number as a line only when the whole text
names no note and the text before it does.

The instance socket also answers `current` (`ok <vault path>`) and
`vault` (`ok <root>`) straight from its listener thread, from values the
app keeps up to date (`instance::set_current`, `set_vault`) — cce-graph's
local graph polls `current`.

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
callouts show raw, note embeds (`![[Note]]`) show as links (image embeds
render; mid-sentence ones below their line rather than inside it), and a fenced block has no language label or copy button. Also heading
completion (`[[Note#`), rendered snippets in the panes (they show
raw lines), search debounce for large vaults (it scans every note per
keystroke), the icon (`Icon=cce-notes` has no SVG in
cce-icons yet), and per-note scroll in the history.
