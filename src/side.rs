//! What the side panes list, built from the index: a note's backlinks and
//! unlinked mentions, its outline, and full-text or tag search results.
//! Pure functions over `Index`, so they test without a window.

use std::collections::BTreeMap;

use cce_vault::index::stem;
use cce_vault::Index;

use crate::panel::{Action, Item};

/// Search results kept per query; the pane is for finding, not listing.
const SEARCH_LIMIT: usize = 50;

/// Linked mentions (grouped by note, each with its lines), then unlinked
/// mentions — Obsidian's backlinks pane.
pub fn backlinks(ix: &Index, path: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for b in ix.backlinks(path) {
        if b.source == path {
            continue;
        }
        by_source.entry(b.source).or_default().push(b.link.line);
    }
    let count: usize = by_source.len();
    out.push(Item::header(format!("Linked mentions ({count})")));
    if by_source.is_empty() {
        out.push(Item::note("No backlinks"));
    }
    for (source, mut lines) in by_source {
        lines.sort_unstable();
        lines.dedup();
        out.push(Item::title(stem(source), Action::Open { path: source.to_string(), line: lines.first().copied() }));
        let text = ix.read_text(source).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        for line in lines {
            // A canvas link has no line of its own worth showing.
            let Some(t) = all.get(line).map(|t| t.trim()).filter(|t| !t.is_empty()) else { continue };
            out.push(Item::line(1, t.to_string(), Action::Open { path: source.to_string(), line: Some(line) }));
        }
    }
    let mentions = ix.unlinked_mentions(path);
    out.push(Item::header(format!("Unlinked mentions ({})", mentions.len())));
    for hits in mentions {
        let first = hits.lines.first().map(|l| l.line);
        out.push(Item::title(stem(&hits.path), Action::Open { path: hits.path.clone(), line: first }));
        for l in hits.lines {
            out.push(Item::line(1, l.text, Action::Open { path: hits.path.clone(), line: Some(l.line) }));
        }
    }
    out
}

/// The note's headings, indented by level.
pub fn outline(ix: &Index, path: &str) -> Vec<Item> {
    let Some(note) = ix.note(path) else { return vec![Item::note("Not a note")] };
    if note.headings.is_empty() {
        return vec![Item::note("No headings")];
    }
    let top = note.headings.iter().map(|h| h.level).min().unwrap_or(1);
    note.headings
        .iter()
        .map(|h| Item::line((h.level - top) as usize, h.text.clone(), Action::Line(h.line)))
        .collect()
}

/// A search query's results: `#tag` or `tag:tag` lists the notes carrying
/// it (nested tags included); anything else is full text, every word
/// required, grouped by note with the matching lines.
pub fn search(ix: &Index, query: &str) -> Vec<Item> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    if let Some(tag) = tag_query(q) {
        let mut notes = ix.tagged(tag);
        notes.sort_by_key(|p| p.to_lowercase());
        let mut out = vec![Item::header(format!("#{tag} — {}", plural(notes.len(), "note")))];
        out.extend(notes.into_iter().map(|p| Item::title(stem(p), Action::Open { path: p.to_string(), line: None })));
        return out;
    }
    let hits = ix.search(q, SEARCH_LIMIT);
    let mut out = vec![Item::header(plural(hits.len(), "note"))];
    for h in hits {
        let first = h.lines.first().map(|l| l.line);
        let title = if h.total > h.lines.len() {
            format!("{}  ({} lines)", stem(&h.path), h.total)
        } else {
            stem(&h.path).to_string()
        };
        out.push(Item::title(title, Action::Open { path: h.path.clone(), line: first }));
        for l in h.lines {
            out.push(Item::line(1, l.text, Action::Open { path: h.path.clone(), line: Some(l.line) }));
        }
    }
    out
}

/// The tag a query asks for, if it is a tag query.
pub fn tag_query(q: &str) -> Option<&str> {
    let t = q.strip_prefix("tag:").or_else(|| q.strip_prefix('#'))?.trim().trim_start_matches('#');
    (!t.is_empty() && !t.contains(char::is_whitespace)).then_some(t)
}

pub fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::Kind;

    fn vault(files: &[(&str, &str)]) -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        for (p, t) in files {
            let abs = dir.path().join(p);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, t).unwrap();
        }
        let ix = Index::open(dir.path(), false).unwrap();
        (dir, ix)
    }

    fn texts(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| format!("{}{}", "  ".repeat(i.depth), i.text)).collect()
    }

    #[test]
    fn backlinks_group_by_note_with_lines_then_mentions() {
        let (_d, ix) = vault(&[
            ("Target.md", "# T\n"),
            ("a/One.md", "intro\nsee [[Target]] here\nand [[Target#T]] again\n"),
            ("Two.md", "about Target, unlinked\n"),
        ]);
        let items = backlinks(&ix, "Target.md");
        assert_eq!(
            texts(&items),
            [
                "Linked mentions (1)",
                "One",
                "  see [[Target]] here",
                "  and [[Target#T]] again",
                "Unlinked mentions (1)",
                "Two",
                "  about Target, unlinked",
            ]
        );
        assert_eq!(items[2].action, Some(Action::Open { path: "a/One.md".into(), line: Some(1) }));
        assert_eq!(items[0].kind, Kind::Header);
    }

    #[test]
    fn outline_indents_from_the_top_level() {
        let (_d, ix) = vault(&[("N.md", "## A\ntext\n### B\n## C\n")]);
        let items = outline(&ix, "N.md");
        assert_eq!(texts(&items), ["A", "  B", "C"]);
        assert_eq!(items[1].action, Some(Action::Line(2)));
        let (_d, ix) = vault(&[("E.md", "no headings\n")]);
        assert_eq!(outline(&ix, "E.md")[0].kind, Kind::Note);
    }

    #[test]
    fn search_text_and_tags() {
        let (_d, ix) = vault(&[
            ("A.md", "apple pie\nbanana\n#fruit/red\n"),
            ("B.md", "apple tart #fruit\n"),
            ("C.md", "nothing\n"),
        ]);
        let items = search(&ix, "apple");
        assert_eq!(items[0].text, "2 notes");
        assert!(texts(&items).contains(&"  apple pie".to_string()));
        let tags = search(&ix, "#fruit");
        assert_eq!(texts(&tags), ["#fruit — 2 notes", "A", "B"]);
        assert_eq!(texts(&search(&ix, "tag:fruit/red")), ["#fruit/red — 1 note", "A"]);
        assert!(search(&ix, "  ").is_empty());
        assert_eq!(tag_query("#a b"), None);
    }
}
