//! `[[` completion in source mode: find the link being typed at the caret,
//! and write the chosen note back in the shortest form that still reaches
//! it, as Obsidian does. Positions are char indices (the TextBox's own
//! `cursor_idx` unit), not bytes.

use cce_vault::index::stem;
use cce_vault::{FileKind, Index};

/// How many choices the popup offers.
pub const LIMIT: usize = 8;

/// An open `[[` before `cursor` on the caret's line, with nothing closing
/// it yet: the char index just past the `[[`, and what has been typed
/// since. A `|` (display text) or `#` (a heading) ends completion — the
/// note part is already chosen by then.
pub fn open_link(text: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut i = cursor;
    while i >= 2 {
        let c = chars[i - 1];
        if c == '\n' || c == ']' || c == '|' || c == '#' {
            return None;
        }
        if c == '[' && chars[i - 2] == '[' {
            let query: String = chars[i..cursor].iter().collect();
            return Some((i, query));
        }
        i -= 1;
    }
    None
}

/// What to write between the brackets for `path`: its bare name when that
/// resolves back to it from `from`, else its vault path; `.md` dropped,
/// other extensions kept.
pub fn link_text(ix: &Index, from: Option<&str>, path: &str) -> String {
    let is_note = FileKind::of(path) == FileKind::Note;
    let file = path.rsplit('/').next().unwrap_or(path);
    let short = if is_note { stem(path).to_string() } else { file.to_string() };
    if ix.resolve_text(from, &short).as_deref() == Some(path) {
        short
    } else if is_note {
        path.strip_suffix(".md").unwrap_or(path).to_string()
    } else {
        path.to_string()
    }
}

/// Choices for a query: fuzzy over names, aliases and paths.
pub fn choices(ix: &Index, query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        let mut notes: Vec<(&String, u64)> =
            ix.files().iter().filter(|(_, e)| e.kind == FileKind::Note).map(|(p, e)| (p, e.mtime)).collect();
        notes.sort_by(|a, b| b.1.cmp(&a.1));
        return notes.into_iter().take(LIMIT).map(|(p, _)| p.clone()).collect();
    }
    ix.find(query, LIMIT).into_iter().map(|m| m.path).collect()
}

/// Replace chars `start..cursor` with `link`, closing it with `]]` unless
/// the text already closes it there. Returns the new text and the caret
/// just past the closing brackets.
pub fn apply(text: &str, start: usize, cursor: usize, link: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let closed = chars.get(cursor) == Some(&']') && chars.get(cursor + 1) == Some(&']');
    let mut out: String = chars[..start].iter().collect();
    out.push_str(link);
    if !closed {
        out.push_str("]]");
    }
    let caret = start + link.chars().count() + 2;
    out.extend(&chars[cursor..]);
    (out, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_open_link_at_the_caret() {
        let t = "see [[Pro";
        assert_eq!(open_link(t, 9), Some((6, "Pro".into())));
        assert_eq!(open_link("x [[", 4), Some((4, String::new())));
        assert_eq!(open_link("[[A]] b", 7), None);
        assert_eq!(open_link("[[A|sh", 6), None);
        assert_eq!(open_link("[[A#h", 5), None);
        assert_eq!(open_link("[[A\nb", 5), None);
        assert_eq!(open_link("[x", 2), None);
        // Non-ASCII before the link: indices are chars.
        assert_eq!(open_link("é [[ab", 6), Some((4, "ab".into())));
    }

    #[test]
    fn applies_and_closes_once() {
        assert_eq!(apply("see [[Pro", 6, 9, "Projects"), ("see [[Projects]]".into(), 16));
        assert_eq!(apply("[[Pro]] x", 2, 5, "Projects"), ("[[Projects]] x".into(), 12));
        assert_eq!(apply("é [[a tail", 4, 5, "Alpha"), ("é [[Alpha]] tail".into(), 11));
    }

    #[test]
    fn link_text_is_shortest_that_resolves() {
        let dir = tempfile::tempdir().unwrap();
        for p in ["a/Note.md", "b/Note.md", "Solo.md", "img/pic.png"] {
            let abs = dir.path().join(p);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, "x").unwrap();
        }
        let ix = Index::open(dir.path(), false).unwrap();
        assert_eq!(link_text(&ix, None, "Solo.md"), "Solo");
        assert_eq!(link_text(&ix, None, "img/pic.png"), "pic.png");
        // Two notes share the name: from a/, the bare name reaches a/Note.
        assert_eq!(link_text(&ix, Some("a/Other.md"), "a/Note.md"), "Note");
        assert_eq!(link_text(&ix, Some("a/Other.md"), "b/Note.md"), "b/Note");
    }
}
