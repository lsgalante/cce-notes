//! Where a note's GFM tables are, for handing the note to cce-sheets.
//!
//! The rule is cce-sheets' own (`cce-sheets/src/md.rs`, `tables`), kept in
//! step by hand so "Edit tables in Sheets" is offered exactly when cce-sheets
//! will find something to open: a line with a pipe, then a delimiter line of
//! as many `---` cells (`:` marks allowed), then body lines until a blank
//! line, a line without a pipe or a fence. Fenced code (``` / ~~~) is
//! skipped. Only the line ranges are needed here.

use std::ops::Range;

/// Each table's source lines (0-based, end exclusive), in order.
pub fn table_lines(text: &str) -> Vec<Range<usize>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(f) = fence {
            if line.trim_start().starts_with(f) {
                fence = None;
            }
            i += 1;
            continue;
        }
        if let Some(f) = is_fence(line) {
            fence = Some(f);
            i += 1;
            continue;
        }
        let delim = lines.get(i + 1).and_then(|l| delimiter_cells(l));
        if line.contains('|') && delim == Some(cells(line).len()) {
            let mut end = i + 2;
            while end < lines.len() {
                let l = lines[end];
                if l.trim().is_empty() || !l.contains('|') || is_fence(l).is_some() {
                    break;
                }
                end += 1;
            }
            out.push(i..end);
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// Whether source `line` is inside one of `tables`.
pub fn in_table(tables: &[Range<usize>], line: usize) -> bool {
    tables.iter().any(|t| t.contains(&line))
}

fn is_fence(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// A row's cells, as cce-sheets splits it: outer pipes optional, `\|` is
/// not a separator.
fn cells(line: &str) -> Vec<String> {
    let s = line.trim();
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = if s.ends_with('|') && !s.ends_with("\\|") { &s[..s.len() - 1] } else { s };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// How many cells a delimiter line has, or `None` if it is not one.
fn delimiter_cells(line: &str) -> Option<usize> {
    if !line.contains('-') {
        return None;
    }
    let cells = cells(line);
    cells
        .iter()
        .all(|c| {
            let dashes = c.trim().trim_matches(':');
            !dashes.is_empty() && dashes.chars().all(|ch| ch == '-')
        })
        .then_some(cells.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tables_and_skips_fences() {
        let note = "# Costs\n\
                    \n\
                    | Item | Price |\n\
                    |:-----|------:|\n\
                    | Tea  | 3     |\n\
                    | Cake | 4 |\n\
                    \n\
                    ```\n\
                    | a | b |\n\
                    |---|---|\n\
                    ```\n\
                    a | b\n\
                    --|--\n\
                    1 | 2\n\
                    after\n";
        assert_eq!(table_lines(note), vec![2..6, 11..14]);
        let t = table_lines(note);
        assert!(in_table(&t, 2) && in_table(&t, 5) && !in_table(&t, 6) && !in_table(&t, 8));
    }

    #[test]
    fn not_tables() {
        // A rule under a sentence with a pipe, mismatched widths, no pipes.
        assert!(table_lines("a | b\n---\n").is_empty());
        assert!(table_lines("| a | b |\n|---|\n").is_empty());
        assert!(table_lines("a\n---\nb\n").is_empty());
        assert!(table_lines("plain text\n").is_empty());
        // An escaped pipe is not a column.
        assert_eq!(table_lines("| a \\| b |\n|---|\n| x |\n"), vec![0..3]);
    }
}
