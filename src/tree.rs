//! The file tree pane's model: the vault's notes as folder and note rows,
//! folders first and each level sorted case-insensitively, as Obsidian's
//! file explorer shows them. Folders exist only as the parents of indexed
//! files, so an empty folder does not appear.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The folder's or note's vault path.
    pub path: String,
    /// What the row shows: a folder's name, or a note's name without `.md`.
    pub name: String,
    pub depth: usize,
    pub folder: bool,
    pub open: bool,
}

#[derive(Default)]
pub struct Tree {
    open: BTreeSet<String>,
}

#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<String, Dir<'a>>,
    files: Vec<&'a str>,
}

impl Tree {
    pub fn toggle(&mut self, folder: &str) {
        if !self.open.remove(folder) {
            self.open.insert(folder.to_string());
        }
    }

    /// Open every folder above `path`, so its row is visible.
    pub fn reveal(&mut self, path: &str) {
        let mut at = 0;
        while let Some(i) = path[at..].find('/') {
            self.open.insert(path[..at + i].to_string());
            at += i + 1;
        }
    }

    /// The visible rows for these note paths.
    pub fn rows<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<Row> {
        let mut root = Dir::default();
        for p in paths {
            let mut dir = &mut root;
            let mut parts: Vec<&str> = p.split('/').collect();
            parts.pop();
            for part in parts {
                dir = dir.dirs.entry(part.to_string()).or_default();
            }
            dir.files.push(p);
        }
        let mut out = Vec::new();
        self.walk(&root, "", 0, &mut out);
        out
    }

    fn walk(&self, dir: &Dir, prefix: &str, depth: usize, out: &mut Vec<Row>) {
        let mut dirs: Vec<(&String, &Dir)> = dir.dirs.iter().collect();
        dirs.sort_by_key(|(name, _)| name.to_lowercase());
        for (name, sub) in dirs {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let open = self.open.contains(&path);
            out.push(Row { path: path.clone(), name: name.clone(), depth, folder: true, open });
            if open {
                self.walk(sub, &path, depth + 1, out);
            }
        }
        let mut files = dir.files.clone();
        files.sort_by_key(|p| p.to_lowercase());
        for p in files {
            let file = p.rsplit('/').next().unwrap_or(p);
            let name = file.strip_suffix(".md").unwrap_or(file).to_string();
            out.push(Row { path: p.to_string(), name, depth, folder: false, open: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| format!("{}{}{}", "  ".repeat(r.depth), r.name, if r.folder { "/" } else { "" })).collect()
    }

    #[test]
    fn folders_first_sorted_and_collapsed() {
        let paths = ["b.md", "A.md", "Tasks/x.md", "archive/old/y.md"];
        let mut t = Tree::default();
        assert_eq!(names(&t.rows(paths)), ["archive/", "Tasks/", "A", "b"]);
        t.toggle("Tasks");
        assert_eq!(names(&t.rows(paths)), ["archive/", "Tasks/", "  x", "A", "b"]);
        t.toggle("Tasks");
        t.reveal("archive/old/y.md");
        assert_eq!(names(&t.rows(paths)), ["archive/", "  old/", "    y", "Tasks/", "A", "b"]);
        assert!(t.rows(paths)[1].open);
    }
}
