//! `cce-notes` — the vault's notes editor (Obsidian-on-cce, milestone 2).
//!
//! A file tree on the left and one note on the right, read as rendered
//! Markdown (the reading view) or edited in live preview (markup shown
//! only on the caret's lines, cce-ui's `DocEditor`), toggled with Ctrl+E as
//! in Obsidian; Ctrl+Shift+E turns the preview off (source mode). Links
//! follow on click — or Ctrl+click on the caret's line — and a link to a
//! note that does not exist creates it. Ctrl+O is the quick switcher;
//! Alt+←/→ walk the history.
//!
//! The files are the source of truth and there is no daemon: the app
//! embeds a `cce_vault::Index` and a watcher, so an edit from Obsidian,
//! Dropbox or another cce app arrives like any other change. A clean
//! buffer reloads silently; a dirty one is flagged as a conflict and never
//! overwritten. Editing saves a moment after typing stops, on leaving the
//! note, and on exit.
//!
//! The vault comes from `--vault <dir>`, `$CCE_VAULT`, or
//! `vault { path "…" }` in `~/.config/cce/config.kdl`. The app runs single
//! instance (see [`instance`]); `cce-notes <note>` opens a note in it.

mod images;
mod instance;
mod reading;
mod tree;
mod complete;
mod mcp;
mod panel;
mod paste;
mod side;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use cce_ui::widget::Handle;
use cce_ui::engine::{Application, CursorIcon, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::{
    Adapted, Bounds, ElementState, Event, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, ScrollMotion,
    TextBox, WidgetHost,
};
use cce_ui::widget::doc_editor::{self, DocEditor, EditorTheme, Pos};
use cce_vault::markdown::{Block, SpanLink};
use cce_vault::{FileKind, Index, VaultWatcher};

use instance::Command;
use panel::{Action, Panel};
use reading::{srgb_u8, Hit, Layout, ShapingMeasure, Theme};

const BAND_H: f32 = 40.0;
const STATUS_H: f32 = 26.0;
const TREE_W: f32 = 240.0;
const TREE_ROW_H: f32 = 24.0;
/// Obsidian's "readable line length": the reading column's widest.
const READ_MAX_W: f32 = 720.0;
const READ_PAD: f32 = 28.0;
const READ_SIZE: f32 = 15.0;
const SWITCHER_W: f32 = 560.0;
const SWITCHER_ROW_H: f32 = 28.0;
const SWITCHER_ROWS: usize = 10;
const SIDE_W: f32 = 260.0;
/// The note pane's narrowest before the side pane gives way to it.
const NOTE_MIN_W: f32 = 360.0;
const COMPLETE_W: f32 = 320.0;
const COMPLETE_ROW_H: f32 = 24.0;
/// Editing saves once typing has paused this long.
const AUTOSAVE_AFTER: Duration = Duration::from_millis(1500);

const FG: [f32; 4] = cce_ui::colors::TEXT_FG;
const DIM: [f32; 4] = cce_ui::colors::TEXT_DIM;
const ROW_HOVER: [f32; 4] = [1.0, 1.0, 1.0, 0.05];
const ROW_CURRENT: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
const CONFLICT: [f32; 4] = [0.95, 0.45, 0.35, 1.0];

#[derive(Debug, Clone)]
pub enum Message {
    Command(Command),
    VaultChanged(Vec<PathBuf>),
    Mcp(cce_ui::mcp::McpToolCall),
    /// An embedded image finished decoding (vault path, pixels or failure).
    ImageDecoded(String, Option<images::Decoded>),
    /// Links resolved to images already decoded: lay out again.
    ImagesReady,
    Exit,
}

/// Startup arguments, fixed before the engine starts (`Application::new`
/// takes none).
struct Startup {
    vault: Result<PathBuf, String>,
    command: Command,
}

static STARTUP: OnceLock<Startup> = OnceLock::new();

/// Shortcuts, from input.kdl's `cce-notes` domain over the `cce-ui` one.
struct Keys {
    switcher: String,
    toggle_mode: String,
    toggle_source: String,
    save: String,
    reload: String,
    back: String,
    forward: String,
    toggle_tree: String,
    toggle_side: String,
    search: String,
    rename: String,
    graph: String,
    daily: String,
    quit: String,
}

impl Keys {
    fn load() -> Keys {
        let get = cce_ui::input::app_chord;
        Keys {
            switcher: get("quick_switcher", "ctrl+o"),
            toggle_mode: get("toggle_mode", "ctrl+e"),
            toggle_source: get("toggle_source", "ctrl+shift+e"),
            save: get("save", "ctrl+s"),
            reload: get("reload", "ctrl+r"),
            back: get("back", "alt+arrowleft"),
            forward: get("forward", "alt+arrowright"),
            toggle_tree: get("toggle_tree", "ctrl+\\"),
            toggle_side: get("toggle_side", "ctrl+]"),
            search: get("search", "ctrl+shift+f"),
            rename: get("rename", "f2"),
            graph: get("graph", "ctrl+g"),
            daily: get("daily", "alt+d"),
            quit: get("quit", "ctrl+q"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Reading,
    /// The editor: live preview, or source with the preview off.
    Source,
}

/// The left pane's tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LeftTab {
    Files,
    Search,
}

/// The right pane's tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SideTab {
    Backlinks,
    Outline,
}

/// `[[` completion while editing: the link being typed and its choices.
struct Completion {
    /// The caret's line, and the char index in it just past the `[[`.
    line: usize,
    start: usize,
    query: String,
    /// Vault paths, best first.
    choices: Vec<String>,
    selected: usize,
}

/// One quick-switcher result: an existing note, or "create this".
/// `alias` is the name the query matched when that is not the note's own
/// (an alias): the row shows the path, an `arrow-left` glyph, then it.
#[derive(Clone, Debug)]
enum Choice {
    Note { path: String, alias: Option<String> },
    Create(String),
}

struct Switcher {
    choices: Vec<Choice>,
    selected: usize,
    /// The query the choices were computed for.
    query: String,
    /// A rename prompt for this note rather than a switcher.
    rename: Option<String>,
    /// A line under the input: what a rename will do, or why it cannot.
    hint: Option<(String, bool)>,
}

struct NotesApp {
    keys: Keys,
    index: Option<Index>,
    vault_error: Option<String>,
    _watcher: Option<VaultWatcher>,

    tree: tree::Tree,
    rows: Vec<tree::Row>,
    show_tree: bool,
    tree_scroll: f32,
    tree_motion: ScrollMotion,
    tree_hover: Option<usize>,

    /// The open note's vault path.
    current: Option<String>,
    /// The note's text as last read from or written to disk.
    saved: String,
    blocks: Vec<Block>,
    theme: Theme,
    layout: Option<(f32, Layout)>,
    /// Shapes for layout widths; loads the same fonts as the renderer
    /// (system fonts included, see `load_system_fonts`). Made on first use.
    measure: Option<ShapingMeasure>,
    mode: Mode,
    read_scroll: f32,
    read_motion: ScrollMotion,
    /// A source line to bring into view once the layout exists.
    pending_line: Option<usize>,
    hover_hit: bool,

    editor: DocEditor,
    /// Embedded images, shared with the editor (see `images.rs`).
    images: images::Images,
    sender: calloop::channel::Sender<Message>,
    /// A renderer has been made before: the next one is a reconnect, and
    /// every image id died with the old one.
    seen_renderer: bool,
    /// The editor's revision when its text last matched the disk.
    saved_rev: u64,
    /// The revision as last seen by the autosave check, and when it changed.
    edit_seen: u64,
    edit_changed_at: Instant,

    history: Vec<String>,
    hist_pos: usize,

    switcher: Option<Switcher>,
    switcher_input: Handle<Adapted<TextBox>>,

    left_tab: LeftTab,
    search_input: Handle<Adapted<TextBox>>,
    search_panel: Panel,
    /// The query the search panel shows results for.
    search_seen: String,

    show_side: bool,
    side_tab: SideTab,
    side_panel: Panel,
    /// What the side panel was built for; a change resets its scroll.
    side_key: Option<(String, SideTab)>,
    side_dirty: bool,

    completion: Option<Completion>,
    /// A `[[` (line, start) whose completion was dismissed with Escape: it
    /// stays shut until the caret leaves that link.
    completion_dismissed: Option<(usize, usize)>,

    /// The disk copy of the open note diverged from the buffer: autosave
    /// stops and only Ctrl+S (keep mine) or Ctrl+R (load theirs) resolves
    /// it. Also set, with `gone`, when the note vanished on disk.
    conflict: bool,
    /// The open note was deleted or moved on disk. A save would recreate it
    /// (`write_text` creates missing files): bringing back a note deleted
    /// elsewhere, or duplicating one Obsidian renamed. So it is a conflict
    /// too — only Ctrl+S writes it again, and Ctrl+R closes it.
    gone: bool,
    status: Option<(String, bool)>,

    width: u32,
    height: u32,
    scale: f64,
    needs_rebuild: bool,
    ui_context: cce_ui::context::UiContext,
    pointer: (f32, f32),
}

/// Where everything sits for a window size.
struct Metrics {
    /// The whole left pane: tabs, then the tree or the search.
    tree: Rect,
    left_tabs: Rect,
    /// The search box, when the search tab shows.
    search_box: Rect,
    /// Below the tabs (and search box): the tree's rows or the results.
    tree_body: Rect,
    note: Rect,
    side: Rect,
    side_tabs: Rect,
    side_body: Rect,
    /// The note's name in the band; a click renames it.
    title: Rect,
    mode_chip: Rect,
}

impl NotesApp {
    /// The app over an opened vault (or the error opening it), showing no
    /// note yet — `create` adds the instance listener, MCP and the startup
    /// command; tests start here.
    fn new(
        index: Option<Index>,
        vault_error: Option<String>,
        watcher: Option<VaultWatcher>,
        sender: calloop::channel::Sender<Message>,
    ) -> NotesApp {
        let images = images::Images::default();
        let mut editor = DocEditor::new("", EditorTheme::new(READ_SIZE), true);
        let shared = images.clone();
        editor.set_images(Box::new(move |t| shared.lookup(t)));
        editor.max_width = READ_MAX_W;
        editor.pad = READ_PAD;
        let switcher_input = TextBox::new(String::new()).with_placeholder("Find or create a note…");
        let search_input = TextBox::new(String::new()).with_placeholder("Search");

        // The context owns the widgets; the app keeps their handles.
        let mut ui_context = cce_ui::context::UiContext::new();
        let mut app = NotesApp {
            keys: Keys::load(),
            index,
            vault_error,
            _watcher: watcher,
            tree: tree::Tree::default(),
            rows: Vec::new(),
            show_tree: true,
            tree_scroll: 0.0,
            tree_motion: ScrollMotion::new(),
            tree_hover: None,
            current: None,
            saved: String::new(),
            blocks: Vec::new(),
            theme: Theme { body_font: "sans-serif".into(), mono_font: "monospace".into(), size: READ_SIZE },
            layout: None,
            measure: None,
            mode: Mode::Reading,
            read_scroll: 0.0,
            read_motion: ScrollMotion::new(),
            pending_line: None,
            hover_hit: false,
            saved_rev: editor.buf.revision,
            edit_seen: editor.buf.revision,
            editor,
            images,
            sender: sender.clone(),
            seen_renderer: false,
            edit_changed_at: Instant::now(),
            history: Vec::new(),
            hist_pos: 0,
            switcher: None,
            switcher_input: ui_context.insert(switcher_input),
            left_tab: LeftTab::Files,
            search_input: ui_context.insert(search_input),
            search_panel: Panel::default(),
            search_seen: String::new(),
            show_side: true,
            side_tab: SideTab::Backlinks,
            side_panel: Panel::default(),
            side_key: None,
            side_dirty: true,
            completion: None,
            completion_dismissed: None,
            conflict: false,
            gone: false,
            status: None,
            width: 1000,
            height: 700,
            scale: 1.0,
            needs_rebuild: true,
            ui_context,
            pointer: (0.0, 0.0),
        };
        app.rebuild_rows();
        app
    }

    fn metrics(&self) -> Metrics {
        let (w, h) = (self.width as f32, self.height as f32);
        let body_h = (h - BAND_H - STATUS_H).max(0.0);
        let tree_w = if self.show_tree && self.index.is_some() { TREE_W.min(w * 0.4) } else { 0.0 };
        let side_w = if self.show_side && self.current.is_some() && w - tree_w - SIDE_W >= NOTE_MIN_W {
            SIDE_W
        } else {
            0.0
        };
        let inset = cce_ui::layout::root_plate_inset();
        // The tab strips, mode chip and search box at the toolkit's control height.
        let tab_h = cce_ui::layout::button_height();
        let search_h = cce_ui::layout::textbox_height();
        let chip_h = cce_ui::layout::button_height();
        let tree = Rect { x: 0.0, y: BAND_H, width: tree_w, height: body_h };
        let left_tabs = Rect { x: 0.0, y: BAND_H, width: tree_w, height: tab_h };
        let search_box = Rect { x: 8.0, y: BAND_H + tab_h + 4.0, width: (tree_w - 16.0).max(0.0), height: search_h };
        let body_top = if self.left_tab == LeftTab::Search { search_box.y + search_h + 4.0 } else { BAND_H + tab_h };
        let side = Rect { x: w - side_w, y: BAND_H, width: side_w, height: body_h };
        let mode_chip = Rect { x: w - inset - 84.0, y: (BAND_H - chip_h) / 2.0, width: 84.0, height: chip_h };
        Metrics {
            tree,
            left_tabs,
            search_box,
            tree_body: Rect { x: 0.0, y: body_top, width: tree_w, height: (BAND_H + body_h - body_top).max(0.0) },
            note: Rect { x: tree_w, y: BAND_H, width: w - tree_w - side_w, height: body_h },
            side,
            side_tabs: Rect { x: side.x, y: BAND_H, width: side_w, height: tab_h },
            side_body: Rect { x: side.x, y: BAND_H + tab_h, width: side_w, height: (body_h - tab_h).max(0.0) },
            title: Rect { x: inset, y: 0.0, width: (mode_chip.x - 12.0 - inset).max(0.0), height: BAND_H },
            mode_chip,
        }
    }

    /// The reading column's origin and width inside the note pane.
    fn reading_frame(&self, m: &Metrics) -> (f32, f32, f32) {
        let width = (m.note.width - 2.0 * READ_PAD).clamp(80.0, READ_MAX_W);
        let x = m.note.x + (m.note.width - width) / 2.0;
        (x, m.note.y + READ_PAD, width)
    }

    /// The editor fills the note pane; it centres its own column.
    fn editor_rect(&self, m: &Metrics) -> Rect {
        m.note
    }

    /// The editor takes keys: editing, with nothing else holding them.
    fn editor_focused(&self) -> bool {
        self.mode == Mode::Source && self.switcher.is_none() && !self.ui_context[self.search_input].editing
    }

    fn rebuild_rows(&mut self) {
        let Some(ix) = &self.index else { return };
        let notes = ix.files().iter().filter(|(_, e)| e.kind == FileKind::Note).map(|(p, _)| p.as_str());
        self.rows = self.tree.rows(notes);
        self.tree_hover = None;
    }

    fn editor_text(&self) -> String {
        self.editor.text()
    }

    fn dirty(&self) -> bool {
        self.mode == Mode::Source && self.current.is_some() && self.editor.buf.revision != self.saved_rev
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error));
    }

    fn invalidate(&mut self) {
        self.layout = None;
        self.needs_rebuild = true;
    }

    // ---- notes -------------------------------------------------------

    /// Write the buffer to `<name> (conflict <time>).md` beside the open
    /// note, leaving the note itself as it is on disk. The path written.
    fn write_conflict_copy(&mut self) -> Result<String, String> {
        let text = self.editor_text();
        let cur = self.current.clone().ok_or("no note is open")?;
        let ix = self.index.as_mut().ok_or("no vault")?;
        let (dir, file) = match cur.rsplit_once('/') {
            Some((d, f)) => (format!("{d}/"), f),
            None => (String::new(), cur.as_str()),
        };
        let stem = file.strip_suffix(".md").unwrap_or(file);
        let when = chrono::Local::now().format("%Y-%m-%d %H%M%S");
        for n in 1.. {
            let suffix = if n == 1 { String::new() } else { format!(" {n}") };
            match ix.create(&format!("{dir}{stem} (conflict {when}{suffix}).md"), &text) {
                Ok(path) => return Ok(path),
                Err(cce_vault::WriteError::Exists(_)) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        unreachable!()
    }

    /// Show `text` as the current note's content, as on disk.
    fn load_text(&mut self, text: String) {
        self.images.note_changed();
        self.blocks = cce_vault::markdown::blocks(&text);
        // Clears the undo history too: undo must not step back into another
        // note's (or the pre-reload) text.
        self.editor.set_text(&text);
        self.saved_rev = self.editor.buf.revision;
        self.completion = None;
        self.edit_seen = self.saved_rev;
        self.saved = text;
        self.conflict = false;
        self.gone = false;
        self.side_dirty = true;
        self.invalidate();
    }

    /// What the status line says while a conflict holds.
    fn conflict_hint(&self) -> &'static str {
        if self.gone {
            "Deleted or moved on disk · Ctrl+S saves yours again · Ctrl+R closes it"
        } else {
            "Changed on disk · Ctrl+S keeps yours · Ctrl+R reloads"
        }
    }

    /// Write the source buffer if it differs from disk.
    fn save(&mut self) -> bool {
        self.save_now(false)
    }

    /// [`save`](Self::save), or with `force` write even an unchanged buffer.
    fn save_now(&mut self, force: bool) -> bool {
        if !self.dirty() && !(force && self.mode == Mode::Source) {
            return true;
        }
        let text = self.editor_text();
        let rev = self.editor.buf.revision;
        if self.conflict && !force {
            // Only Ctrl+S ("keep mine") writes over a conflict. Every other
            // save — leaving for reading view, a rename — used to land here
            // and overwrite the disk copy without a word.
            self.set_status(self.conflict_hint(), true);
            return false;
        }
        if text == self.saved && !force {
            // Edited back to what is on disk: nothing to write.
            self.saved_rev = rev;
            return true;
        }
        let (Some(cur), Some(ix)) = (self.current.clone(), self.index.as_mut()) else { return false };
        match ix.write_text(&cur, &text) {
            Ok(()) => {
                self.saved = text;
                self.saved_rev = rev;
                self.conflict = false;
                self.gone = false;
                true
            }
            Err(e) => {
                self.set_status(format!("Could not save {cur}: {e}"), true);
                false
            }
        }
    }

    /// Open a vault path, recording it in the history unless walking it.
    fn open_path(&mut self, path: &str, line: Option<usize>, record: bool) {
        let Some(ix) = &self.index else { return };
        let kind = ix.entry(path).map(|e| e.kind);
        match kind {
            Some(FileKind::Note) | None => {}
            Some(_) => {
                // Canvases and attachments open in their own apps.
                let abs = ix.abs(path);
                let mut open = std::process::Command::new("xdg-open");
                open.arg(abs);
                let _ = spawn_detached(open);
                return;
            }
        }
        if self.current.as_deref() == Some(path) {
            // Already open — a click on it in the tree, a search hit or
            // backlink in it, a self-link, cce-graph, `cce-notes <it>`. This
            // used to read the file again over the buffer: the typing since
            // the last autosave was lost, and in a conflict all of it.
            // Reloading on purpose is Ctrl+R (`reload`).
            if let Some(l) = line {
                self.goto_line(l);
            }
            self.tree.reveal(path);
            self.rebuild_rows();
            return;
        }
        if self.conflict && self.dirty() {
            self.set_status("Resolve the conflict first: Ctrl+S keeps yours, Ctrl+R loads the disk copy", true);
            return;
        }
        if !self.save() {
            return;
        }
        let Some(ix) = &self.index else { return };
        let text = match ix.read_text(path) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(format!("Could not read {path}: {e}"), true);
                return;
            }
        };
        self.current = Some(path.to_string());
        instance::set_current(Some(path));
        self.load_text(text);
        self.read_scroll = 0.0;
        self.read_motion = ScrollMotion::new();
        if let Some(l) = line {
            self.goto_line(l);
        }
        self.tree.reveal(path);
        self.rebuild_rows();
        if record {
            self.history.truncate(self.hist_pos + 1);
            if self.history.last().map(String::as_str) != Some(path) {
                self.history.push(path.to_string());
            }
            self.hist_pos = self.history.len() - 1;
        }
        self.status = None;
    }

    /// Ctrl+R: the disk copy over the buffer — what a conflict's "load
    /// theirs" means, so unsaved edits go. A note gone from disk closes.
    fn reload(&mut self) {
        let (Some(cur), Some(ix)) = (self.current.clone(), self.index.as_ref()) else { return };
        match ix.read_text(&cur) {
            Ok(text) => {
                let scroll = self.read_scroll;
                self.load_text(text);
                self.read_scroll = scroll;
                self.status = None;
            }
            Err(_) if self.gone => self.close_note(),
            Err(e) => self.set_status(format!("Could not read {cur}: {e}"), true),
        }
    }

    /// Show no note (the one open is gone and its edits were given up).
    fn close_note(&mut self) {
        self.current = None;
        instance::set_current(None);
        self.load_text(String::new());
        self.mode = Mode::Reading;
        self.read_scroll = 0.0;
        self.status = None;
    }

    /// Open what a command or argv names: an absolute file in the vault,
    /// a vault path, or a note name; with an optional `#heading`.
    fn open_target(&mut self, target: &str, line: Option<usize>) {
        if self.index.is_none() {
            return;
        }
        // An older sender's `open <note> <line>` arrives whole (the line
        // now rides after a tab): a trailing number is a line only when the
        // whole text names no note and the text before it does — so a note
        // called "Chapter 3" opens as itself.
        if line.is_none() && self.resolve_target(target).is_none() {
            if let Some((t, l)) = target.rsplit_once(' ') {
                if let (Ok(l), false) = (l.parse::<usize>(), t.trim().is_empty()) {
                    if self.resolve_target(t.trim_end()).is_some() {
                        return self.open_target(t.trim_end(), Some(l));
                    }
                }
            }
        }
        let (name, sub) = match target.split_once('#') {
            Some((n, s)) => (n, Some(s)),
            None => (target, None),
        };
        match self.resolve_target(target) {
            Some(p) => {
                let line = line.map(|l| l.saturating_sub(1)).or_else(|| sub.and_then(|s| self.subpath_line(&p, s)));
                self.open_path(&p, line, true);
            }
            None => self.set_status(format!("No note named {name}"), true),
        }
    }

    /// The vault path a command's target names (its `#heading` aside): an
    /// absolute file in the vault, a vault path, or a note name.
    fn resolve_target(&self, target: &str) -> Option<String> {
        let ix = self.index.as_ref()?;
        let name = target.split_once('#').map_or(target, |(n, _)| n);
        if Path::new(name).is_absolute() {
            let p = Path::new(name);
            p.canonicalize().ok().and_then(|c| ix.rel(&c)).or_else(|| ix.rel(p))
        } else {
            ix.lookup(name)
        }
    }

    /// The source line of `#Heading` or `#^block` in a note.
    fn subpath_line(&self, path: &str, sub: &str) -> Option<usize> {
        let note = self.index.as_ref()?.note(path)?;
        if let Some(id) = sub.strip_prefix('^') {
            return note.blocks.iter().find(|b| b.id == id).map(|b| b.line);
        }
        let want = sub.trim().to_lowercase();
        note.headings.iter().find(|h| h.text.trim().to_lowercase() == want).map(|h| h.line)
    }

    fn follow(&mut self, link: SpanLink) {
        match link {
            SpanLink::Note { target, subpath } | SpanLink::Embed { target, subpath } => {
                let Some(ix) = &self.index else { return };
                if target.is_empty() {
                    // `[[#Heading]]`: within this note.
                    if let (Some(cur), Some(s)) = (self.current.clone(), subpath) {
                        self.pending_line = self.subpath_line(&cur, &s);
                    }
                    return;
                }
                match ix.resolve_text(self.current.as_deref(), &target) {
                    Some(p) => {
                        let line = subpath.and_then(|s| self.subpath_line(&p, &s));
                        self.open_path(&p, line, true);
                    }
                    None => self.create_and_open(&target),
                }
            }
            SpanLink::Url(url) => {
                let mut open = std::process::Command::new("xdg-open");
                open.arg(url);
                let _ = spawn_detached(open);
            }
            SpanLink::Tag(tag) => self.open_search(&format!("#{tag}")),
        }
    }

    /// Create a note for a link or switcher query with no target, and open
    /// it in source mode, as Obsidian does on a click on an unresolved link.
    fn create_and_open(&mut self, name: &str) {
        let Some(ix) = self.index.as_mut() else { return };
        let name = name.trim().trim_start_matches('/');
        let path = if name.to_lowercase().ends_with(".md") { name.to_string() } else { format!("{name}.md") };
        match ix.create(&path, "") {
            Ok(rel) => {
                self.open_path(&rel, None, true);
                self.set_mode(Mode::Source);
            }
            Err(e) => self.set_status(format!("Could not create {path}: {e}"), true),
        }
    }

    fn toggle_task(&mut self, line: usize, status: char) {
        let (Some(cur), Some(ix)) = (self.current.clone(), self.index.as_mut()) else { return };
        let next = if status == ' ' { 'x' } else { ' ' };
        match ix.set_task(&cur, line, next).and_then(|_| ix.read_text(&cur).map_err(Into::into)) {
            Ok(text) => {
                let scroll = self.read_scroll;
                self.load_text(text);
                self.read_scroll = scroll;
            }
            Err(e) => self.set_status(format!("Could not update the task: {e}"), true),
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode || self.current.is_none() {
            return;
        }
        if mode == Mode::Reading {
            if !self.save() {
                return;
            }
            let text = self.editor_text();
            self.blocks = cce_vault::markdown::blocks(&text);
            self.completion = None;
            self.invalidate();
        } else {
            self.ui_context.unfocus_id(self.search_input.id());
        }
        self.mode = mode;
        self.needs_rebuild = true;
    }

    fn go_history(&mut self, delta: isize) {
        let next = self.hist_pos as isize + delta;
        if next < 0 || next as usize >= self.history.len() {
            return;
        }
        let path = self.history[next as usize].clone();
        if self.index.as_ref().is_some_and(|ix| ix.entry(&path).is_some()) {
            self.hist_pos = next as usize;
            self.open_path(&path, None, false);
        } else {
            // Deleted since: drop it and keep walking.
            self.history.remove(next as usize);
            if (next as usize) < self.hist_pos {
                self.hist_pos -= 1;
            }
            self.go_history(delta);
        }
    }

    fn open_daily(&mut self, date: Option<chrono::NaiveDate>) {
        let Some(ix) = self.index.as_mut() else { return };
        let date = date.unwrap_or_else(|| chrono::Local::now().date_naive());
        match ix.daily(date, true) {
            Ok((path, _created)) => self.open_path(&path, None, true),
            Err(e) => self.set_status(format!("Could not open the daily note: {e}"), true),
        }
    }

    fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Open { target, line } => self.open_target(&target, line),
            Command::Daily(d) => self.open_daily(d),
            Command::Search(q) => self.open_search(&q),
            Command::Show => {}
        }
        self.needs_rebuild = true;
    }

    // ---- vault changes -------------------------------------------------

    fn vault_changed(&mut self, paths: Vec<PathBuf>) {
        let Some(ix) = self.index.as_mut() else { return };
        // A path the index has not seen is a file (or folder) that just
        // appeared; with a removal, the vault's set of files changed — the
        // only change that can move what a link resolves to.
        let appeared = paths.iter().any(|p| ix.rel(p).is_some_and(|r| ix.entry(&r).is_none()));
        let changes = ix.apply_changes(&paths);
        // An image added, changed or removed: links resolve again, and the
        // editor asks again (it only asks while laying a line out).
        let images_touched = paths.iter().any(|p| cce_vault::markdown::is_image(&p.to_string_lossy()));
        let relink = appeared || !changes.removed.is_empty() || images_touched;
        self.images.vault_changed(ix, &paths, relink);
        if images_touched {
            self.editor.invalidate();
        }
        self.rebuild_rows();
        // Link colours, backlinks and search hits depend on every file.
        self.layout = None;
        self.side_dirty = true;
        if self.left_tab == LeftTab::Search {
            self.refresh_search(true);
        }
        let Some(cur) = self.current.clone() else { return };
        let Some(ix) = self.index.as_ref() else { return };
        let abs = ix.abs(&cur);
        let canon = abs.canonicalize().ok();
        let touches = |p: &PathBuf| p == &abs || (canon.is_some() && p.canonicalize().ok() == canon);
        if !paths.iter().any(touches) {
            return;
        }
        match ix.read_text(&cur) {
            // Back to what we last read or wrote (or back from gone): no
            // divergence left.
            Ok(disk) if disk == self.saved => {
                self.conflict = false;
                self.gone = false;
            }
            Ok(disk) => {
                self.gone = false;
                if self.dirty() {
                    self.conflict = true;
                } else {
                    let scroll = self.read_scroll;
                    self.load_text(disk);
                    self.read_scroll = scroll;
                }
            }
            // Deleted or moved: hold every save (see `gone`), dirty or not —
            // a clean buffer typed into later would recreate it too.
            Err(_) => {
                self.gone = true;
                self.conflict = true;
            }
        }
    }

    // ---- quick switcher -------------------------------------------------

    fn open_switcher(&mut self, query: &str) {
        self.ui_context[self.switcher_input].text = query.to_string();
        self.ui_context[self.switcher_input].edit_buffer = query.to_string();
        self.ui_context[self.switcher_input].cursor_idx = query.chars().count();
        self.ui_context[self.switcher_input].select_anchor = None;
        self.switcher =
            Some(Switcher { choices: Vec::new(), selected: 0, query: "\u{0}".into(), rename: None, hint: None });
        self.ui_context.set_focused_id(self.switcher_input.id());
        WidgetHost::focus(&mut self.ui_context[self.switcher_input]);
        self.refresh_switcher();
    }

    fn close_switcher(&mut self) {
        self.switcher = None;
        self.ui_context.unfocus_id(self.switcher_input.id());
        self.needs_rebuild = true;
    }

    fn switcher_query(&self) -> String {
        if self.ui_context[self.switcher_input].editing {
            self.ui_context[self.switcher_input].edit_buffer.clone()
        } else {
            self.ui_context[self.switcher_input].text.clone()
        }
    }

    fn refresh_switcher(&mut self) {
        let query = self.switcher_query();
        let Some(ix) = &self.index else { return };
        let Some(sw) = self.switcher.as_mut() else { return };
        if sw.query == query {
            return;
        }
        if let Some(from) = sw.rename.clone() {
            sw.choices = Vec::new();
            sw.hint = Some(match rename_target(&from, &query) {
                None => ("Type the note's new name or path".into(), false),
                Some(to) if to == from => ("Unchanged".into(), false),
                Some(to) => match ix.plan_rename(&from, &to) {
                    Ok(plan) => {
                        let notes: std::collections::BTreeSet<&str> = plan.edits.iter().map(|e| e.path.as_str()).collect();
                        let what = if plan.edits.is_empty() {
                            "no links to update".to_string()
                        } else {
                            format!("updates {} in {}", side::plural(plan.edits.len(), "link"), side::plural(notes.len(), "note"))
                        };
                        (format!("Enter renames to {to} — {what}"), false)
                    }
                    Err(e) => (e.to_string(), true),
                },
            });
            sw.query = query;
            self.needs_rebuild = true;
            return;
        }
        sw.choices = if query.trim().is_empty() {
            // Recent first: the history, then the newest files.
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for p in self.history.iter().rev() {
                if ix.entry(p).is_some() && seen.insert(p.clone()) {
                    out.push(p.clone());
                }
            }
            let mut notes: Vec<(&String, u64)> =
                ix.files().iter().filter(|(_, e)| e.kind == FileKind::Note).map(|(p, e)| (p, e.mtime)).collect();
            notes.sort_by_key(|a| std::cmp::Reverse(a.1));
            for (p, _) in notes {
                if seen.insert(p.clone()) {
                    out.push(p.clone());
                }
            }
            out.into_iter()
                .take(SWITCHER_ROWS)
                .map(|p| Choice::Note { path: p, alias: None })
                .collect()
        } else {
            let mut c: Vec<Choice> = ix
                .find(&query, SWITCHER_ROWS)
                .into_iter()
                .filter(|m| ix.entry(&m.path).is_some_and(|e| e.kind == FileKind::Note))
                .map(|m| {
                    let alias = (!m.path.trim_end_matches(".md").ends_with(m.matched.as_str())).then_some(m.matched);
                    Choice::Note { path: m.path, alias }
                })
                .collect();
            if ix.lookup(&query).is_none() {
                c.push(Choice::Create(query.trim().to_string()));
            }
            c
        };
        sw.selected = 0;
        sw.query = query;
        self.needs_rebuild = true;
    }

    fn switcher_pick(&mut self, create: bool) {
        let Some(sw) = &self.switcher else { return };
        let query = self.switcher_query();
        if let Some(from) = sw.rename.clone() {
            match rename_target(&from, &query) {
                Some(to) if to != from => {
                    if sw.hint.as_ref().is_some_and(|h| h.1) {
                        return; // the plan already said why it cannot
                    }
                    self.close_switcher();
                    self.rename(&from, &to);
                }
                Some(_) => self.close_switcher(),
                None => {}
            }
            return;
        }
        let choice = if create { Some(Choice::Create(query.trim().to_string())) } else { sw.choices.get(sw.selected).cloned() };
        self.close_switcher();
        match choice {
            Some(Choice::Note { path, .. }) => self.open_path(&path, None, true),
            Some(Choice::Create(name)) if !name.is_empty() => self.create_and_open(&name),
            _ => {}
        }
    }

    fn switcher_rect(&self) -> Rect {
        let w = SWITCHER_W.min(self.width as f32 - 32.0);
        let n = self.switcher.as_ref().map_or(0, |s| s.choices.len() + usize::from(s.hint.is_some()));
        Rect {
            x: (self.width as f32 - w) / 2.0,
            y: BAND_H + 24.0,
            width: w,
            height: switcher_rows_top() + n as f32 * SWITCHER_ROW_H + 6.0,
        }
    }

    fn switcher_row_at(&self, x: f32, y: f32) -> Option<usize> {
        let r = self.switcher_rect();
        let top = r.y + switcher_rows_top();
        if x < r.x || x > r.x + r.width || y < top {
            return None;
        }
        let i = ((y - top) / SWITCHER_ROW_H) as usize;
        (i < self.switcher.as_ref()?.choices.len()).then_some(i)
    }

    // ---- rename ----------------------------------------------------------

    fn open_rename(&mut self) {
        let Some(cur) = self.current.clone() else { return };
        let shown = cur.strip_suffix(".md").unwrap_or(&cur).to_string();
        self.open_switcher(&shown);
        if let Some(sw) = self.switcher.as_mut() {
            sw.rename = Some(cur);
            sw.query = "\u{0}".into();
        }
        self.refresh_switcher();
    }

    /// Rename (or move) a note and rewrite every link to it, as Obsidian
    /// does with "automatically update internal links" on.
    fn rename(&mut self, from: &str, to: &str) {
        if self.current.as_deref() == Some(from) && !self.save() {
            return;
        }
        let Some(ix) = self.index.as_mut() else { return };
        match ix.rename(from, to) {
            Ok(plan) => {
                for h in &mut self.history {
                    if h == from {
                        *h = plan.to.clone();
                    }
                }
                let notes: std::collections::BTreeSet<&str> = plan.edits.iter().map(|e| e.path.as_str()).collect();
                let msg = format!(
                    "Renamed to {} · updated {} in {}",
                    plan.to,
                    side::plural(plan.edits.len(), "link"),
                    side::plural(notes.len(), "note")
                );
                if self.current.as_deref() == Some(from) {
                    // Its own self-links may have been rewritten too.
                    let to = plan.to.clone();
                    self.current = Some(to.clone());
                    instance::set_current(Some(&to));
                    if let Ok(text) = self.index.as_ref().map(|ix| ix.read_text(&to)).transpose().map(Option::unwrap_or_default) {
                        let scroll = self.read_scroll;
                        self.load_text(text);
                        self.read_scroll = scroll;
                    }
                    self.tree.reveal(&to);
                }
                self.rebuild_rows();
                self.side_dirty = true;
                self.layout = None;
                self.set_status(msg, false);
            }
            Err(e) => self.set_status(format!("Could not rename: {e}"), true),
        }
    }

    // ---- side panes ------------------------------------------------------

    fn refresh_side(&mut self) {
        self.side_dirty = false;
        let (Some(ix), Some(cur)) = (&self.index, self.current.clone()) else {
            self.side_panel.set(Vec::new(), true);
            self.side_key = None;
            return;
        };
        let items = match self.side_tab {
            SideTab::Backlinks => side::backlinks(ix, &cur),
            SideTab::Outline => side::outline(ix, &cur),
        };
        let key = (cur, self.side_tab);
        let reset = self.side_key.as_ref() != Some(&key);
        self.side_panel.set(items, reset);
        self.side_key = Some(key);
    }

    fn search_query(&self) -> String {
        if self.ui_context[self.search_input].editing {
            self.ui_context[self.search_input].edit_buffer.clone()
        } else {
            self.ui_context[self.search_input].text.clone()
        }
    }

    /// Rerun the search if the query changed, or always with `force`.
    fn refresh_search(&mut self, force: bool) {
        let q = self.search_query();
        if !force && q == self.search_seen {
            return;
        }
        let Some(ix) = &self.index else { return };
        let reset = q != self.search_seen;
        self.search_panel.set(side::search(ix, &q), reset);
        self.search_seen = q;
        self.needs_rebuild = true;
    }

    /// Show the search tab holding `query` (a tag click, Ctrl+Shift+F).
    fn open_search(&mut self, query: &str) {
        self.left_tab = LeftTab::Search;
        self.show_tree = true;
        self.ui_context[self.search_input].text = query.to_string();
        self.ui_context[self.search_input].edit_buffer = query.to_string();
        self.ui_context[self.search_input].cursor_idx = query.chars().count();
        self.ui_context[self.search_input].select_anchor = None;
        self.ui_context[self.search_input].sync_editor_state();
        self.ui_context.set_focused_id(self.search_input.id());
        WidgetHost::focus(&mut self.ui_context[self.search_input]);
        self.refresh_search(true);
        self.invalidate();
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Open { path, line } => self.open_path(&path, line, true),
            Action::Line(line) => self.goto_line(line),
        }
    }

    /// Bring a 0-based source line of the open note into view: scroll the
    /// reading view to its block, or put the editor's caret at its start.
    fn goto_line(&mut self, line: usize) {
        match self.mode {
            Mode::Reading => self.pending_line = Some(line),
            Mode::Source => {
                self.ui_context.unfocus_id(self.search_input.id());
                self.editor.reveal_line(line);
            }
        }
        self.needs_rebuild = true;
    }

    // ---- [[ completion -----------------------------------------------------

    /// The caret's line and its char index in it.
    fn caret_chars(&self) -> (usize, usize) {
        let p = self.editor.buf.caret;
        let line = self.editor.buf.line(p.line);
        (p.line, line[..p.col.min(line.len())].chars().count())
    }

    /// Ctrl+V with a picture (or copied image files) on the clipboard: save
    /// them in the vault's attachment folder for this note, as Obsidian
    /// does, and embed them at the caret, each on its own line. False when
    /// the clipboard holds nothing of the kind (the text paste runs); true,
    /// with a status, when it did not answer at all.
    fn paste_images(&mut self) -> bool {
        if self.index.is_none() || self.current.is_none() {
            return false;
        }
        let clip = match paste::read() {
            Ok(Some(clip)) => clip,
            Ok(None) => {
                log::debug!("paste: nothing to attach, pasting text");
                return false;
            }
            // Taken, not passed on: the text paste would wait on the same
            // silent clipboard, with no deadline of its own.
            Err(paste::Unanswered) => {
                self.set_status("The clipboard did not answer; nothing was pasted", true);
                return true;
            }
        };
        let (a, b) = self.editor.buf.selection().unwrap_or((self.editor.buf.caret, self.editor.buf.caret));
        self.attach(clip, a, b, "Pasted");
        true
    }

    /// Store `clip` in the attachment folder for the open note and put an
    /// embed for each file in place of `a..b`, each on its own line.
    /// `verb` names the action in the status line.
    fn attach(&mut self, clip: paste::Clip, a: Pos, b: Pos, verb: &str) {
        let (Some(ix), Some(cur)) = (self.index.as_ref(), self.current.clone()) else { return };
        let root = ix.root().to_path_buf();
        let folder = cce_vault::attachments::folder(&root, &cur);
        let written = match paste::store(clip, &root.join(&folder), chrono::Local::now().naive_local()) {
            Ok(w) => w,
            Err(e) => {
                self.set_status(format!("Could not save the image: {e}"), true);
                return;
            }
        };
        // The bare name, as Obsidian writes it — unless another file of that
        // name would win the link, then the vault path. (The index learns of
        // the new file from the watcher, after this.)
        let embeds: Vec<String> = written
            .iter()
            .filter_map(|p| {
                let rel = ix.rel(p)?;
                let name = p.file_name()?.to_string_lossy().into_owned();
                let link = if ix.resolve_text(Some(&cur), &name).is_some_and(|other| other != rel) { rel } else { name };
                Some(format!("![[{link}]]"))
            })
            .collect();
        if embeds.is_empty() {
            return;
        }
        let before = self.editor.buf.line(a.line)[..a.col].to_string();
        let after = self.editor.buf.line(b.line)[b.col..].to_string();
        let (text, at) = paste::insertion(&embeds, &before, &after);
        let head = &text[..at];
        let caret = match head.rfind('\n') {
            Some(nl) => Pos::new(a.line + head.matches('\n').count(), head.len() - nl - 1),
            None => Pos::new(a.line, a.col + head.len()),
        };
        self.editor.edit(a, b, &text, caret);
        let n = written.len();
        let place = if folder.is_empty() { "the vault".to_string() } else { folder };
        self.set_status(format!("{verb} {n} image{} into {place}", if n == 1 { "" } else { "s" }), false);
    }

    /// A drop on the window: images (pixels from a browser, or image files
    /// from a file manager) become attachments embedded after the line they
    /// were dropped on. In reading view the note switches to editing and they go at
    /// its end.
    fn dropped(&mut self, mime: &str, data: &[u8], x: f32, y: f32) {
        if self.index.is_none() || self.current.is_none() {
            self.set_status("Open a note to drop images into it", true);
            return;
        }
        let Some(clip) = paste::from_drop(mime, data) else {
            self.set_status("Only images can be dropped into a note", true);
            return;
        };
        let at = if self.mode == Mode::Source && self.editor_rect(&self.metrics()).contains(x, y) {
            // After the line under the pointer: dropping onto a word must
            // not split it around the picture.
            let line = self.editor.pos_at(x, y).line;
            Pos::new(line, self.editor.buf.line(line).len())
        } else {
            self.set_mode(Mode::Source);
            let last = self.editor.buf.line_count() - 1;
            Pos::new(last, self.editor.buf.line(last).len())
        };
        self.attach(clip, at, at, "Dropped");
    }

    /// Open, update or close the completion for the caret's position.
    fn update_completion(&mut self) {
        if !self.editor_focused() || self.editor.buf.selection().is_some() {
            self.completion = None;
            return;
        }
        let (line, col) = self.caret_chars();
        let found = complete::open_link(self.editor.buf.line(line), col);
        let Some((start, query)) = found else {
            self.completion = None;
            self.completion_dismissed = None;
            return;
        };
        if self.completion_dismissed == Some((line, start)) {
            return;
        }
        if self.completion.as_ref().is_some_and(|c| c.line == line && c.start == start && c.query == query) {
            return;
        }
        let Some(ix) = &self.index else { return };
        let choices = complete::choices(ix, &query);
        self.completion = Some(Completion { line, start, query, choices, selected: 0 });
        self.needs_rebuild = true;
    }

    /// Keys the open completion takes; true when it took this one.
    fn completion_key(&mut self, key: &Key) -> bool {
        let Some(c) = self.completion.as_mut() else { return false };
        if c.choices.is_empty() {
            return false;
        }
        let n = c.choices.len();
        match key {
            Key::Named(NamedKey::ArrowDown) => c.selected = (c.selected + 1) % n,
            Key::Named(NamedKey::ArrowUp) => c.selected = (c.selected + n - 1) % n,
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => self.accept_completion(),
            Key::Named(NamedKey::Escape) => {
                self.completion_dismissed = Some((c.line, c.start));
                self.completion = None;
            }
            _ => return false,
        }
        self.needs_rebuild = true;
        true
    }

    /// Write the chosen link into the caret's line: one undo step.
    fn accept_completion(&mut self) {
        let Some(c) = self.completion.take() else { return };
        let (Some(ix), Some(path)) = (&self.index, c.choices.get(c.selected)) else { return };
        let link = complete::link_text(ix, self.current.as_deref(), path);
        let (line, col) = self.caret_chars();
        if line != c.line {
            return;
        }
        let text = self.editor.buf.line(line).to_string();
        let (next, caret) = complete::apply(&text, c.start, col, &link);
        let caret_byte = next.char_indices().nth(caret).map_or(next.len(), |(b, _)| b);
        self.editor.edit(Pos::new(line, 0), Pos::new(line, text.len()), &next, Pos::new(line, caret_byte));
    }

    /// The completion popup's rect, under the caret and inside the note
    /// pane. Valid after the editor is prepared for this frame.
    fn completion_rect(&mut self, m: &Metrics) -> Option<Rect> {
        let n = self.completion.as_ref().filter(|c| !c.choices.is_empty())?.choices.len();
        let r = self.editor_rect(m);
        let caret = self.editor.caret_rect();
        let h = n as f32 * COMPLETE_ROW_H + 8.0;
        let w = COMPLETE_W.min(r.width);
        let x = caret.x.min(r.x + r.width - w).max(r.x);
        // Below the caret line, or above it when there is no room below.
        let below = caret.y + caret.height + 2.0;
        let y = if below + h <= r.y + r.height { below } else { (caret.y - h - 2.0).max(r.y) };
        Some(Rect { x, y, width: w, height: h })
    }

    fn completion_row_at(&mut self, m: &Metrics, x: f32, y: f32) -> Option<usize> {
        let r = self.completion_rect(m)?;
        if !r.contains(x, y) {
            return None;
        }
        let i = ((y - r.y - 4.0) / COMPLETE_ROW_H).floor();
        (i >= 0.0 && (i as usize) < self.completion.as_ref()?.choices.len()).then_some(i as usize)
    }

    // ---- layout and scrolling -------------------------------------------

    fn ensure_layout(&mut self, width: f32) {
        if matches!(&self.layout, Some((w, _)) if (w - width).abs() < 0.5) {
            return;
        }
        let m = self.measure.get_or_insert_with(|| ShapingMeasure::new(true));
        let index = &self.index;
        let cur = self.current.as_deref();
        let resolved = |l: &SpanLink| match l {
            SpanLink::Note { target, .. } | SpanLink::Embed { target, .. } => {
                target.is_empty() || index.as_ref().is_some_and(|ix| ix.resolve_text(cur, target).is_some())
            }
            _ => true,
        };
        let images = &self.images;
        let laid = reading::layout_with(&self.blocks, width, &self.theme, m, &resolved, &|t| images.lookup(t));
        self.layout = Some((width, laid));
    }

    fn read_max_scroll(&self, m: &Metrics) -> f32 {
        let h = self.layout.as_ref().map_or(0.0, |(_, l)| l.height);
        (h + 2.0 * READ_PAD - m.note.height).max(0.0)
    }

    fn tree_max_scroll(&self, m: &Metrics) -> f32 {
        (self.rows.len() as f32 * TREE_ROW_H + 8.0 - m.tree_body.height).max(0.0)
    }

    fn tree_row_at(&self, m: &Metrics, x: f32, y: f32) -> Option<usize> {
        if self.left_tab != LeftTab::Files || !m.tree_body.contains(x, y) {
            return None;
        }
        let i = ((y - m.tree_body.y - 4.0 + self.tree_scroll) / TREE_ROW_H).floor();
        (i >= 0.0 && (i as usize) < self.rows.len()).then_some(i as usize)
    }

    /// The reading view's click target under a window point.
    fn reading_hit(&self, x: f32, y: f32) -> Option<Hit> {
        let m = self.metrics();
        if self.mode != Mode::Reading || self.switcher.is_some() || !m.note.contains(x, y) {
            return None;
        }
        let (ox, oy, _) = self.reading_frame(&m);
        let (_, l) = self.layout.as_ref()?;
        l.hit(x - ox, y - oy + self.read_scroll).cloned()
    }

    // ---- painting --------------------------------------------------------

    fn paint_band(&self, pc: &mut PaintCtx, m: &Metrics) {
        let w = self.width as f32;
        pc.recess_edges(
            Rect { x: 0.0, y: 0.0, width: w, height: BAND_H },
            (0.0, 0.0, 0.0, 0.0),
            cce_ui::layout::bar_wall_width(),
            (false, false, true, false),
        );
        let (family, size) = cce_ui::layout::menubar_font_parsed();
        let inset = cce_ui::layout::root_plate_inset();
        let ty = cce_ui::layout::align_text_y(0.0, BAND_H, size, 0.0);
        let right = m.mode_chip.x - 12.0;
        match &self.current {
            Some(cur) => {
                let (dir, file) = match cur.rsplit_once('/') {
                    Some((d, f)) => (format!("{d} / "), f),
                    None => (String::new(), cur.as_str()),
                };
                let name = file.strip_suffix(".md").unwrap_or(file);
                let bounds = Some([inset, 0.0, right, BAND_H]);
                let dir_w = dir.chars().count() as f32 * size * 0.6;
                let name_w = name.chars().count() as f32 * size * 0.6;
                pc.text_with(dir, inset, ty, size, srgb_u8(DIM), Some(family.clone()), bounds);
                pc.text_with(name.to_string(), inset + dir_w, ty, size, srgb_u8(FG), Some(family.clone()), bounds);
                if self.dirty() {
                    // Unsaved: a small dot after the name.
                    let side = (size * 0.45).round().max(4.0);
                    let dot = Rect { x: inset + dir_w + name_w + size * 0.5, y: (BAND_H - side) / 2.0, width: side, height: side };
                    if dot.x + dot.width <= right {
                        pc.icon("circle", dot, cce_ui::colors::to_srgb(FG));
                    }
                }
            }
            None => {
                let vault = self.index.as_ref().map(|ix| vault_name(ix.root())).unwrap_or_else(|| "Notes".into());
                pc.text_with(vault, inset, ty, size, srgb_u8(FG), Some(family.clone()), None);
            }
        }
        if self.current.is_some() {
            let label = match self.mode {
                Mode::Reading => "Reading",
                Mode::Source if self.editor.preview => "Editing",
                Mode::Source => "Source",
            };
            let c = m.mode_chip;
            pc.rounded_rect(c, 6.0, (true, true, true, true), [1.0, 1.0, 1.0, 0.06]);
            let lw = label.len() as f32 * size * 0.6;
            pc.text_with(
                label.to_string(),
                c.x + (c.width - lw) / 2.0,
                cce_ui::layout::align_text_y(c.y, c.height, size, 0.0),
                size,
                srgb_u8(DIM),
                Some(family),
                Some([c.x, c.y, c.x + c.width, c.y + c.height]),
            );
        }
    }

    fn paint_status(&self, pc: &mut PaintCtx) {
        let (w, h) = (self.width as f32, self.height as f32);
        let y = h - STATUS_H;
        pc.recess_edges(
            Rect { x: 0.0, y, width: w, height: STATUS_H },
            (0.0, 0.0, 0.0, 0.0),
            cce_ui::layout::bar_wall_width(),
            (true, false, false, false),
        );
        let (family, size) = cce_ui::layout::statusbar_font_parsed();
        let inset = cce_ui::layout::root_plate_inset();
        let ty = cce_ui::layout::align_text_y(y, STATUS_H, size, 0.0);
        let (msg, color) = if self.conflict {
            (self.conflict_hint().to_string(), CONFLICT)
        } else if let Some((msg, err)) = &self.status {
            (msg.clone(), if *err { CONFLICT } else { DIM })
        } else if let (Some(cur), Some(ix)) = (&self.current, &self.index) {
            let words = if self.mode == Mode::Source {
                self.editor.buf.lines().iter().map(|l| l.split_whitespace().count()).sum()
            } else {
                self.saved.split_whitespace().count()
            };
            // Notes linking here, as the backlinks pane counts them.
            let back = ix.backlinks(cur).iter().filter(|b| b.source != cur).map(|b| b.source).collect::<std::collections::HashSet<_>>().len();
            (format!("{} · {}", plural(words, "word"), plural(back, "backlink")), DIM)
        } else {
            (String::new(), DIM)
        };
        pc.text_with(msg, inset, ty, size, srgb_u8(color), Some(family.clone()), Some([inset, y, w * 0.75, h]));
        if let Some(ix) = &self.index {
            let name = vault_name(ix.root());
            let nw = name.chars().count() as f32 * size * 0.6;
            pc.text_with(name, w - inset - nw, ty, size, srgb_u8(DIM), Some(family), None);
        }
    }

    fn paint_tree(&self, pc: &mut PaintCtx, m: &Metrics) {
        if m.tree.width <= 0.0 {
            return;
        }
        pc.recess_edges(m.tree, (0.0, 0.0, 0.0, 0.0), cce_ui::layout::bar_wall_width(), (false, true, false, false));
        let active = if self.left_tab == LeftTab::Files { 0 } else { 1 };
        paint_tabs(pc, m.left_tabs, &["Files", "Search"], active);
        if self.left_tab == LeftTab::Search {
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.ui_context[self.search_input], pc);
            if self.search_panel.items.is_empty() {
                let (family, size) = cce_ui::layout::tree_font_parsed();
                let b = m.tree_body;
                pc.text_with(
                    "Words, or #tag",
                    b.x + 12.0,
                    b.y + 10.0,
                    (size * 0.9).round(),
                    srgb_u8(DIM),
                    Some(family),
                    Some([b.x, b.y, b.x + b.width, b.y + b.height]),
                );
            }
            self.search_panel.paint(pc, m.tree_body);
            return;
        }
        let t = m.tree_body;
        let (family, size) = cce_ui::layout::tree_font_parsed();
        pc.clip(t, |pc| {
            for (i, row) in self.rows.iter().enumerate() {
                let y = t.y + 4.0 + i as f32 * TREE_ROW_H - self.tree_scroll;
                if y + TREE_ROW_H < t.y || y > t.y + t.height {
                    continue;
                }
                let r = Rect { x: t.x + 4.0, y, width: t.width - 10.0, height: TREE_ROW_H };
                if !row.folder && self.current.as_deref() == Some(row.path.as_str()) {
                    pc.rounded_rect(r, 4.0, (true, true, true, true), ROW_CURRENT);
                } else if self.tree_hover == Some(i) {
                    pc.rounded_rect(r, 4.0, (true, true, true, true), ROW_HOVER);
                }
                let x = r.x + 8.0 + row.depth as f32 * 14.0;
                let ty = cce_ui::layout::align_text_y(r.y, r.height, size, 0.0);
                // A folder's disclosure chevron sits in the gutter every
                // row's name is indented past, so files line up with it.
                let gutter = (size * 1.2).round();
                let color = if row.folder {
                    let side = (size * 0.9).round();
                    let chevron = Rect { x, y: r.y + (r.height - side) / 2.0, width: side, height: side };
                    pc.icon(if row.open { "chevron-down" } else { "chevron-right" }, chevron, cce_ui::colors::to_srgb(DIM));
                    DIM
                } else {
                    FG
                };
                pc.text_with(
                    row.name.clone(),
                    x + gutter,
                    ty,
                    size,
                    srgb_u8(color),
                    Some(family.clone()),
                    Some([r.x, r.y.max(t.y), r.x + r.width, (r.y + r.height).min(t.y + t.height)]),
                );
            }
        });
    }

    fn paint_note(&mut self, pc: &mut PaintCtx, m: &Metrics) {
        if self.switcher.is_some() {
            // The switcher covers the note; text cannot be hidden under a
            // plate, so the note is not painted at all while it is open.
            return;
        }
        if self.index.is_none() {
            let msg = self.vault_error.clone().unwrap_or_default();
            let (family, size) = cce_ui::layout::list_font_parsed();
            let r = m.note;
            for (i, line) in msg.lines().enumerate() {
                pc.text_with(
                    line.to_string(),
                    r.x + READ_PAD,
                    r.y + READ_PAD + i as f32 * size * 1.6,
                    size,
                    srgb_u8(if i == 0 { FG } else { DIM }),
                    Some(family.clone()),
                    None,
                );
            }
            return;
        }
        if self.current.is_none() {
            let (family, size) = cce_ui::layout::list_font_parsed();
            let hint = "Ctrl+O to open a note · Alt+D for today's note";
            let hw = hint.chars().count() as f32 * size * 0.6;
            pc.text_with(
                hint,
                m.note.x + (m.note.width - hw) / 2.0,
                m.note.y + m.note.height / 2.0,
                size,
                srgb_u8(DIM),
                Some(family),
                None,
            );
            return;
        }
        match self.mode {
            Mode::Source => {
                let e = self.editor_rect(m);
                let focused = self.editor_focused();
                self.editor.prepare(e);
                let hole = self.completion_rect(m);
                let (index, cur) = (&self.index, self.current.as_deref());
                let resolved = |t: &doc_editor::Target| match t {
                    doc_editor::Target::Note { target, .. } => {
                        target.is_empty() || index.as_ref().is_some_and(|ix| ix.resolve_text(cur, target).is_some())
                    }
                    doc_editor::Target::Url(_) => true,
                };
                match hole {
                    // Text draws over every plate, so the popup cannot simply
                    // cover the editor: paint the editor four times, clipped
                    // to the bands around the popup, and the popup into the
                    // hole.
                    Some(hole) => {
                        let (l, t, r, b) = (e.x, e.y, e.x + e.width, e.y + e.height);
                        let bands = [
                            Rect { x: l, y: t, width: r - l, height: hole.y - t },
                            Rect { x: l, y: hole.y + hole.height, width: r - l, height: b - hole.y - hole.height },
                            Rect { x: l, y: hole.y, width: hole.x - l, height: hole.height },
                            Rect { x: hole.x + hole.width, y: hole.y, width: r - hole.x - hole.width, height: hole.height },
                        ];
                        let ed = &mut self.editor;
                        for band in bands.into_iter().filter(|b| b.width > 0.0 && b.height > 0.0) {
                            pc.clip(band, |pc| ed.paint_prepared_with(pc, focused, &resolved));
                        }
                        self.paint_completion(pc, hole);
                    }
                    None => self.editor.paint_prepared_with(pc, focused, &resolved),
                }
            }
            Mode::Reading => {
                let (ox, oy, width) = self.reading_frame(m);
                self.ensure_layout(width);
                if let Some(line) = self.pending_line.take() {
                    let max = self.read_max_scroll(m);
                    let y = self.layout.as_ref().map_or(0.0, |(_, l)| l.y_of_line(line));
                    self.read_scroll = y.clamp(0.0, max);
                    self.read_motion.y.jump_to(self.read_scroll);
                }
                let note = m.note;
                if let Some((_, l)) = &self.layout {
                    let images = &self.images;
                    pc.clip(note, |pc| l.paint_with(pc, (ox, oy), self.read_scroll, note, &|t| images.lookup(t)));
                }
            }
        }
    }

    fn paint_switcher(&self, pc: &mut PaintCtx) {
        let Some(sw) = &self.switcher else { return };
        let r = self.switcher_rect();
        pc.rounded_rect(r, 10.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_BG);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.ui_context[self.switcher_input], pc);
        let (family, size) = cce_ui::layout::list_font_parsed();
        let top = r.y + switcher_rows_top();
        for (i, c) in sw.choices.iter().enumerate() {
            let row = Rect { x: r.x + 6.0, y: top + i as f32 * SWITCHER_ROW_H, width: r.width - 12.0, height: SWITCHER_ROW_H };
            if i == sw.selected {
                pc.rounded_rect(row, 5.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_HOVER);
            }
            let (label, color, alias) = match c {
                Choice::Note { path, alias } => (path.trim_end_matches(".md").to_string(), FG, alias.as_deref()),
                Choice::Create(name) => (format!("Create “{name}”"), DIM, None),
            };
            let ty = cce_ui::layout::align_text_y(row.y, row.height, size, 0.0);
            let bounds = Some([row.x, row.y, row.x + row.width - 6.0, row.y + row.height]);
            let lw = label.chars().count() as f32 * size * 0.6;
            pc.text_with(label, row.x + 10.0, ty, size, srgb_u8(color), Some(family.clone()), bounds);
            if let Some(alias) = alias {
                // "path ← alias": the alias the query matched, behind an
                // arrow glyph pointing back at the note it names.
                let side = (size * 0.9).round();
                let ax = row.x + 10.0 + lw + size * 0.6;
                let arrow = Rect { x: ax, y: row.y + (row.height - side) / 2.0, width: side, height: side };
                if arrow.x + arrow.width < row.x + row.width - 6.0 {
                    pc.icon("arrow-left", arrow, cce_ui::colors::to_srgb(DIM));
                }
                let tx = ax + side + size * 0.4;
                pc.text_with(alias.to_string(), tx, ty, size, srgb_u8(DIM), Some(family.clone()), bounds);
            }
        }
        if let Some((hint, err)) = &sw.hint {
            let y = top + sw.choices.len() as f32 * SWITCHER_ROW_H;
            let row = Rect { x: r.x + 6.0, y, width: r.width - 12.0, height: SWITCHER_ROW_H };
            pc.text_with(
                hint.clone(),
                row.x + 10.0,
                cce_ui::layout::align_text_y(row.y, row.height, size, 0.0),
                size,
                srgb_u8(if *err { CONFLICT } else { DIM }),
                Some(family.clone()),
                Some([row.x, row.y, row.x + row.width - 6.0, row.y + row.height]),
            );
        }
    }

    fn paint_side(&self, pc: &mut PaintCtx, m: &Metrics) {
        if m.side.width <= 0.0 {
            return;
        }
        pc.recess_edges(m.side, (0.0, 0.0, 0.0, 0.0), cce_ui::layout::bar_wall_width(), (false, false, false, true));
        let active = if self.side_tab == SideTab::Backlinks { 0 } else { 1 };
        paint_tabs(pc, m.side_tabs, &["Backlinks", "Outline"], active);
        self.side_panel.paint(pc, m.side_body);
    }

    fn paint_completion(&self, pc: &mut PaintCtx, r: Rect) {
        let Some(c) = &self.completion else { return };
        pc.rounded_rect(r, 8.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_BG);
        let (family, size) = cce_ui::layout::list_font_parsed();
        for (i, path) in c.choices.iter().enumerate() {
            let row = Rect { x: r.x + 4.0, y: r.y + 4.0 + i as f32 * COMPLETE_ROW_H, width: r.width - 8.0, height: COMPLETE_ROW_H };
            if i == c.selected {
                pc.rounded_rect(row, 5.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_HOVER);
            }
            let (dir, file) = match path.rsplit_once('/') {
                Some((d, f)) => (Some(d), f),
                None => (None, path.as_str()),
            };
            let name = file.strip_suffix(".md").unwrap_or(file);
            let ty = cce_ui::layout::align_text_y(row.y, row.height, size, 0.0);
            let bounds = Some([row.x, row.y, row.x + row.width - 6.0, row.y + row.height]);
            pc.text_with(name.to_string(), row.x + 8.0, ty, size, srgb_u8(FG), Some(family.clone()), bounds);
            if let Some(d) = dir {
                let nx = row.x + 8.0 + (name.chars().count() as f32 + 2.0) * size * 0.6;
                pc.text_with(d.to_string(), nx, ty, size, srgb_u8(DIM), Some(family.clone()), bounds);
            }
        }
    }
}

/// Spawn `cmd` and reap it on a background thread, so the child never lingers
/// as a zombie once it exits. The same helper cce-mail, cce-files, cce-terminal
/// and cce-system-interface each keep; cce-ui's shared `process::spawn_detached`
/// went away in cce-ui 4e94236.
fn spawn_detached(mut cmd: std::process::Command) -> std::io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Equal-width text tabs across `r`, the active one underlined.
fn paint_tabs(pc: &mut PaintCtx, r: Rect, labels: &[&str], active: usize) {
    if r.width <= 0.0 {
        return;
    }
    let (family, size) = cce_ui::layout::tree_font_parsed();
    let w = r.width / labels.len() as f32;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect { x: r.x + i as f32 * w, y: r.y, width: w, height: r.height };
        let lw = label.chars().count() as f32 * size * 0.6;
        let color = if i == active { FG } else { DIM };
        pc.text_with(
            label.to_string(),
            cell.x + ((cell.width - lw) / 2.0).max(4.0),
            cce_ui::layout::align_text_y(cell.y, cell.height, size, 0.0),
            size,
            srgb_u8(color),
            Some(family.clone()),
            Some([cell.x, cell.y, cell.x + cell.width, cell.y + cell.height]),
        );
        if i == active {
            let u = Rect { x: cell.x + 14.0, y: cell.y + cell.height - 3.0, width: (cell.width - 28.0).max(4.0), height: 2.0 };
            pc.rounded_rect(u, 1.0, (true, true, true, true), cce_ui::colors::to_linear([0.66, 0.55, 0.98, 1.0]));
        }
    }
}

/// The switcher's choice rows start under its input: a 12px inset, the
/// textbox, an 8px gap.
fn switcher_rows_top() -> f32 {
    12.0 + cce_ui::layout::textbox_height() + 8.0
}

fn tab_at(r: Rect, n: usize, x: f32, y: f32) -> Option<usize> {
    if r.width <= 0.0 || !r.contains(x, y) {
        return None;
    }
    Some((((x - r.x) / (r.width / n as f32)) as usize).min(n - 1))
}

/// The vault path a rename prompt's text names: `.md` added unless it
/// names another extension the note already had. `None` while empty.
fn rename_target(from: &str, query: &str) -> Option<String> {
    let q = query.trim().trim_start_matches('/').trim_end_matches('/');
    if q.is_empty() {
        return None;
    }
    let keeps_ext = !from.to_lowercase().ends_with(".md") && from.rsplit('.').next() == q.rsplit('.').next();
    if q.to_lowercase().ends_with(".md") || keeps_ext {
        Some(q.to_string())
    } else {
        Some(format!("{q}.md"))
    }
}


use side::plural;

fn vault_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string())
}

impl Application for NotesApp {
    type Message = Message;

    fn ui_context(&self) -> Option<&cce_ui::context::UiContext> {
        Some(&self.ui_context)
    }

    /// Tab accepts the editor's link completion, as Enter does. The toolkit's Tab walk (on by
    /// default since 2026-10-08) would take it first.
    fn plate_navigation(&self) -> bool {
        false
    }

    fn ui_context_mut(&mut self) -> Option<&mut cce_ui::context::UiContext> {
        Some(&mut self.ui_context)
    }

    fn create(sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        // The app keeps calloop's sender; `AppSender` converts into it.
        let sender: calloop::channel::Sender<Self::Message> = sender.into();
        let startup = STARTUP.get().expect("startup set in main");
        let (index, vault_error, watcher) = match &startup.vault {
            Ok(root) => match Index::open(root, true) {
                Ok(mut ix) => {
                    if let Err(e) = ix.save_cache() {
                        log::warn!("vault cache: {e}");
                    }
                    let tx = sender.clone();
                    let watcher = VaultWatcher::spawn(root, move |paths| {
                        let _ = tx.send(Message::VaultChanged(paths));
                    })
                    .map_err(|e| log::warn!("vault watcher: {e}"))
                    .ok();
                    (Some(ix), None, watcher)
                }
                Err(e) => (None, Some(format!("Could not read the vault at {}\n{e}", root.display())), None),
            },
            Err(e) => (None, Some(e.clone()), None),
        };
        // Only the instance serves MCP: a second copy running standalone
        // would fight it for the port.
        if instance::spawn_listener(sender.clone()) && index.is_some() {
            instance::set_vault(index.as_ref().map(|ix| ix.root()));
            mcp::start(sender.clone());
        }
        let _ = sender.send(Message::Command(startup.command.clone()));
        NotesApp::new(index, vault_error, watcher, sender)
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Notes".to_string(),
            app_id: "cce-notes".to_string(),
            width: 1000,
            height: 700,
            fullscreen: false,
            min_size: Some((480, 320)),
        }
    }

    fn update(&mut self, msg: Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            Message::Command(cmd) => self.run_command(cmd),
            Message::VaultChanged(paths) => self.vault_changed(paths),
            Message::Mcp(call) => self.mcp_call(call),
            Message::ImageDecoded(path, decoded) => {
                if self.images.decoded(path, decoded) {
                    self.layout = None;
                    self.editor.invalidate();
                }
            }
            Message::ImagesReady => {
                self.layout = None;
                self.editor.invalidate();
            }
            Message::Exit => *_exit = true,
        }
        *needs_rebuild = true;
    }

    fn idle_poll_interval(&self) -> Option<Duration> {
        // Wakes the autosave check while the editor holds unsaved text.
        self.dirty().then_some(Duration::from_millis(500))
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.ui_context.tick(dt) {
            *needs_rebuild = true;
        }
        let m = self.metrics();
        if self.read_motion.is_animating() {
            let max = self.read_max_scroll(&m);
            self.read_motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
            self.read_scroll = self.read_motion.y.pos();
            *needs_rebuild = true;
        }
        if self.tree_motion.is_animating() {
            let max = self.tree_max_scroll(&m);
            self.tree_motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
            self.tree_scroll = self.tree_motion.y.pos();
            *needs_rebuild = true;
        }
        if self.search_panel.tick(dt, m.tree_body) | self.side_panel.tick(dt, m.side_body) {
            *needs_rebuild = true;
        }
        if self.mode == Mode::Source && self.editor.tick(dt) {
            *needs_rebuild = true;
        }
        if self.dirty() && !self.conflict {
            let now = Instant::now();
            if self.editor.buf.revision != self.edit_seen {
                self.edit_seen = self.editor.buf.revision;
                self.edit_changed_at = now;
            } else if now.duration_since(self.edit_changed_at) >= AUTOSAVE_AFTER {
                self.save();
                *needs_rebuild = true;
            }
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        let size_changed =
            self.width != size.width as u32 || self.height != size.height as u32 || self.scale != scale;
        if size_changed {
            if self.scale != scale {
                self.layout = None;
                self.editor.invalidate();
            }
            self.width = size.width as u32;
            self.height = size.height as u32;
            self.scale = scale;
            cce_ui::scale::set_scale_factor(scale as f32);
        }
        let m = self.metrics();
        if self.needs_rebuild || size_changed {
            let r = self.switcher_rect();
            self.ui_context[self.switcher_input].set_rect(r.x + 12.0, r.y + 12.0, r.width - 24.0, cce_ui::layout::textbox_height());
            let sb = m.search_box;
            self.ui_context[self.search_input].set_rect(sb.x, sb.y, sb.width, sb.height);
            self.needs_rebuild = false;
            self.ui_context.rebuild_spatial_grid();
            // Keep scroll offsets in range after a resize or a new layout.
            self.tree_scroll = self.tree_scroll.min(self.tree_max_scroll(&m));
            self.search_panel.clamp(m.tree_body);
            self.side_panel.clamp(m.side_body);
        }
        if self.side_dirty && m.side.width > 0.0 {
            self.refresh_side();
        }

        let (w, h) = (self.width as f32, self.height as f32);
        let mut pc = PaintCtx::new();
        pc.root_plate(w, h);
        self.paint_band(&mut pc, &m);
        if self.switcher.is_none() {
            self.paint_tree(&mut pc, &m);
            self.paint_side(&mut pc, &m);
        }
        self.paint_note(&mut pc, &m);
        let max = self.read_max_scroll(&m);
        if self.read_scroll > max {
            self.read_scroll = max;
            self.read_motion.y.jump_to(max);
        }
        self.paint_status(&mut pc);
        self.paint_switcher(&mut pc);
        // What this frame asked for and nobody has: resolve and decode it.
        if let Some(ix) = &self.index {
            let tx = self.sender.clone();
            let ready = self.images.pump(ix, self.current.as_deref(), move |path, d| {
                let _ = tx.send(Message::ImageDecoded(path, d));
            });
            if ready {
                // Through the loop, so a frame follows.
                let _ = self.sender.send(Message::ImagesReady);
            }
        }
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    /// Pixels first (a browser's dragged picture); a file manager's drag
    /// offers only the file list.
    fn drop_mimes(&self) -> &'static [&'static str] {
        paste::DROP_MIMES
    }

    fn handle_drop(&mut self, mime: &str, data: &[u8], pos: LogicalPosition, needs_rebuild: &mut bool) {
        self.dropped(mime, data, pos.x, pos.y);
        *needs_rebuild = true;
    }

    /// A reconnect builds a new renderer and every uploaded image id dies
    /// with the old one: forget them, and the next frame uploads afresh.
    /// The first renderer needs nothing (uploads queued before it drain
    /// into it).
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        if std::mem::replace(&mut self.seen_renderer, true) {
            self.images.renderer_reset();
            self.layout = None;
            self.editor.invalidate();
        }
    }

    /// Ctrl+Z reaches the editor through here: the runner routes the undo
    /// chord to the focused widget first, and the editor is not one.
    fn undo(&mut self, needs_rebuild: &mut bool) -> bool {
        if !self.editor_focused() {
            return false;
        }
        self.editor.undo();
        self.update_completion();
        *needs_rebuild = true;
        true
    }

    fn redo(&mut self, needs_rebuild: &mut bool) -> bool {
        if !self.editor_focused() {
            return false;
        }
        self.editor.redo();
        self.update_completion();
        *needs_rebuild = true;
        true
    }

    /// The reading view needs bold and italic faces of the DE's sans
    /// (Noto Sans by default), which the toolkit's bundle does not carry:
    /// without system fonts, `**bold**` fell back to a serif face. The
    /// measuring FontSystem loads the same set ([`NotesApp::measure_fs`]).
    fn load_system_fonts(&self) -> bool {
        true
    }

    fn cursor_icon(&self, x: f32, y: f32) -> Option<CursorIcon> {
        let m = self.metrics();
        let over_title = self.current.is_some() && self.switcher.is_none() && m.title.contains(x, y);
        if self.hover_hit
            || self.tree_hover.is_some()
            || self.search_panel.hover.is_some()
            || self.side_panel.hover.is_some()
            || over_title
        {
            return Some(CursorIcon::Pointer);
        }
        if self.left_tab == LeftTab::Search && m.search_box.contains(x, y) {
            return Some(CursorIcon::Text);
        }
        (self.mode == Mode::Source && self.switcher.is_none() && self.editor_rect(&m).contains(x, y))
            .then_some(CursorIcon::Text)
    }

    fn on_exit(&mut self) {
        if !self.conflict {
            self.save();
        } else if self.dirty() {
            // A conflict cannot be asked about on the way out, and neither
            // copy may silently win: keep ours beside the note.
            match self.write_conflict_copy() {
                Ok(path) => log::warn!("unsaved edits kept in {path} (the note changed on disk)"),
                Err(e) => log::error!("could not keep the unsaved edits: {e}"),
            }
        }
        if let Some(ix) = self.index.as_mut() {
            let _ = ix.save_cache();
        }
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (x, y) = (pos.x, pos.y);
        self.pointer = (x, y);
        // The shared context menu (the switcher's or the search box's) gets the
        // pointer to itself while open, over the switcher too: its row highlight.
        if cce_ui::widget::context_menu::is_visible() {
            if cce_ui::widget::context_menu::cursor_moved(x, y) {
                *needs_rebuild = true;
            }
            return;
        }
        let ev = Event::PointerMove { x, y, local_x: x, local_y: y };
        if self.switcher.is_some() {
            let row = self.switcher_row_at(x, y);
            if let (Some(sw), Some(i)) = (self.switcher.as_mut(), row) {
                if sw.selected != i {
                    sw.selected = i;
                    *needs_rebuild = true;
                }
            }
            if self.ui_context.propagate_event(&ev, self.switcher_input.id()) {
                *needs_rebuild = true;
            }
            return;
        }
        let m = self.metrics();
        let hover = self.tree_row_at(&m, x, y);
        if hover != self.tree_hover {
            self.tree_hover = hover;
            *needs_rebuild = true;
        }
        let search_area = if self.left_tab == LeftTab::Search { m.tree_body } else { Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 } };
        if self.search_panel.hover_at(search_area, x, y) | self.side_panel.hover_at(m.side_body, x, y) {
            *needs_rebuild = true;
        }
        if self.left_tab == LeftTab::Search && self.ui_context.propagate_event(&ev, self.search_input.id()) {
            *needs_rebuild = true;
        }
        let mut hit = self.reading_hit(x, y).is_some();
        if self.mode == Mode::Source {
            if self.editor.dragging() {
                if self.editor.drag(x, y) != doc_editor::Response::None {
                    self.update_completion();
                    *needs_rebuild = true;
                }
            } else if m.note.contains(x, y) {
                hit = self.editor.link_at(x, y);
            }
        }
        if hit != self.hover_hit {
            self.hover_hit = hit;
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Message> {
        let (x, y) = (pos.x, pos.y);
        *needs_rebuild = true;

        // The shared context menu a right-click on the switcher or search box
        // opens takes every click while open: a row runs, a press anywhere else
        // dismisses it -- and only it, so the press that closes the menu does
        // not also close the switcher beneath. The toolkit leaves this routing to
        // the app; without it the menu could not be closed by clicking outside
        // it, and its rows did nothing. A Paste or Cut is a new query.
        if cce_ui::widget::context_menu::is_visible() {
            cce_ui::widget::context_menu::mouse_input(button, state, x, y, Some(&mut self.ui_context));
            if self.switcher.is_some() {
                self.refresh_switcher();
            } else {
                self.refresh_search(false);
            }
            return None;
        }

        let ev = Event::MouseButton { button, state, x, y, local_x: x, local_y: y };
        let pressed = state == ElementState::Pressed && button == MouseButton::Left;

        if self.switcher.is_some() {
            if pressed {
                if let Some(i) = self.switcher_row_at(x, y) {
                    if let Some(sw) = self.switcher.as_mut() {
                        sw.selected = i;
                    }
                    self.switcher_pick(false);
                    return None;
                }
                if !self.switcher_rect().contains(x, y) {
                    self.close_switcher();
                    return None;
                }
            }
            self.ui_context.propagate_event(&ev, self.switcher_input.id());
            return None;
        }

        let m = self.metrics();
        // The search box keeps the keyboard only while it is clicked into.
        let in_search = self.left_tab == LeftTab::Search && m.search_box.contains(x, y);
        if pressed && !in_search && self.ui_context[self.search_input].editing {
            self.ui_context.unfocus_id(self.search_input.id());
        }
        if in_search {
            if pressed && !self.ui_context[self.search_input].editing {
                self.ui_context.set_focused_id(self.search_input.id());
                WidgetHost::focus(&mut self.ui_context[self.search_input]);
            }
            self.ui_context.propagate_event(&ev, self.search_input.id());
            return None;
        }
        if pressed {
            if let Some(i) = self.completion_row_at(&m, x, y) {
                if let Some(c) = self.completion.as_mut() {
                    c.selected = i;
                }
                self.accept_completion();
                return None;
            }
            if let Some(t) = tab_at(m.left_tabs, 2, x, y) {
                self.left_tab = if t == 0 { LeftTab::Files } else { LeftTab::Search };
                if self.left_tab == LeftTab::Search {
                    let q = self.search_query();
                    self.open_search(&q);
                }
                self.needs_rebuild = true;
                return None;
            }
            if let Some(t) = tab_at(m.side_tabs, 2, x, y) {
                self.side_tab = if t == 0 { SideTab::Backlinks } else { SideTab::Outline };
                self.side_dirty = true;
                return None;
            }
            let search_area = if self.left_tab == LeftTab::Search { m.tree_body } else { Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 } };
            if let Some(a) = self.search_panel.click(search_area, x, y).or_else(|| self.side_panel.click(m.side_body, x, y)) {
                self.run_action(a);
                return None;
            }
            if self.current.is_some() && m.title.contains(x, y) {
                self.open_rename();
                return None;
            }
        }
        if pressed && m.mode_chip.contains(x, y) && self.current.is_some() {
            let next = if self.mode == Mode::Reading { Mode::Source } else { Mode::Reading };
            self.set_mode(next);
            return None;
        }
        if pressed {
            if let Some(i) = self.tree_row_at(&m, x, y) {
                let row = self.rows[i].clone();
                if row.folder {
                    self.tree.toggle(&row.path);
                    self.rebuild_rows();
                } else {
                    self.open_path(&row.path, None, true);
                }
                return None;
            }
            if let Some(hit) = self.reading_hit(x, y) {
                match hit {
                    Hit::Link(link) => self.follow(link),
                    Hit::Task { line, status } => self.toggle_task(line, status),
                }
                self.hover_hit = false;
                return None;
            }
        }
        // Back/forward mouse buttons walk the history, as in a browser.
        if state == ElementState::Pressed {
            match button {
                MouseButton::Back => {
                    self.go_history(-1);
                    return None;
                }
                MouseButton::Forward => {
                    self.go_history(1);
                    return None;
                }
                _ => {}
            }
        }
        if self.mode == Mode::Source && button == MouseButton::Left {
            if state == ElementState::Released {
                self.editor.release();
            } else if self.editor_rect(&m).contains(x, y) {
                let (shift, ctrl) = (self.ui_context.shift_pressed, self.ui_context.ctrl_pressed);
                match self.editor.press(x, y, shift, ctrl) {
                    doc_editor::Response::Follow(target) => {
                        self.editor.release();
                        self.hover_hit = false;
                        self.follow(match target {
                            doc_editor::Target::Note { target, subpath } => SpanLink::Note { target, subpath },
                            doc_editor::Target::Url(url) => SpanLink::Url(url),
                        });
                        return None;
                    }
                    _ => self.update_completion(),
                }
            }
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (x, y) = (pos.x, pos.y);
        if self.switcher.is_some() {
            return;
        }
        let m = self.metrics();
        if m.side_body.contains(x, y) {
            if self.side_panel.wheel(delta, m.side_body) {
                *needs_rebuild = true;
            }
            return;
        }
        if self.left_tab == LeftTab::Search && m.tree_body.contains(x, y) {
            if self.search_panel.wheel(delta, m.tree_body) {
                *needs_rebuild = true;
            }
            return;
        }
        if m.tree_body.contains(x, y) && self.left_tab == LeftTab::Files {
            let max = self.tree_max_scroll(&m);
            self.tree_motion.reconcile(0.0, self.tree_scroll);
            if self.tree_motion.apply(delta, (TREE_ROW_H, TREE_ROW_H), Bounds::max(0.0), Bounds::max(max)) {
                self.tree_scroll = self.tree_motion.y.pos();
                *needs_rebuild = true;
            }
            return;
        }
        if !m.note.contains(x, y) {
            return;
        }
        match self.mode {
            Mode::Reading => {
                let max = self.read_max_scroll(&m);
                self.read_motion.reconcile(0.0, self.read_scroll);
                let line = READ_SIZE * 1.5;
                if self.read_motion.apply(delta, (line, line * 2.0), Bounds::max(0.0), Bounds::max(max)) {
                    self.read_scroll = self.read_motion.y.pos();
                    *needs_rebuild = true;
                }
            }
            Mode::Source => {
                if self.editor.wheel(delta) {
                    *needs_rebuild = true;
                }
            }
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Message> {
        *needs_rebuild = true;
        let pressed = event.state == ElementState::Pressed;
        let ev = Event::KeyInput(event.clone());

        if self.switcher.is_some() {
            if pressed {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.close_switcher();
                        return None;
                    }
                    Key::Named(NamedKey::Enter) => {
                        self.switcher_pick(event.shift);
                        return None;
                    }
                    Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::ArrowUp) => {
                        if let Some(sw) = self.switcher.as_mut() {
                            let n = sw.choices.len();
                            if n > 0 {
                                let down = matches!(event.logical_key, Key::Named(NamedKey::ArrowDown));
                                sw.selected = if down { (sw.selected + 1) % n } else { (sw.selected + n - 1) % n };
                            }
                        }
                        return None;
                    }
                    _ => {}
                }
            }
            self.ui_context.propagate_event(&ev, self.switcher_input.id());
            self.refresh_switcher();
            return None;
        }

        if pressed {
            let k = |chord: &str| cce_ui::widget::match_key_shortcut(event, chord);
            if k(&self.keys.quit) {
                return Some(Message::Exit);
            }
            if k(&self.keys.switcher) && self.index.is_some() {
                self.open_switcher("");
                return None;
            }
            if k(&self.keys.toggle_mode) {
                let next = if self.mode == Mode::Reading { Mode::Source } else { Mode::Reading };
                self.set_mode(next);
                return None;
            }
            if k(&self.keys.toggle_source) && self.current.is_some() {
                // Live preview <-> source, both editing; from reading it
                // opens the editor in the mode it flips to.
                let on = !self.editor.preview;
                self.editor.set_preview(on);
                self.set_mode(Mode::Source);
                return None;
            }
            if k(&self.keys.save) {
                // In a conflict this is "keep mine": it writes over the disk copy.
                self.save_now(self.conflict);
                return None;
            }
            if k(&self.keys.reload) {
                self.reload();
                return None;
            }
            if k(&self.keys.back) {
                self.go_history(-1);
                return None;
            }
            if k(&self.keys.forward) {
                self.go_history(1);
                return None;
            }
            if k(&self.keys.toggle_tree) {
                self.show_tree = !self.show_tree;
                self.invalidate();
                return None;
            }
            if k(&self.keys.daily) && self.index.is_some() {
                self.open_daily(None);
                return None;
            }
            if k(&self.keys.toggle_side) {
                self.show_side = !self.show_side;
                self.side_dirty = true;
                self.invalidate();
                return None;
            }
            if k(&self.keys.search) && self.index.is_some() {
                let q = self.search_query();
                self.open_search(&q);
                self.ui_context[self.search_input].select_all();
                return None;
            }
            if k(&self.keys.rename) && self.current.is_some() {
                self.open_rename();
                return None;
            }
            if k(&self.keys.graph) && self.index.is_some() {
                // The local graph follows this window's note (it polls
                // `current` on the instance socket). cce-graph's vault mode
                // is single-instance, so a second press only raises nothing
                // new.
                let vault = self.index.as_ref().map(|ix| ix.root().to_path_buf());
                let mut cmd = std::process::Command::new("cce-graph");
                cmd.arg("--vault");
                if let Some(v) = vault {
                    cmd.arg(v);
                }
                cmd.arg("--local");
                if let Err(e) = spawn_detached(cmd) {
                    self.set_status(format!("Could not start cce-graph: {e}"), true);
                }
                return None;
            }
        }

        // The search box, while it holds the keyboard.
        if self.ui_context[self.search_input].editing {
            if pressed {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.ui_context.unfocus_id(self.search_input.id());
                        return None;
                    }
                    Key::Named(NamedKey::Enter) => {
                        // Enter opens the first result.
                        let first = self.search_panel.items.iter().find_map(|i| i.action.clone());
                        if let Some(a) = first {
                            self.ui_context.unfocus_id(self.search_input.id());
                            self.run_action(a);
                        }
                        return None;
                    }
                    _ => {}
                }
            }
            self.ui_context.propagate_event(&ev, self.search_input.id());
            self.refresh_search(false);
            return None;
        }

        match self.mode {
            Mode::Source => {
                if !pressed {
                    return None;
                }
                if self.completion_key(&event.logical_key) {
                    return None;
                }
                // An image on the clipboard pastes as an attachment;
                // anything else falls through to the editor's text paste.
                if cce_ui::widget::match_key_shortcut(event, "ctrl+v") && self.paste_images() {
                    self.update_completion();
                    return None;
                }
                self.editor.key(event);
                self.update_completion();
            }
            Mode::Reading if pressed => {
                let m = self.metrics();
                let max = self.read_max_scroll(&m);
                let page = m.note.height * 0.9;
                let target = match &event.logical_key {
                    Key::Named(NamedKey::ArrowDown) => Some(self.read_scroll + READ_SIZE * 3.0),
                    Key::Named(NamedKey::ArrowUp) => Some(self.read_scroll - READ_SIZE * 3.0),
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => Some(self.read_scroll + page),
                    Key::Named(NamedKey::PageUp) => Some(self.read_scroll - page),
                    Key::Named(NamedKey::Home) => Some(0.0),
                    Key::Named(NamedKey::End) => Some(max),
                    _ => None,
                };
                if let Some(t) = target {
                    let s = cce_ui::widget::scroll_motion::scroll_settings();
                    self.read_motion.reconcile(0.0, self.read_scroll);
                    self.read_motion.y.scroll_to(t, Bounds::max(max), &s);
                }
            }
            Mode::Reading => {}
        }
        None
    }
}

fn main() {
    env_logger::init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut explicit: Option<PathBuf> = None;
    if let Some(i) = args.iter().position(|a| a == "--vault") {
        if i + 1 >= args.len() {
            eprintln!("cce-notes: --vault needs a directory");
            std::process::exit(2);
        }
        explicit = Some(PathBuf::from(args.remove(i + 1)));
        args.remove(i);
    }
    let command = match Command::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cce-notes: {e}");
            std::process::exit(2);
        }
    };
    let vault = cce_vault::config::vault_root(explicit.as_deref()).map_err(|e| {
        format!(
            "No vault: {e}\nSet `vault {{ path \"~/Notes\" }}` in ~/.config/cce/config.kdl, \
             or run with --vault <dir> or CCE_VAULT=<dir>."
        )
    });
    // `--vault` naming another vault than the running instance shows: it
    // cannot show this one (its note would open there, or a same-named one
    // would), so this launch is a window of its own — no socket, no MCP.
    let standalone = match (&explicit, &vault) {
        (Some(_), Ok(root)) => !instance::running_instance_fits(instance::running_vault_reply().as_deref(), root),
        _ => false,
    };
    if standalone {
        eprintln!("cce-notes: another window shows a different vault; opening {} in a window of its own", explicit.as_ref().unwrap().display());
    } else if instance::forward_or_claim(&command) {
        return;
    }
    let _ = STARTUP.set(Startup { vault, command });
    cce_ui::engine::run::<NotesApp>();
    instance::cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app over a temp vault holding `files`, editing nothing yet.
    fn app_over(files: &[(&str, &str)]) -> (tempfile::TempDir, NotesApp) {
        let dir = tempfile::tempdir().unwrap();
        for (p, t) in files {
            let abs = dir.path().join(p);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, t).unwrap();
        }
        let ix = Index::open(dir.path(), false).unwrap();
        let (tx, _rx) = calloop::channel::channel();
        (dir, NotesApp::new(Some(ix), None, None, tx))
    }

    /// Open `path` in the editor and type `text` at its end, unsaved.
    fn open_and_type(app: &mut NotesApp, path: &str, text: &str) {
        app.open_path(path, None, true);
        app.set_mode(Mode::Source);
        let last = app.editor.buf.line_count() - 1;
        let end = Pos::new(last, app.editor.buf.line(last).len());
        app.editor.edit(end, end, text, end);
        assert!(app.dirty());
    }

    /// Something else writes the note, and the watcher reports it.
    fn changed_elsewhere(app: &mut NotesApp, dir: &Path, path: &str, text: &str) {
        let abs = dir.join(path);
        std::fs::write(&abs, text).unwrap();
        app.vault_changed(vec![abs]);
    }

    fn disk(dir: &Path, path: &str) -> String {
        std::fs::read_to_string(dir.join(path)).unwrap()
    }

    #[test]
    fn opening_the_open_note_keeps_what_was_typed() {
        let (_d, mut app) = app_over(&[("A.md", "one\ntwo\n")]);
        open_and_type(&mut app, "A.md", "typed");
        // A click on it in the tree, a search hit in it, cce-graph...
        app.open_path("A.md", Some(1), true);
        assert!(app.editor_text().ends_with("typed"), "the buffer was reloaded from disk");
        assert!(app.dirty());
        assert_eq!(app.editor.buf.caret.line, 1, "a line still goes there");
    }

    #[test]
    fn a_conflict_is_never_overwritten_by_reading_view_rename_or_reopen() {
        let (d, mut app) = app_over(&[("A.md", "base\n")]);
        open_and_type(&mut app, "A.md", "mine");
        changed_elsewhere(&mut app, d.path(), "A.md", "theirs\n");
        assert!(app.conflict);

        app.set_mode(Mode::Reading);
        assert_eq!(disk(d.path(), "A.md"), "theirs\n", "Ctrl+E wrote over the disk copy");
        assert_eq!(app.mode, Mode::Source, "it stays in the editor until resolved");

        app.rename("A.md", "B.md");
        assert_eq!(disk(d.path(), "A.md"), "theirs\n", "a rename wrote over the disk copy");
        assert!(!d.path().join("B.md").exists());

        app.open_path("A.md", None, true);
        assert!(app.editor_text().ends_with("mine"), "reopening discarded ours");

        // Ctrl+R: theirs, on purpose.
        app.reload();
        assert_eq!(app.editor_text(), "theirs\n");
        assert!(!app.conflict && !app.dirty());

        // Ctrl+S in a conflict: ours, on purpose.
        open_and_type(&mut app, "A.md", "again");
        changed_elsewhere(&mut app, d.path(), "A.md", "theirs 2\n");
        assert!(app.save_now(app.conflict));
        assert!(disk(d.path(), "A.md").ends_with("again"));
        assert!(!app.conflict);
    }

    #[test]
    fn a_note_deleted_on_disk_is_not_written_back() {
        let (d, mut app) = app_over(&[("A.md", "base\n"), ("B.md", "b\n")]);
        open_and_type(&mut app, "A.md", "mine");
        let abs = d.path().join("A.md");
        std::fs::remove_file(&abs).unwrap();
        app.vault_changed(vec![abs.clone()]);
        assert!(app.gone && app.conflict);

        // What autosave and leaving for reading view do: nothing.
        assert!(!app.save());
        app.set_mode(Mode::Reading);
        assert!(!abs.exists(), "the deleted note came back");

        // Clean, it may be left; typed into, it still is not recreated.
        app.load_text("base\n".into());
        app.gone = true;
        app.conflict = true;
        let end = Pos::new(1, 0);
        app.editor.edit(end, end, "later", end);
        assert!(!app.save());
        assert!(!abs.exists());

        // Ctrl+S brings it back on purpose; Ctrl+R closes it.
        assert!(app.save_now(true));
        assert!(abs.exists() && !app.gone);
        std::fs::remove_file(&abs).unwrap();
        app.vault_changed(vec![abs.clone()]);
        app.reload();
        assert_eq!(app.current, None);
        assert!(!abs.exists());

        // A note that returns (a sync's delete then write) clears it.
        open_and_type(&mut app, "B.md", "x");
        let b = d.path().join("B.md");
        std::fs::remove_file(&b).unwrap();
        app.vault_changed(vec![b.clone()]);
        assert!(app.gone);
        std::fs::write(&b, "b\n").unwrap();
        app.vault_changed(vec![b]);
        assert!(!app.gone && !app.conflict);
    }

    #[test]
    fn a_note_name_ending_in_a_number_opens_as_itself() {
        let (_d, mut app) = app_over(&[("Chapter 3.md", "c\n"), ("Notes.md", "a\nb\nc\n")]);
        app.open_target("Chapter 3", None);
        assert_eq!(app.current.as_deref(), Some("Chapter 3.md"));
        // An older sender's `open Notes 2`: no note is called that, so 2 is
        // a (1-based) line of Notes.
        app.open_target("Notes 2", None);
        assert_eq!(app.current.as_deref(), Some("Notes.md"));
        assert_eq!(app.pending_line, Some(1));
        // Neither names anything: said so, nothing opened.
        app.open_target("Nothing 4", None);
        assert_eq!(app.current.as_deref(), Some("Notes.md"));
    }

    #[test]
    fn quitting_in_a_conflict_keeps_ours_as_a_copy() {
        let (d, mut app) = app_over(&[("dir/A.md", "base\n")]);
        open_and_type(&mut app, "dir/A.md", "mine");
        changed_elsewhere(&mut app, d.path(), "dir/A.md", "theirs\n");
        app.on_exit();
        assert_eq!(disk(d.path(), "dir/A.md"), "theirs\n", "theirs is left as it is");
        let copies: Vec<String> = std::fs::read_dir(d.path().join("dir"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("A (conflict "))
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert!(disk(d.path(), &format!("dir/{}", copies[0])).ends_with("mine"));

        // Without a conflict, exit saves as before and makes no copy.
        let (d, mut app) = app_over(&[("B.md", "base\n")]);
        open_and_type(&mut app, "B.md", "mine");
        app.on_exit();
        assert!(disk(d.path(), "B.md").ends_with("mine"));
        assert_eq!(std::fs::read_dir(d.path()).unwrap().filter(|e| e.as_ref().unwrap().path().is_file()).count(), 1);
    }
}
