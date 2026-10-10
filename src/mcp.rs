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

use cce_ui::mcp::{McpTool, McpToolCall};
use cce_vault::index::stem;
use cce_vault::FileKind;
use serde_json::{json, Value};

use crate::{Message, NotesApp};

const DEFAULT_PORT: u16 = 3002;

pub fn start(sender: calloop::channel::Sender<Message>) {
    let port = std::env::var("CCE_NOTES_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    cce_ui::mcp::start_mcp_server("cce-notes", port, tools(), sender, Message::Mcp);
}

fn tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, input_schema: Value| McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    let note_arg = json!({ "type": "string", "description": "A note name, vault path, or path without .md" });
    vec![
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
            "current_note",
            "What the window shows: the open note's path, the mode (reading or source), whether it has unsaved edits, and whether they conflict with the disk copy (changed, or deleted or moved, on disk).",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{key}`"))
}

fn limit_arg(args: &Value, default: usize) -> usize {
    args.get("limit").and_then(Value::as_u64).map(|n| n.clamp(1, 200) as usize).unwrap_or(default)
}

impl NotesApp {
    pub(crate) fn mcp_call(&mut self, call: McpToolCall) {
        let result = self.mcp_dispatch(&call.name, &call.arguments);
        let _ = call.reply.send(result);
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

fn ix_abs(app: &NotesApp, path: &str) -> std::path::PathBuf {
    app.index.as_ref().map(|ix| ix.abs(path)).unwrap_or_default()
}
