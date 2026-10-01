//! `cce-notes` — the vault's notes editor (Obsidian-on-cce, milestone 2).
//!
//! A file tree on the left and one note on the right, read as rendered
//! Markdown (the reading view) or edited as text (source mode), toggled
//! with Ctrl+E as in Obsidian. Links follow on click — or Ctrl+click in
//! source — and a link to a note that does not exist creates it. Ctrl+O is
//! the quick switcher; Alt+←/→ walk the history.
//!
//! The files are the source of truth and there is no daemon: the app
//! embeds a `cce_vault::Index` and a watcher, so an edit from Obsidian,
//! Dropbox or another cce app arrives like any other change. A clean
//! buffer reloads silently; a dirty one is flagged as a conflict and never
//! overwritten. Source mode saves a moment after typing stops, on leaving
//! the note, and on exit.
//!
//! The vault comes from `--vault <dir>`, `$CCE_VAULT`, or
//! `vault { path "…" }` in `~/.config/cce/config.kdl`. The app runs single
//! instance (see [`instance`]); `cce-notes <note>` opens a note in it.

mod instance;
mod reading;
mod tree;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use cce_ui::engine::{Application, CursorIcon, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx, TextAttrs};
use cce_ui::widget::{
    Adapted, Bounds, ElementState, Event, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, ScrollMotion,
    TextBox, WidgetHost,
};
use cce_vault::markdown::{Block, SpanLink};
use cce_vault::{FileKind, Index, VaultWatcher};
use wayland_client::QueueHandle;

use instance::Command;
use reading::{srgb_u8, Hit, Layout, Measure, Theme};

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
/// Source mode saves once typing has paused this long.
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
    save: String,
    reload: String,
    back: String,
    forward: String,
    toggle_tree: String,
    daily: String,
    quit: String,
}

impl Keys {
    fn load() -> Keys {
        let get = cce_ui::input::app_chord;
        Keys {
            switcher: get("quick_switcher", "ctrl+o"),
            toggle_mode: get("toggle_mode", "ctrl+e"),
            save: get("save", "ctrl+s"),
            reload: get("reload", "ctrl+r"),
            back: get("back", "alt+arrowleft"),
            forward: get("forward", "alt+arrowright"),
            toggle_tree: get("toggle_tree", "ctrl+\\"),
            daily: get("daily", "alt+d"),
            quit: get("quit", "ctrl+q"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Reading,
    Source,
}

/// One quick-switcher result: an existing note, or "create this".
#[derive(Clone, Debug)]
enum Choice {
    Note { path: String, shown: String },
    Create(String),
}

struct Switcher {
    choices: Vec<Choice>,
    selected: usize,
    /// The query the choices were computed for.
    query: String,
}

/// Widths through the renderer's own shaping entry, cached per run.
struct RendererMeasure<'a> {
    fs: &'a mut cce_ui::cosmic_text::FontSystem,
    scale: f32,
    cache: &'a mut HashMap<(String, u32, String, TextAttrs), f32>,
}

impl Measure for RendererMeasure<'_> {
    fn width(&mut self, text: &str, size: f32, font: &str, attrs: TextAttrs) -> f32 {
        let key = (text.to_string(), (size * 100.0) as u32, font.to_string(), attrs);
        if let Some(w) = self.cache.get(&key) {
            return *w;
        }
        let buf = cce_ui::backend::window_runner::get_text_buffer_attrs(self.fs, text, size, Some(font), attrs);
        let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max) / self.scale;
        self.cache.insert(key, w);
        w
    }
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
    widths: HashMap<(String, u32, String, TextAttrs), f32>,
    /// Shapes for layout widths; loads the same fonts as the renderer
    /// (system fonts included, see `load_system_fonts`). Made on first use.
    measure_fs: Option<cce_ui::cosmic_text::FontSystem>,
    mode: Mode,
    read_scroll: f32,
    read_motion: ScrollMotion,
    /// A source line to bring into view once the layout exists.
    pending_line: Option<usize>,
    hover_hit: bool,

    editor: Adapted<TextBox>,
    /// The buffer as last seen by the autosave check, and when it changed.
    edit_seen: String,
    edit_changed_at: Instant,

    history: Vec<String>,
    hist_pos: usize,

    switcher: Option<Switcher>,
    switcher_input: Adapted<TextBox>,

    conflict: bool,
    status: Option<(String, bool)>,

    width: u32,
    height: u32,
    scale: f64,
    needs_rebuild: bool,
    ui_context: cce_ui::context::UiContext,
    widgets_registered: bool,
    pointer: (f32, f32),
}

/// Where everything sits for a window size.
struct Metrics {
    tree: Rect,
    note: Rect,
    mode_chip: Rect,
}

impl NotesApp {
    fn metrics(&self) -> Metrics {
        let (w, h) = (self.width as f32, self.height as f32);
        let body_h = (h - BAND_H - STATUS_H).max(0.0);
        let tree_w = if self.show_tree && self.index.is_some() { TREE_W.min(w * 0.4) } else { 0.0 };
        let inset = cce_ui::layout::root_plate_inset();
        Metrics {
            tree: Rect { x: 0.0, y: BAND_H, width: tree_w, height: body_h },
            note: Rect { x: tree_w, y: BAND_H, width: w - tree_w, height: body_h },
            mode_chip: Rect { x: w - inset - 84.0, y: (BAND_H - 24.0) / 2.0, width: 84.0, height: 24.0 },
        }
    }

    /// The reading column's origin and width inside the note pane.
    fn reading_frame(&self, m: &Metrics) -> (f32, f32, f32) {
        let width = (m.note.width - 2.0 * READ_PAD).min(READ_MAX_W).max(80.0);
        let x = m.note.x + (m.note.width - width) / 2.0;
        (x, m.note.y + READ_PAD, width)
    }

    fn editor_rect(&self, m: &Metrics) -> Rect {
        let gap = cce_ui::layout::root_plate_gap();
        Rect {
            x: m.note.x + gap,
            y: m.note.y + gap,
            width: (m.note.width - 2.0 * gap).max(50.0),
            height: (m.note.height - 2.0 * gap).max(50.0),
        }
    }

    fn rebuild_rows(&mut self) {
        let Some(ix) = &self.index else { return };
        let notes = ix.files().iter().filter(|(_, e)| e.kind == FileKind::Note).map(|(p, _)| p.as_str());
        self.rows = self.tree.rows(notes);
    }

    fn editor_text(&self) -> &str {
        if self.editor.editing {
            &self.editor.edit_buffer
        } else {
            &self.editor.text
        }
    }

    fn dirty(&self) -> bool {
        self.mode == Mode::Source && self.current.is_some() && self.editor_text() != self.saved
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error));
    }

    fn invalidate(&mut self) {
        self.layout = None;
        self.needs_rebuild = true;
    }

    // ---- notes -------------------------------------------------------

    /// Show `text` as the current note's content, as on disk.
    fn load_text(&mut self, text: String) {
        self.blocks = cce_vault::markdown::blocks(&text);
        self.editor.text = text.clone();
        self.editor.edit_buffer = text.clone();
        self.editor.cursor_idx = 0;
        self.editor.select_anchor = None;
        self.edit_seen = text.clone();
        self.saved = text;
        self.conflict = false;
        self.invalidate();
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
        let text = self.editor_text().to_string();
        let (Some(cur), Some(ix)) = (self.current.clone(), self.index.as_mut()) else { return false };
        match ix.write_text(&cur, &text) {
            Ok(()) => {
                self.saved = text;
                self.conflict = false;
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
                let _ = std::process::Command::new("xdg-open").arg(abs).spawn();
                return;
            }
        }
        if self.current.as_deref() != Some(path) {
            if self.conflict {
                self.set_status("Resolve the conflict first: Ctrl+S keeps yours, Ctrl+R loads the disk copy", true);
                return;
            }
            if !self.save() {
                return;
            }
        }
        let Some(ix) = &self.index else { return };
        let text = match ix.read_text(path) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(format!("Could not read {path}: {e}"), true);
                return;
            }
        };
        let same = self.current.as_deref() == Some(path);
        self.current = Some(path.to_string());
        self.load_text(text);
        if !same {
            self.read_scroll = 0.0;
            self.read_motion = ScrollMotion::new();
        }
        self.pending_line = line;
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

    /// Open what a command or argv names: an absolute file in the vault,
    /// a vault path, or a note name; with an optional `#heading`.
    fn open_target(&mut self, target: &str, line: Option<usize>) {
        let Some(ix) = &self.index else { return };
        let (name, sub) = match target.split_once('#') {
            Some((n, s)) => (n, Some(s)),
            None => (target, None),
        };
        let path = if Path::new(name).is_absolute() {
            let p = Path::new(name);
            p.canonicalize().ok().and_then(|c| ix.rel(&c)).or_else(|| ix.rel(p))
        } else {
            ix.lookup(name)
        };
        match path {
            Some(p) => {
                let line = line.map(|l| l.saturating_sub(1)).or_else(|| sub.and_then(|s| self.subpath_line(&p, s)));
                self.open_path(&p, line, true);
            }
            None => self.set_status(format!("No note named {name}"), true),
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
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            // Tag search comes with the search pane.
            SpanLink::Tag(_) => {}
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
            let text = self.editor_text().to_string();
            self.blocks = cce_vault::markdown::blocks(&text);
            self.editor.unfocus();
            self.invalidate();
        } else {
            self.ui_context.set_focused(&mut self.editor);
            WidgetHost::focus(&mut self.editor);
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
            Command::Search(q) => self.open_switcher(&q),
            Command::Show => {}
        }
        self.needs_rebuild = true;
    }

    // ---- vault changes -------------------------------------------------

    fn vault_changed(&mut self, paths: Vec<PathBuf>) {
        let Some(ix) = self.index.as_mut() else { return };
        ix.apply_changes(&paths);
        self.rebuild_rows();
        // Link colours depend on what resolves.
        self.layout = None;
        let Some(cur) = self.current.clone() else { return };
        let Some(ix) = self.index.as_ref() else { return };
        let abs = ix.abs(&cur);
        if !paths.iter().any(|p| p == &abs || p.canonicalize().ok().as_deref() == abs.canonicalize().ok().as_deref()) {
            return;
        }
        match ix.read_text(&cur) {
            Ok(disk) if disk == self.saved => {}
            Ok(disk) => {
                if self.dirty() {
                    self.conflict = true;
                } else {
                    let scroll = self.read_scroll;
                    self.load_text(disk);
                    self.read_scroll = scroll;
                }
            }
            Err(_) => self.set_status(format!("{cur} was deleted or moved on disk"), true),
        }
    }

    // ---- quick switcher -------------------------------------------------

    fn open_switcher(&mut self, query: &str) {
        self.switcher_input.text = query.to_string();
        self.switcher_input.edit_buffer = query.to_string();
        self.switcher_input.cursor_idx = query.chars().count();
        self.switcher_input.select_anchor = None;
        self.switcher = Some(Switcher { choices: Vec::new(), selected: 0, query: "\u{0}".into() });
        self.ui_context.set_focused(&mut self.switcher_input);
        WidgetHost::focus(&mut self.switcher_input);
        self.refresh_switcher();
    }

    fn close_switcher(&mut self) {
        self.switcher = None;
        self.switcher_input.unfocus();
        if self.mode == Mode::Source {
            self.ui_context.set_focused(&mut self.editor);
            WidgetHost::focus(&mut self.editor);
        }
        self.needs_rebuild = true;
    }

    fn switcher_query(&self) -> String {
        if self.switcher_input.editing {
            self.switcher_input.edit_buffer.clone()
        } else {
            self.switcher_input.text.clone()
        }
    }

    fn refresh_switcher(&mut self) {
        let query = self.switcher_query();
        let Some(ix) = &self.index else { return };
        let Some(sw) = self.switcher.as_mut() else { return };
        if sw.query == query {
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
            notes.sort_by(|a, b| b.1.cmp(&a.1));
            for (p, _) in notes {
                if seen.insert(p.clone()) {
                    out.push(p.clone());
                }
            }
            out.into_iter()
                .take(SWITCHER_ROWS)
                .map(|p| Choice::Note { shown: p.clone(), path: p })
                .collect()
        } else {
            let mut c: Vec<Choice> = ix
                .find(&query, SWITCHER_ROWS)
                .into_iter()
                .filter(|m| ix.entry(&m.path).is_some_and(|e| e.kind == FileKind::Note))
                .map(|m| {
                    let shown = if m.path.trim_end_matches(".md").ends_with(m.matched.as_str()) {
                        m.path.clone()
                    } else {
                        format!("{}  ← {}", m.path, m.matched)
                    };
                    Choice::Note { path: m.path, shown }
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
        let n = self.switcher.as_ref().map_or(0, |s| s.choices.len());
        Rect {
            x: (self.width as f32 - w) / 2.0,
            y: BAND_H + 24.0,
            width: w,
            height: 12.0 + 32.0 + 8.0 + n as f32 * SWITCHER_ROW_H + 6.0,
        }
    }

    fn switcher_row_at(&self, x: f32, y: f32) -> Option<usize> {
        let r = self.switcher_rect();
        let top = r.y + 12.0 + 32.0 + 8.0;
        if x < r.x || x > r.x + r.width || y < top {
            return None;
        }
        let i = ((y - top) / SWITCHER_ROW_H) as usize;
        (i < self.switcher.as_ref()?.choices.len()).then_some(i)
    }

    // ---- layout and scrolling -------------------------------------------

    fn ensure_layout(&mut self, width: f32) {
        if matches!(&self.layout, Some((w, _)) if (w - width).abs() < 0.5) {
            return;
        }
        let fs = self.measure_fs.get_or_insert_with(cce_ui::create_font_system_with_system_fonts);
        let mut m = RendererMeasure { fs, scale: self.scale as f32, cache: &mut self.widths };
        let index = &self.index;
        let cur = self.current.as_deref();
        let resolved = |l: &SpanLink| match l {
            SpanLink::Note { target, .. } | SpanLink::Embed { target, .. } => {
                target.is_empty() || index.as_ref().is_some_and(|ix| ix.resolve_text(cur, target).is_some())
            }
            _ => true,
        };
        let laid = reading::layout(&self.blocks, width, &self.theme, &mut m, &resolved);
        self.layout = Some((width, laid));
    }

    fn read_max_scroll(&self, m: &Metrics) -> f32 {
        let h = self.layout.as_ref().map_or(0.0, |(_, l)| l.height);
        (h + 2.0 * READ_PAD - m.note.height).max(0.0)
    }

    fn tree_max_scroll(&self, m: &Metrics) -> f32 {
        (self.rows.len() as f32 * TREE_ROW_H + 8.0 - m.tree.height).max(0.0)
    }

    fn tree_row_at(&self, m: &Metrics, x: f32, y: f32) -> Option<usize> {
        if x < m.tree.x || x > m.tree.x + m.tree.width || y < m.tree.y || y > m.tree.y + m.tree.height {
            return None;
        }
        let i = ((y - m.tree.y - 4.0 + self.tree_scroll) / TREE_ROW_H).floor();
        (i >= 0.0 && (i as usize) < self.rows.len()).then_some(i as usize)
    }

    /// The reading view's click target under a window point.
    fn reading_hit(&self, x: f32, y: f32) -> Option<Hit> {
        let m = self.metrics();
        if self.mode != Mode::Reading || self.switcher.is_some() || !contains(m.note, x, y) {
            return None;
        }
        let (ox, oy, _) = self.reading_frame(&m);
        let (_, l) = self.layout.as_ref()?;
        l.hit(x - ox, y - oy + self.read_scroll).cloned()
    }

    /// In source mode, the link under the editor's caret (after a click).
    fn link_at_caret(&self) -> Option<SpanLink> {
        let text = self.editor_text();
        let byte = text.char_indices().nth(self.editor.cursor_idx).map_or(text.len(), |(b, _)| b);
        let note = cce_vault::parse::parse(text);
        let link = note.links.iter().find(|l| l.span.start <= byte && byte < l.span.end)?;
        let url = link.target.contains("://") || link.target.starts_with("mailto:");
        Some(if url {
            SpanLink::Url(link.target.clone())
        } else {
            SpanLink::Note { target: link.target.clone(), subpath: link.subpath.clone() }
        })
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
                let dirty = if self.dirty() { " •" } else { "" };
                let bounds = Some([inset, 0.0, right, BAND_H]);
                let dir_w = dir.chars().count() as f32 * size * 0.6;
                pc.text_with(dir, inset, ty, size, srgb_u8(DIM), Some(family.clone()), bounds);
                pc.text_with(format!("{name}{dirty}"), inset + dir_w, ty, size, srgb_u8(FG), Some(family.clone()), bounds);
            }
            None => {
                let vault = self.index.as_ref().map(|ix| vault_name(ix.root())).unwrap_or_else(|| "Notes".into());
                pc.text_with(vault, inset, ty, size, srgb_u8(FG), Some(family.clone()), None);
            }
        }
        if self.current.is_some() {
            let label = match self.mode {
                Mode::Reading => "Reading",
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
            ("Changed on disk · Ctrl+S keeps yours · Ctrl+R reloads".to_string(), CONFLICT)
        } else if let Some((msg, err)) = &self.status {
            (msg.clone(), if *err { CONFLICT } else { DIM })
        } else if let (Some(cur), Some(ix)) = (&self.current, &self.index) {
            let text = if self.mode == Mode::Source { self.editor_text() } else { self.saved.as_str() };
            let words = text.split_whitespace().count();
            let back = ix.backlinks(cur).len();
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
        let t = m.tree;
        pc.recess_edges(t, (0.0, 0.0, 0.0, 0.0), cce_ui::layout::bar_wall_width(), (false, true, false, false));
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
                let (label, color) = if row.folder {
                    (format!("{} {}", if row.open { "▾" } else { "▸" }, row.name), DIM)
                } else {
                    (format!("  {}", row.name), FG)
                };
                pc.text_with(
                    label,
                    x,
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
            Mode::Source => cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.editor, pc),
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
                    pc.clip(note, |pc| l.paint(pc, (ox, oy), self.read_scroll, note));
                }
            }
        }
    }

    fn paint_switcher(&self, pc: &mut PaintCtx) {
        let Some(sw) = &self.switcher else { return };
        let r = self.switcher_rect();
        pc.rounded_rect(r, 10.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_BG);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.switcher_input, pc);
        let (family, size) = cce_ui::layout::list_font_parsed();
        let top = r.y + 12.0 + 32.0 + 8.0;
        for (i, c) in sw.choices.iter().enumerate() {
            let row = Rect { x: r.x + 6.0, y: top + i as f32 * SWITCHER_ROW_H, width: r.width - 12.0, height: SWITCHER_ROW_H };
            if i == sw.selected {
                pc.rounded_rect(row, 5.0, (true, true, true, true), cce_ui::colors::PANEL_MENU_HOVER);
            }
            let (label, color) = match c {
                Choice::Note { shown, .. } => (shown.trim_end_matches(".md").to_string(), FG),
                Choice::Create(name) => (format!("Create “{name}”"), DIM),
            };
            pc.text_with(
                label,
                row.x + 10.0,
                cce_ui::layout::align_text_y(row.y, row.height, size, 0.0),
                size,
                srgb_u8(color),
                Some(family.clone()),
                Some([row.x, row.y, row.x + row.width - 6.0, row.y + row.height]),
            );
        }
    }
}

fn contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn vault_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string())
}

impl Application for NotesApp {
    type Message = Message;

    fn ui_context(&self) -> Option<&cce_ui::context::UiContext> {
        Some(&self.ui_context)
    }

    fn ui_context_mut(&mut self) -> Option<&mut cce_ui::context::UiContext> {
        Some(&mut self.ui_context)
    }

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
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
        instance::spawn_listener(sender.clone());
        let _ = sender.send(Message::Command(startup.command.clone()));

        let mut editor = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(true).with_max_width(None);
        editor.font_family = "monospace".to_string();
        editor.font_size = 13.0;
        let switcher_input = TextBox::new(String::new()).with_placeholder("Find or create a note…");

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
            widths: HashMap::new(),
            measure_fs: None,
            mode: Mode::Reading,
            read_scroll: 0.0,
            read_motion: ScrollMotion::new(),
            pending_line: None,
            hover_hit: false,
            editor,
            edit_seen: String::new(),
            edit_changed_at: Instant::now(),
            history: Vec::new(),
            hist_pos: 0,
            switcher: None,
            switcher_input,
            conflict: false,
            status: None,
            width: 1000,
            height: 700,
            scale: 1.0,
            needs_rebuild: true,
            ui_context: cce_ui::context::UiContext::new(),
            widgets_registered: false,
            pointer: (0.0, 0.0),
        };
        app.rebuild_rows();
        app
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
            Message::Exit => *_exit = true,
        }
        *needs_rebuild = true;
    }

    fn idle_poll_interval(&self) -> Option<Duration> {
        // Wakes the autosave check while source mode holds unsaved text.
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
        if self.dirty() && !self.conflict {
            let now = Instant::now();
            if self.editor_text() != self.edit_seen {
                self.edit_seen = self.editor_text().to_string();
                self.edit_changed_at = now;
            } else if now.duration_since(self.edit_changed_at) >= AUTOSAVE_AFTER {
                self.save();
                *needs_rebuild = true;
            }
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        if !self.widgets_registered {
            self.widgets_registered = true;
            let (id, ptr) = (self.editor.id(), self.editor.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
            let (id, ptr) = (self.switcher_input.id(), self.switcher_input.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
        }
        let size_changed =
            self.width != size.width as u32 || self.height != size.height as u32 || self.scale != scale;
        if size_changed {
            if self.scale != scale {
                self.widths.clear();
                self.layout = None;
            }
            self.width = size.width as u32;
            self.height = size.height as u32;
            self.scale = scale;
            cce_ui::scale::set_scale_factor(scale as f32);
        }
        let m = self.metrics();
        if self.needs_rebuild || size_changed {
            let e = self.editor_rect(&m);
            self.editor.set_rect(e.x, e.y, e.width, e.height);
            let r = self.switcher_rect();
            self.switcher_input.set_rect(r.x + 12.0, r.y + 12.0, r.width - 24.0, 32.0);
            self.needs_rebuild = false;
            self.ui_context.rebuild_spatial_grid();
            // Keep scroll offsets in range after a resize or a new layout.
            self.tree_scroll = self.tree_scroll.min(self.tree_max_scroll(&m));
        }

        let (w, h) = (self.width as f32, self.height as f32);
        let mut pc = PaintCtx::new();
        pc.root_plate(w, h);
        self.paint_band(&mut pc, &m);
        if self.switcher.is_none() {
            self.paint_tree(&mut pc, &m);
        }
        self.paint_note(&mut pc, &m);
        let max = self.read_max_scroll(&m);
        if self.read_scroll > max {
            self.read_scroll = max;
            self.read_motion.y.jump_to(max);
        }
        self.paint_status(&mut pc);
        self.paint_switcher(&mut pc);
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
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
        if self.hover_hit || self.tree_hover.is_some() {
            return Some(CursorIcon::Pointer);
        }
        let m = self.metrics();
        (self.mode == Mode::Source && self.switcher.is_none() && contains(self.editor_rect(&m), x, y))
            .then_some(CursorIcon::Text)
    }

    fn on_exit(&mut self) {
        if !self.conflict {
            self.save();
        }
        if let Some(ix) = self.index.as_mut() {
            let _ = ix.save_cache();
        }
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (x, y) = (pos.x as f32, pos.y as f32);
        self.pointer = (x, y);
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
        let hit = self.reading_hit(x, y).is_some();
        if hit != self.hover_hit {
            self.hover_hit = hit;
            *needs_rebuild = true;
        }
        if self.mode == Mode::Source && self.ui_context.propagate_event(&ev, self.editor.id()) {
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
        let (x, y) = (pos.x as f32, pos.y as f32);
        let ev = Event::MouseButton { button, state, x, y, local_x: x, local_y: y };
        *needs_rebuild = true;
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
                if !contains(self.switcher_rect(), x, y) {
                    self.close_switcher();
                    return None;
                }
            }
            self.ui_context.propagate_event(&ev, self.switcher_input.id());
            return None;
        }

        let m = self.metrics();
        if pressed && contains(m.mode_chip, x, y) && self.current.is_some() {
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
        if self.mode == Mode::Source {
            let ctrl_click = pressed && self.ui_context.ctrl_pressed;
            self.ui_context.propagate_event(&ev, self.editor.id());
            if ctrl_click && contains(self.editor_rect(&m), x, y) {
                if let Some(link) = self.link_at_caret() {
                    self.follow(link);
                }
            }
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (x, y) = (pos.x as f32, pos.y as f32);
        if self.switcher.is_some() {
            return;
        }
        let m = self.metrics();
        if contains(m.tree, x, y) {
            let max = self.tree_max_scroll(&m);
            self.tree_motion.reconcile(0.0, self.tree_scroll);
            if self.tree_motion.apply(delta, (TREE_ROW_H, TREE_ROW_H), Bounds::max(0.0), Bounds::max(max)) {
                self.tree_scroll = self.tree_motion.y.pos();
                *needs_rebuild = true;
            }
            return;
        }
        if !contains(m.note, x, y) {
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
                let ev = Event::MouseWheel { delta: *delta, x, y, local_x: x, local_y: y };
                if self.ui_context.propagate_event(&ev, self.editor.id()) {
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
            if k(&self.keys.save) {
                // In a conflict this is "keep mine": it writes over the disk copy.
                self.save_now(self.conflict);
                return None;
            }
            if k(&self.keys.reload) {
                if let Some(cur) = self.current.clone() {
                    let mode = self.mode;
                    self.mode = Mode::Reading;
                    self.open_path(&cur, None, false);
                    self.mode = mode;
                }
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
        }

        match self.mode {
            Mode::Source => {
                self.ui_context.propagate_event(&ev, self.editor.id());
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
    if instance::forward_or_claim(&command) {
        return;
    }
    let vault = cce_vault::config::vault_root(explicit.as_deref()).map_err(|e| {
        format!(
            "No vault: {e}\nSet `vault {{ path \"~/Notes\" }}` in ~/.config/cce/config.kdl, \
             or run with --vault <dir> or CCE_VAULT=<dir>."
        )
    });
    let _ = STARTUP.set(Startup { vault, command });
    cce_ui::engine::run::<NotesApp>();
    instance::cleanup();
}
