//! MCP tools, so agents can read and write the vault through the running
//! app (cce-ui's tools-only server, as cce-designer uses it):
//!
//! ```sh
//! claude mcp add --transport http cce-notes http://127.0.0.1:3002/mcp
//! ```
//!
//! Only the single instance serves it (a forwarding launch exits first).
//! `CCE_NOTES_MCP_PORT` moves the port, for a second instance in a shadow.
//! Calls run on the app's event loop, so they see the same index as the
//! window, and writes go through the same paths (a note open and dirty in
//! the window raises the usual conflict rather than being overwritten).

use cce_ui::mcp::{McpServer, McpTool, McpToolCall, ToolClass};
use cce_vault::index::stem;
use cce_vault::FileKind;
use serde_json::{json, Value};

use crate::{Message, NotesApp};

const DEFAULT_PORT: u16 = 3002;

/// What the model is told about the notes app when a client connects.
const BRIEF: &str = "\
cce-notes is the user's notes vault: Markdown notes with [[wikilinks]], #tags, YAML frontmatter, \
`- [ ]` tasks (`📅 YYYY-MM-DD` for a due date) and a daily note per day. The user keeps their \
schedule, tasks, finances and projects here. Call `context` first: it names the note open in \
the window and quotes its first lines. Find notes with `search` (full text, or `#tag`) and \
`find_notes` (names), and `tasks` for open tasks and what is due; read a note whole with \
`read_note` before changing it. Change notes with `edit_note` (an exact passage), \
`append_note` (at the end, or under a heading) and `set_task` (tick, untick, set a due date); \
`append_daily` adds to a daily note. Write tasks as `- [ ] text 📅 YYYY-MM-DD` and links as \
[[Note Name]]. A note open in the window with unsaved edits cannot be changed until the user \
saves it. The vault keeps every version of a note, so a change can be taken back. The user's \
calendar and spreadsheets may be connected as well.";

pub fn start(sender: calloop::channel::Sender<Message>) {
    let port = std::env::var("CCE_NOTES_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    server().fixed_port(port).start(sender, Message::Mcp);
}

/// The server: the tools, what each does to the world, the brief, and the
/// vault apps a client should load beside it.
fn server() -> McpServer {
    McpServer::new("cce-notes", tools())
        .instructions(BRIEF)
        .companions(&["cce-calendar", "cce-spreadsheet"])
        .classify(ToolClass::Read, &["search", "find_notes", "read_note", "backlinks", "current_note", "tasks"])
        // A note's every version is kept (cce-sync), so a write can be taken back.
        .classify(ToolClass::Edit, &["open_note", "append_daily", "edit_note", "append_note", "set_task"])
}

/// The `context` answer for the window's state: the open note (its text as
/// shown, unsaved edits included) or none.
fn note_context(path: Option<&str>, mode: &str, unsaved: bool, conflict: bool, text: Option<&str>) -> cce_ui::mcp::Context {
    cce_ui::mcp::Context {
        document: path.map(|p| cce_ui::mcp::ContextDocument { title: stem(p).to_string(), path: Some(p.to_string()) }),
        view: json!({ "mode": mode, "conflict_with_disk": conflict }),
        dirty: path.map(|_| unsaved),
        undo: Some("the vault keeps every version of a note; the window's own undo covers typing".into()),
        summary: text.filter(|t| !t.trim().is_empty()).map(|t| format!("The open note, from the top:\n{t}")),
        ..cce_ui::mcp::Context::new(concat!("cce-notes ", env!("CARGO_PKG_VERSION")))
    }
}

fn tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, input_schema: Value| McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    let note_arg = json!({ "type": "string", "description": "A note name, vault path, or path without .md" });
    vec![
        cce_ui::mcp::context_tool(),
        tool(
            "search",
            "Full-text search over the vault's notes (every word must match), or `#tag` / `tag:name` for the notes carrying a tag. Returns matching notes with up to 5 matching lines each (1-based line numbers).",
            json!({ "type": "object", "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Most notes to return (default 20)" }
            }, "required": ["query"] }),
        ),
        tool(
            "find_notes",
            "Fuzzy-match note names, aliases and paths, best first.",
            json!({ "type": "object", "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Default 10" }
            }, "required": ["query"] }),
        ),
        tool(
            "read_note",
            "A note's full Markdown text, with its vault path.",
            json!({ "type": "object", "properties": { "note": note_arg }, "required": ["note"] }),
        ),
        tool(
            "backlinks",
            "Notes linking to a note (with the linking lines, 1-based), and unlinked mentions of its name.",
            json!({ "type": "object", "properties": { "note": note_arg }, "required": ["note"] }),
        ),
        tool(
            "open_note",
            "Show a note in the cce-notes window, optionally at a 1-based line.",
            json!({ "type": "object", "properties": {
                "note": note_arg,
                "line": { "type": "integer" }
            }, "required": ["note"] }),
        ),
        tool(
            "append_daily",
            "Append a paragraph to a day's daily note (today by default), creating the note from the vault's daily template if needed. Returns the note's path.",
            json!({ "type": "object", "properties": {
                "text": { "type": "string" },
                "date": { "type": "string", "description": "YYYY-MM-DD; default today" }
            }, "required": ["text"] }),
        ),
        tool(
            "tasks",
            "Tasks (`- [ ]` lines) across the vault, each with its note, 1-based line, status (' ' open, 'x' done), text and due date.",
            json!({ "type": "object", "properties": {
                "status": { "type": "string", "enum": ["open", "done", "all"], "description": "Default open" },
                "due_before": { "type": "string", "description": "Only tasks due on or before this date, YYYY-MM-DD" },
                "note": { "type": "string", "description": "Only in this note or folder (vault path)" },
                "limit": { "type": "integer", "description": "Most tasks to return (default 200)" }
            } }),
        ),
        tool(
            "edit_note",
            "Replace an exact passage of a note with new text. `old_text` must match exactly once (or set `replace_all`): read the note first and quote it.",
            json!({ "type": "object", "properties": {
                "note": note_arg,
                "old_text": { "type": "string", "description": "The exact text to replace" },
                "new_text": { "type": "string", "description": "What replaces it" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence" }
            }, "required": ["note", "old_text", "new_text"] }),
        ),
        tool(
            "append_note",
            "Add text at the end of a note (creating it if missing; give a vault path for a new one), or at the end of the section under a heading.",
            json!({ "type": "object", "properties": {
                "note": note_arg,
                "text": { "type": "string" },
                "heading": { "type": "string", "description": "Add under this heading instead of at the end" }
            }, "required": ["note", "text"] }),
        ),
        tool(
            "set_task",
            "Tick or untick the task on a line of a note, and/or set its due date (written `📅 YYYY-MM-DD` at the end; null removes it).",
            json!({ "type": "object", "properties": {
                "note": note_arg,
                "line": { "type": "integer", "description": "The task's line, from 1 (as `tasks` and `search` show)" },
                "done": { "type": "boolean" },
                "due": { "type": ["string", "null"], "description": "YYYY-MM-DD, or null to remove the due date" }
            }, "required": ["note", "line"] }),
        ),
        tool(
            "current_note",
            "What the window shows: the open note's path, the mode (reading or source), whether it has unsaved edits, and whether they conflict with the disk copy (changed, or deleted or moved, on disk).",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{key}`"))
}

/// `body` with `old` replaced by `new` (every occurrence with `all`, else
/// exactly one), and how many were replaced; the refusal says why.
fn replace_exact(body: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    let n = if old.is_empty() { 0 } else { body.matches(old).count() };
    match n {
        0 => Err("old_text was not found; read the note and quote it exactly".into()),
        n if n > 1 && !all => Err(format!("old_text occurs {n} times; quote more of it, or set replace_all")),
        n => Ok(if all { (body.replace(old, new), n) } else { (body.replacen(old, new, 1), 1) }),
    }
}

fn limit_arg(args: &Value, default: usize) -> usize {
    args.get("limit").and_then(Value::as_u64).map(|n| n.clamp(1, 200) as usize).unwrap_or(default)
}

impl NotesApp {
    pub(crate) fn mcp_call(&mut self, call: McpToolCall) {
        let result = self.mcp_dispatch(&call.name, &call.arguments);
        let _ = call.reply.send(result);
    }

    /// Refuse to write a note the window holds unsaved edits to: the write
    /// would turn the user's typing into a conflict.
    fn writable(&self, path: &str) -> Result<(), String> {
        if self.current.as_deref() == Some(path) && self.dirty() {
            return Err(format!("{path} is open in the window with unsaved edits; ask the user to save it first"));
        }
        Ok(())
    }

    /// After an agent's write: the index and the window catch up as they do
    /// for any outside edit.
    fn wrote(&mut self, path: &str) {
        let abs = ix_abs(self, path);
        self.vault_changed(vec![abs]);
    }

    fn resolve_note(&self, note: &str) -> Result<String, String> {
        let ix = self.index.as_ref().ok_or("no vault is configured")?;
        let path = ix.lookup(note).ok_or_else(|| format!("no note named `{note}`"))?;
        match ix.entry(&path).map(|e| e.kind) {
            Some(FileKind::Note) => Ok(path),
            _ => Err(format!("`{path}` is not a note")),
        }
    }

    fn mcp_dispatch(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            cce_ui::mcp::CONTEXT_TOOL => {
                let mode = if self.mode == crate::Mode::Source { "source" } else { "reading" };
                let text = match (&self.current, self.dirty()) {
                    (Some(_), true) => Some(self.editor_text()),
                    (Some(p), false) => self.index.as_ref().and_then(|ix| ix.read_text(p).ok()),
                    (None, _) => None,
                };
                Ok(note_context(self.current.as_deref(), mode, self.dirty(), self.conflict, text.as_deref()).into_result())
            }
            "current_note" => Ok(json!({
                "path": self.current,
                "mode": if self.mode == crate::Mode::Source { "source" } else { "reading" },
                "unsaved": self.dirty(),
                "conflict": self.conflict,
                "deleted_on_disk": self.gone,
            })),
            "open_note" => {
                let path = self.resolve_note(str_arg(args, "note")?)?;
                let line = args.get("line").and_then(Value::as_u64).map(|l| (l as usize).saturating_sub(1));
                self.open_path(&path, line, true);
                if self.current.as_deref() != Some(path.as_str()) {
                    return Err(self.status.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| "could not open".into()));
                }
                self.needs_rebuild = true;
                Ok(json!({ "opened": path }))
            }
            "edit_note" => {
                let path = self.resolve_note(str_arg(args, "note")?)?;
                self.writable(&path)?;
                let all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
                let ix = self.index.as_mut().ok_or("no vault is configured")?;
                let body = ix.read_text(&path).map_err(|e| e.to_string())?;
                let (edited, n) = replace_exact(&body, str_arg(args, "old_text")?, str_arg(args, "new_text")?, all)
                    .map_err(|e| format!("{path}: {e}"))?;
                ix.write_text(&path, &edited).map_err(|e| e.to_string())?;
                self.wrote(&path);
                Ok(json!({ "edited": path, "replacements": n }))
            }
            "append_note" => {
                let given = str_arg(args, "note")?;
                // An existing note by any name, or a new one by its path.
                let path = match self.resolve_note(given) {
                    Ok(p) => p,
                    Err(_) if given.trim().ends_with(".md") => given.trim().trim_start_matches('/').to_string(),
                    Err(_) => format!("{}.md", given.trim().trim_start_matches('/')),
                };
                self.writable(&path)?;
                let heading = args.get("heading").and_then(Value::as_str);
                let ix = self.index.as_mut().ok_or("no vault is configured")?;
                ix.append_under(&path, str_arg(args, "text")?, heading).map_err(|e| e.to_string())?;
                self.wrote(&path);
                Ok(json!({ "appended_to": path }))
            }
            "set_task" => {
                let path = self.resolve_note(str_arg(args, "note")?)?;
                self.writable(&path)?;
                let line = args.get("line").and_then(Value::as_u64).filter(|&l| l >= 1).ok_or("`line` is required, from 1")? as usize - 1;
                let done = args.get("done").and_then(Value::as_bool);
                let due = match args.get("due") {
                    None => None,
                    Some(Value::Null) => Some(None),
                    Some(Value::String(d)) => Some(Some(
                        chrono::NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").map_err(|_| format!("not a YYYY-MM-DD date: {d}"))?,
                    )),
                    Some(v) => return Err(format!("`due` must be YYYY-MM-DD or null, got {v}")),
                };
                if done.is_none() && due.is_none() {
                    return Err("give `done`, `due`, or both".into());
                }
                let ix = self.index.as_mut().ok_or("no vault is configured")?;
                if let Some(done) = done {
                    ix.set_task(&path, line, if done { 'x' } else { ' ' }).map_err(|e| e.to_string())?;
                }
                if let Some(due) = due {
                    ix.set_task_due(&path, line, due).map_err(|e| e.to_string())?;
                }
                self.wrote(&path);
                let now = ix_text_line(self, &path, line);
                Ok(json!({ "note": path, "line": line + 1, "now": now }))
            }
            "append_daily" => {
                let text = str_arg(args, "text")?.to_string();
                let date = match args.get("date").and_then(Value::as_str) {
                    Some(d) => Some(chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| format!("not a date: {d}"))?),
                    None => None,
                };
                let ix = self.index.as_mut().ok_or("no vault is configured")?;
                let date = date.unwrap_or_else(|| chrono::Local::now().date_naive());
                let (path, _) = ix.daily(date, true).map_err(|e| e.to_string())?;
                ix.append(&path, &text).map_err(|e| e.to_string())?;
                // The window, if it shows that note, catches up as it does
                // for any outside edit.
                self.vault_changed(vec![ix_abs(self, &path)]);
                Ok(json!({ "appended_to": path }))
            }
            _ => {
                let ix = self.index.as_ref().ok_or("no vault is configured")?;
                match name {
                    "search" => {
                        let q = str_arg(args, "query")?;
                        let limit = limit_arg(args, 20);
                        if let Some(tag) = crate::side::tag_query(q) {
                            let mut notes = ix.tagged(tag);
                            notes.truncate(limit);
                            return Ok(json!({ "tag": tag, "notes": notes }));
                        }
                        let hits: Vec<Value> = ix
                            .search(q, limit)
                            .into_iter()
                            .map(|h| {
                                json!({
                                    "path": h.path,
                                    "matching_lines": h.total,
                                    "lines": h.lines.iter().map(|l| json!({ "line": l.line + 1, "text": l.text })).collect::<Vec<_>>(),
                                })
                            })
                            .collect();
                        Ok(json!(hits))
                    }
                    "tasks" => {
                        let status = args.get("status").and_then(Value::as_str).unwrap_or("open");
                        let due_before = match args.get("due_before").and_then(Value::as_str) {
                            Some(d) => Some(chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| format!("not a date: {d}"))?),
                            None => None,
                        };
                        let within = args.get("note").and_then(Value::as_str).map(|p| p.trim().trim_start_matches('/').trim_end_matches('/'));
                        let limit = args.get("limit").and_then(Value::as_u64).map(|n| n.clamp(1, 2000) as usize).unwrap_or(200);
                        let mut out: Vec<Value> = ix
                            .tasks()
                            .filter(|(p, t)| match status {
                                "open" => t.status == ' ',
                                "done" => t.status != ' ',
                                _ => true,
                            } && within.is_none_or(|w| *p == w || p.starts_with(&format!("{w}/")) || p.strip_suffix(".md") == Some(w))
                                && due_before.is_none_or(|d| t.due.is_some_and(|due| due <= d)))
                            .map(|(p, t)| json!({
                                "note": p,
                                "line": t.line + 1,
                                "status": t.status.to_string(),
                                // The date is `due`; the text without it.
                                "text": cce_vault::parse::split_due(&t.text).0,
                                "due": t.due.map(|d| d.to_string()),
                            }))
                            .collect();
                        let total = out.len();
                        out.truncate(limit);
                        Ok(json!({ "tasks": out, "total": total }))
                    }
                    "find_notes" => {
                        let q = str_arg(args, "query")?;
                        let found: Vec<Value> = ix
                            .find(q, limit_arg(args, 10))
                            .into_iter()
                            .map(|m| json!({ "path": m.path, "matched": m.matched }))
                            .collect();
                        Ok(json!(found))
                    }
                    "read_note" => {
                        let path = self.resolve_note(str_arg(args, "note")?)?;
                        let text = ix.read_text(&path).map_err(|e| e.to_string())?;
                        Ok(json!({ "path": path, "text": text }))
                    }
                    "backlinks" => {
                        let path = self.resolve_note(str_arg(args, "note")?)?;
                        let mut linked = Vec::new();
                        let mut texts: std::collections::HashMap<&str, String> = Default::default();
                        for b in ix.backlinks(&path) {
                            let text = texts.entry(b.source).or_insert_with(|| ix.read_text(b.source).unwrap_or_default());
                            let line_text = text.lines().nth(b.link.line).unwrap_or("").trim().to_string();
                            linked.push(json!({ "source": b.source, "line": b.link.line + 1, "text": line_text }));
                        }
                        let unlinked: Vec<Value> = ix
                            .unlinked_mentions(&path)
                            .into_iter()
                            .map(|h| {
                                json!({
                                    "source": h.path,
                                    "lines": h.lines.iter().map(|l| json!({ "line": l.line + 1, "text": l.text })).collect::<Vec<_>>(),
                                })
                            })
                            .collect();
                        Ok(json!({ "note": path, "name": stem(&path), "linked": linked, "unlinked": unlinked }))
                    }
                    other => Err(format!("unknown tool `{other}`")),
                }
            }
        }
    }
}

/// Line `line` (0-based) of a note as it stands on disk.
fn ix_text_line(app: &NotesApp, path: &str, line: usize) -> Option<String> {
    let text = app.index.as_ref()?.read_text(path).ok()?;
    text.lines().nth(line).map(str::to_string)
}

fn ix_abs(app: &NotesApp, path: &str) -> std::path::PathBuf {
    app.index.as_ref().map(|ix| ix.abs(path)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::{app_over, open_and_type};

    fn call(app: &mut NotesApp, name: &str, args: Value) -> Result<Value, String> {
        app.mcp_dispatch(name, &args)
    }

    fn on_disk(dir: &tempfile::TempDir, path: &str) -> String {
        std::fs::read_to_string(dir.path().join(path)).unwrap()
    }

    #[test]
    fn edit_note_replaces_exactly_one_passage() {
        let (dir, mut app) = app_over(&[("Plan.md", "rent 900\nrent 900\nfood 300\n")]);
        let err = call(&mut app, "edit_note", json!({ "note": "Plan", "old_text": "rent 900", "new_text": "rent 950" })).unwrap_err();
        assert!(err.contains("occurs 2 times"), "{err}");
        assert!(call(&mut app, "edit_note", json!({ "note": "Plan", "old_text": "nope", "new_text": "x" })).is_err());
        call(&mut app, "edit_note", json!({ "note": "Plan", "old_text": "food 300", "new_text": "food 320" })).unwrap();
        let r = call(&mut app, "edit_note", json!({ "note": "Plan", "old_text": "rent 900", "new_text": "rent 950", "replace_all": true })).unwrap();
        assert_eq!(r["replacements"], 2);
        assert_eq!(on_disk(&dir, "Plan.md"), "rent 950\nrent 950\nfood 320\n");
    }

    #[test]
    fn append_note_goes_under_a_heading_or_makes_a_note() {
        let (dir, mut app) = app_over(&[("Plan.md", "# Plan\n## Tasks\n- [ ] a\n\n## Log\n")]);
        call(&mut app, "append_note", json!({ "note": "Plan", "text": "- [ ] b", "heading": "Tasks" })).unwrap();
        assert_eq!(on_disk(&dir, "Plan.md"), "# Plan\n## Tasks\n- [ ] a\n- [ ] b\n\n## Log\n");
        let r = call(&mut app, "append_note", json!({ "note": "Finance/Insurance", "text": "renews in March" })).unwrap();
        assert_eq!(r["appended_to"], "Finance/Insurance.md");
        assert_eq!(on_disk(&dir, "Finance/Insurance.md"), "renews in March\n");
        // The index knows the new note at once: it reads back by name.
        assert_eq!(call(&mut app, "read_note", json!({ "note": "Insurance" })).unwrap()["path"], "Finance/Insurance.md");
    }

    #[test]
    fn set_task_ticks_and_dates_and_tasks_finds_it() {
        let (dir, mut app) = app_over(&[("Plan.md", "- [ ] pay rent\n- [ ] call bank\n")]);
        call(&mut app, "set_task", json!({ "note": "Plan", "line": 2, "due": "2026-10-30" })).unwrap();
        let due = call(&mut app, "tasks", json!({ "due_before": "2026-10-31" })).unwrap();
        assert_eq!(due["total"], 1);
        assert_eq!(due["tasks"][0]["text"], "call bank");
        assert_eq!(due["tasks"][0]["due"], "2026-10-30");
        let r = call(&mut app, "set_task", json!({ "note": "Plan", "line": 1, "done": true })).unwrap();
        assert_eq!(r["now"], "- [x] pay rent");
        assert_eq!(on_disk(&dir, "Plan.md"), "- [x] pay rent\n- [ ] call bank 📅 2026-10-30\n");
        assert_eq!(call(&mut app, "tasks", json!({})).unwrap()["total"], 1);
        assert_eq!(call(&mut app, "tasks", json!({ "status": "done" })).unwrap()["tasks"][0]["line"], 1);
        assert!(call(&mut app, "set_task", json!({ "note": "Plan", "line": 1 })).is_err(), "nothing to change");
    }

    #[test]
    fn a_note_with_unsaved_edits_in_the_window_is_not_written() {
        let (dir, mut app) = app_over(&[("Plan.md", "rent 900\n")]);
        open_and_type(&mut app, "Plan.md", "typing");
        let err = call(&mut app, "edit_note", json!({ "note": "Plan", "old_text": "rent 900", "new_text": "x" })).unwrap_err();
        assert!(err.contains("unsaved edits"), "{err}");
        assert!(call(&mut app, "append_note", json!({ "note": "Plan", "text": "y" })).is_err());
        assert_eq!(on_disk(&dir, "Plan.md"), "rent 900\n");
    }

    #[test]
    fn every_tool_is_classed() {
        // An unclassed tool makes the popup ask before every call.
        assert!(server().unclassed().is_empty(), "classify these in server(): {:?}", server().unclassed());
    }

    #[test]
    fn context_quotes_the_open_note_and_says_when_none_is() {
        let c = note_context(Some("Finance/Budget.md"), "reading", false, false, Some("# Budget\n- [ ] pay rent")).into_result();
        assert_eq!(c["document"]["title"], "Budget");
        assert_eq!(c["document"]["path"], "Finance/Budget.md");
        assert_eq!(c["dirty"], false);
        assert!(c["summary"].as_str().unwrap().ends_with("- [ ] pay rent"));

        let none = note_context(None, "reading", false, false, None).into_result();
        assert!(none.get("document").is_none() && none.get("summary").is_none() && none.get("dirty").is_none());
    }
}
