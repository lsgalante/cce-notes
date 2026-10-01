# cce-notes

The vault's notes editor — milestone 2 of the Obsidian-on-cce plan, built
on `cce-vault` (milestone 1). A file tree on the left and one note on the
right, shown as rendered Markdown (**reading view**) or edited as text
(**source mode**), toggled with Ctrl+E as in Obsidian.

Read `../cce-vault/CLAUDE.md` first: the index, watcher, writes and the
`markdown` document model this app draws all live there, along with their
invariants. The plan itself is a doc, "Obsidian-style notes on cce:
proposal".

## Layout

| File | What it owns |
| --- | --- |
| `main.rs` | The `Application`: panes, modes, history, quick switcher, autosave, conflicts, input |
| `reading.rs` | Lays out `cce_vault::markdown::Block`s at a width into draw items + click targets |
| `tree.rs` | The file tree's rows (folders first, case-insensitive, collapsible) |
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
- **Source mode autosaves** 1.5 s after typing stops (`AUTOSAVE_AFTER`,
  polled through `idle_poll_interval` only while dirty), on leaving a note,
  on switching to reading, and in `on_exit`.
- **Links:** click in reading, Ctrl+click in source (the caret's byte
  offset is matched against `cce_vault::parse` link spans). An unresolved
  link creates the note at the vault root and opens it in source mode, as
  Obsidian does. Ctrl state comes from `UiContext::ctrl_pressed` (the
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
cce-notes search <query>         # opens the quick switcher holding it
```

A second launch forwards its command to the running instance and exits.
Keys come from input.kdl's `cce-notes` domain: `quick_switcher` (ctrl+o),
`toggle_mode` (ctrl+e), `save` (ctrl+s), `reload` (ctrl+r), `back`
(alt+arrowleft), `forward` (alt+arrowright), `toggle_tree` (ctrl+\\),
`daily` (alt+d), `quit` (ctrl+q). Mouse back/forward walk the history too.

## Testing

`cargo test -p cce-notes` covers layout (with a fixed-width `Measure`), the
tree and the socket commands. Anything visual goes through a shadow at
`--scale 2` with `CCE_FONTS_DIR` set, against a **copy** of the vault
(`CCE_VAULT=<copy>`) — never the live vault, which syncs to other devices.
Fullscreen the window first (`ctl set-mode fullscreen cce-notes`): the
scale-2 shadow output is only 640×360 logical.

## Not done yet (milestone 2 → 3)

Backlinks and outline panes, `[[` completion, rename, the search pane, tag
search (a tag click does nothing yet), MCP tools, embedded images, the
icon (`Icon=cce-notes` has no SVG in cce-icons yet), per-note scroll in
the history, and the move of the reading view into cce-ui.
