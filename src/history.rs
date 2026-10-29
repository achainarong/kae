//! Read-only history presentation: tree identity, patch alignment, and literal
//! search. No filesystem writes or Git commands belong in this module.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use regex::{Regex, RegexBuilder};

use crate::git::CommittedFile;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffLayout {
    #[default]
    Unified,
    SideBySide,
}

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub path: PathBuf,
    pub name: OsString,
    pub depth: usize,
    pub file: Option<usize>,
    pub count: usize,
}

#[derive(Default)]
struct Directory {
    directories: BTreeMap<OsString, Directory>,
    files: BTreeMap<OsString, usize>,
}

pub struct FileTree {
    all: Vec<TreeRow>,
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub collapsed: HashSet<PathBuf>,
    order: Vec<usize>,
}

impl FileTree {
    pub fn new(files: &[CommittedFile]) -> Self {
        Self::build(files, false)
    }

    pub fn compact(files: &[CommittedFile]) -> Self {
        Self::build(files, true)
    }

    /// Add a presentation-only repository root. Empty paths identify the root;
    /// Git actions still receive only the actual changed-file paths.
    pub fn with_root(mut self, name: OsString) -> Self {
        for row in &mut self.all {
            row.depth += 1;
        }
        self.all.insert(
            0,
            TreeRow {
                path: PathBuf::new(),
                name,
                depth: 0,
                file: None,
                count: self.order.len(),
            },
        );
        let selected = self
            .rows
            .get(self.cursor)
            .map(|row| (row.path.clone(), row.file.is_none()));
        self.rebuild(selected);
        self
    }

    fn build(files: &[CommittedFile], compact: bool) -> Self {
        let mut root = Directory::default();
        for (index, file) in files.iter().enumerate() {
            let mut components = file.path.iter().peekable();
            let mut directory = &mut root;
            while let Some(component) = components.next() {
                if components.peek().is_none() {
                    directory.files.insert(component.to_owned(), index);
                } else {
                    directory = directory
                        .directories
                        .entry(component.to_owned())
                        .or_default();
                }
            }
        }
        let mut all = Vec::new();
        fn flatten(
            directory: Directory,
            parent: &Path,
            depth: usize,
            rows: &mut Vec<TreeRow>,
            compact: bool,
        ) -> usize {
            let mut count = directory.files.len();
            for (mut name, mut child) in directory.directories {
                let mut path = parent.join(&name);
                while compact && child.files.is_empty() && child.directories.len() == 1 {
                    let (next, nested) = child.directories.into_iter().next().expect("one child");
                    path.push(&next);
                    name.push("/");
                    name.push(next);
                    child = nested;
                }
                let row = rows.len();
                rows.push(TreeRow {
                    path: path.clone(),
                    name,
                    depth,
                    file: None,
                    count: 0,
                });
                let children = flatten(child, &path, depth + 1, rows, compact);
                rows[row].count = children;
                count += children;
            }
            for (name, index) in directory.files {
                rows.push(TreeRow {
                    path: parent.join(&name),
                    name,
                    depth,
                    file: Some(index),
                    count: 1,
                });
            }
            count
        }
        flatten(root, Path::new(""), 0, &mut all, compact);
        let order = all.iter().filter_map(|row| row.file).collect();
        let mut tree = Self {
            rows: all.clone(),
            all,
            cursor: 0,
            collapsed: HashSet::new(),
            order,
        };
        if let Some(first) = tree.order.first().copied() {
            tree.select_file(first);
        }
        tree
    }

    fn rebuild(&mut self, selected: Option<(PathBuf, bool)>) {
        self.rows.clear();
        let mut hidden_depth = None;
        for row in &self.all {
            if let Some(depth) = hidden_depth {
                if row.depth > depth {
                    continue;
                }
                hidden_depth = None;
            }
            self.rows.push(row.clone());
            if row.file.is_none() && self.collapsed.contains(&row.path) {
                hidden_depth = Some(row.depth);
            }
        }
        self.cursor = selected
            .and_then(|(path, directory)| {
                self.rows
                    .iter()
                    .position(|row| row.path == path && row.file.is_none() == directory)
            })
            .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1)));
    }

    pub fn selected(&self) -> Option<&TreeRow> {
        self.rows.get(self.cursor)
    }

    pub fn navigate(&mut self, delta: isize) -> Option<usize> {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        self.selected().and_then(|row| row.file)
    }

    pub fn toggle(&mut self) -> bool {
        let Some(row) = self.selected() else {
            return false;
        };
        if row.file.is_some() {
            return false;
        }
        let path = row.path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path.clone());
        }
        self.rebuild(Some((path, true)));
        true
    }

    pub fn expand(&mut self) -> Option<usize> {
        let row = self.selected()?;
        if row.file.is_some() {
            return row.file;
        }
        let path = row.path.clone();
        if self.collapsed.remove(&path) {
            self.rebuild(Some((path, true)));
            return None;
        }
        self.navigate(1)
    }

    pub fn collapse(&mut self) {
        let Some(row) = self.selected() else {
            return;
        };
        let path = row.path.clone();
        if row.file.is_none() && !self.collapsed.contains(&path) {
            self.collapsed.insert(path.clone());
            self.rebuild(Some((path, true)));
            return;
        }
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.file.is_none() && row.path != path && path.starts_with(&row.path)
            })
            .max_by_key(|(_, row)| row.depth)
            .map(|(index, _)| index)
        {
            self.cursor = index;
        }
    }

    pub fn select_file(&mut self, index: usize) {
        let Some(row) = self.all.iter().find(|row| row.file == Some(index)) else {
            return;
        };
        let path = row.path.clone();
        for parent in path.ancestors().skip(1) {
            self.collapsed.remove(parent);
        }
        self.rebuild(Some((path, false)));
    }

    pub fn step_file(&mut self, current: usize, backwards: bool) -> Option<usize> {
        let position = self.order.iter().position(|index| *index == current)?;
        let next = if backwards {
            position.saturating_sub(1)
        } else {
            (position + 1).min(self.order.len() - 1)
        };
        let file = self.order[next];
        self.select_file(file);
        Some(file)
    }

    pub fn restore_collapsed(
        &mut self,
        collapsed: HashSet<PathBuf>,
        active: usize,
        cursor: Option<(PathBuf, bool)>,
    ) {
        self.collapsed = collapsed;
        let selected = cursor.or_else(|| {
            self.all
                .iter()
                .find(|row| row.file == Some(active))
                .map(|row| (row.path.clone(), false))
        });
        self.rebuild(selected);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    Meta,
    Hunk,
    Context,
    Delete,
    Insert,
}

#[derive(Clone, Debug)]
pub struct DiffCell {
    pub text: String,
    pub number: Option<usize>,
    pub source: usize,
    /// Number of patch-prefix bytes removed from `text`.
    pub prefix: usize,
    pub kind: CellKind,
    pub annotation: Option<(usize, String)>,
}

#[derive(Clone, Debug)]
pub struct SplitRow {
    pub left: Option<DiffCell>,
    pub right: Option<DiffCell>,
    pub anchor: usize,
}

#[derive(Default)]
pub struct Document {
    pub lines: Vec<String>,
    pub rows: Vec<SplitRow>,
    source_rows: Vec<usize>,
    pub hunks: Vec<usize>,
    pub split_note: Option<&'static str>,
}

impl Document {
    pub fn full(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_owned).collect(),
            split_note: Some("full file"),
            ..Self::default()
        }
    }

    pub fn patch(text: &str) -> Self {
        let mut document = Self {
            lines: text.lines().map(str::to_owned).collect(),
            ..Self::default()
        };
        document.source_rows = vec![0; document.lines.len()];
        if !document.align() {
            document.rows.clear();
            document.source_rows.clear();
            document.split_note = Some("incomplete or unsupported patch");
        } else if document.hunks.is_empty() {
            document.rows.clear();
            document.source_rows.clear();
            document.split_note = Some("metadata / binary change");
        }
        document
    }

    pub fn can_split(&self) -> bool {
        !self.rows.is_empty() && self.split_note.is_none()
    }

    fn align(&mut self) -> bool {
        static HUNK: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid hunk pattern")
        });
        let mut old = 0usize;
        let mut new = 0usize;
        let mut remaining = (0usize, 0usize);
        let mut in_hunk = false;
        let mut deleted: Vec<DiffCell> = Vec::new();
        let mut inserted: Vec<DiffCell> = Vec::new();
        let mut previous = None;
        for source in 0..self.lines.len() {
            let line = self.lines[source].clone();
            if let Some(captures) = HUNK.captures(&line) {
                if remaining != (0, 0) {
                    return false;
                }
                self.flush(&mut deleted, &mut inserted);
                let number = |index| {
                    captures
                        .get(index)
                        .map(|value| value.as_str().parse::<usize>())
                        .transpose()
                };
                let (Ok(Some(a)), Ok(b), Ok(Some(c)), Ok(d)) =
                    (number(1), number(2), number(3), number(4))
                else {
                    return false;
                };
                old = a;
                new = c;
                remaining = (b.unwrap_or(1), d.unwrap_or(1));
                if (remaining.0 > 0 && old == 0) || (remaining.1 > 0 && new == 0) {
                    return false;
                }
                in_hunk = true;
                previous = None;
                self.hunks.push(source);
                self.meta(line, source, CellKind::Hunk);
                continue;
            }
            if line == "\\ No newline at end of file" && in_hunk {
                let target = match previous {
                    Some(CellKind::Delete) => deleted.last_mut(),
                    Some(CellKind::Insert) => inserted.last_mut(),
                    Some(CellKind::Context) => {
                        if let Some(row) = self.rows.last_mut() {
                            if let Some(left) = &mut row.left {
                                left.annotation = Some((source, line.clone()));
                            }
                            if let Some(right) = &mut row.right {
                                right.annotation = Some((source, line.clone()));
                            }
                            self.source_rows[source] = self.rows.len() - 1;
                        }
                        None
                    }
                    _ => return false,
                };
                if let Some(target) = target {
                    target.annotation = Some((source, line));
                }
                continue;
            }
            let tag = line.as_bytes().first().copied();
            if in_hunk && remaining != (0, 0) {
                let kind = match tag {
                    Some(b'-') => CellKind::Delete,
                    Some(b'+') => CellKind::Insert,
                    Some(b' ') => CellKind::Context,
                    _ => return false,
                };
                let mut cell = DiffCell {
                    text: line[1..].to_owned(),
                    number: None,
                    source,
                    prefix: 1,
                    kind,
                    annotation: None,
                };
                match kind {
                    CellKind::Delete => {
                        if remaining.0 == 0 {
                            return false;
                        }
                        remaining.0 -= 1;
                        cell.number = Some(old);
                        old = old.saturating_add(1);
                        deleted.push(cell);
                    }
                    CellKind::Insert => {
                        if remaining.1 == 0 {
                            return false;
                        }
                        remaining.1 -= 1;
                        cell.number = Some(new);
                        new = new.saturating_add(1);
                        inserted.push(cell);
                    }
                    CellKind::Context => {
                        if remaining.0 == 0 || remaining.1 == 0 {
                            return false;
                        }
                        remaining.0 -= 1;
                        remaining.1 -= 1;
                        self.flush(&mut deleted, &mut inserted);
                        let mut left = cell.clone();
                        left.number = Some(old);
                        cell.number = Some(new);
                        old = old.saturating_add(1);
                        new = new.saturating_add(1);
                        self.push(SplitRow {
                            left: Some(left),
                            right: Some(cell),
                            anchor: source,
                        });
                    }
                    _ => unreachable!(),
                }
                previous = Some(kind);
            } else {
                self.flush(&mut deleted, &mut inserted);
                in_hunk = false;
                previous = None;
                if line.starts_with("@@") || line == "[Preview truncated at 256 KiB]" {
                    return false;
                }
                self.meta(line, source, CellKind::Meta);
            }
        }
        self.flush(&mut deleted, &mut inserted);
        remaining == (0, 0)
    }

    fn meta(&mut self, text: String, source: usize, kind: CellKind) {
        let cell = DiffCell {
            text,
            number: None,
            source,
            prefix: 0,
            kind,
            annotation: None,
        };
        self.push(SplitRow {
            left: Some(cell.clone()),
            right: Some(cell),
            anchor: source,
        });
    }

    fn flush(&mut self, deleted: &mut Vec<DiffCell>, inserted: &mut Vec<DiffCell>) {
        let mut left = std::mem::take(deleted).into_iter();
        let mut right = std::mem::take(inserted).into_iter();
        loop {
            let (left, right) = (left.next(), right.next());
            if left.is_none() && right.is_none() {
                break;
            }
            let anchor = left
                .as_ref()
                .map(|cell| cell.source)
                .into_iter()
                .chain(right.as_ref().map(|cell| cell.source))
                .min()
                .unwrap_or(0);
            self.push(SplitRow {
                left,
                right,
                anchor,
            });
        }
    }

    fn push(&mut self, row: SplitRow) {
        for cell in row.left.iter().chain(row.right.iter()) {
            self.source_rows[cell.source] = self.rows.len();
            if let Some((source, _)) = &cell.annotation {
                self.source_rows[*source] = self.rows.len();
            }
        }
        self.rows.push(row);
    }

    pub fn row_for(&self, source: usize) -> usize {
        self.source_rows
            .get(source)
            .copied()
            .unwrap_or_else(|| self.rows.len().saturating_sub(1))
    }

    pub fn step(&self, source: usize, delta: isize, split: bool) -> usize {
        if split && self.can_split() {
            let current = self.row_for(source);
            let row = current
                .saturating_add_signed(delta)
                .min(self.rows.len().saturating_sub(1));
            if row == current {
                return source.min(self.lines.len().saturating_sub(1));
            }
            self.rows.get(row).map(|row| row.anchor).unwrap_or(0)
        } else {
            source
                .saturating_add_signed(delta)
                .min(self.lines.len().saturating_sub(1))
        }
    }

    pub fn last(&self, split: bool) -> usize {
        if split && self.can_split() {
            self.rows.last().map(|row| row.anchor).unwrap_or(0)
        } else {
            self.lines.len().saturating_sub(1)
        }
    }
}

pub struct HistoricalPreview {
    pub text: String,
    pub document: Document,
}
impl HistoricalPreview {
    pub fn new(text: String, full: bool) -> Self {
        let document = if full {
            Document::full(&text)
        } else {
            Document::patch(&text)
        };
        Self { text, document }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: usize,
    pub bytes: Range<usize>,
}

#[derive(Clone, Default)]
pub struct Search {
    pub query: String,
    pub editing: Option<String>,
    pub matches: Vec<SearchMatch>,
    pub active: Option<usize>,
}

impl Search {
    pub fn reindex(&mut self, document: &Document, near: usize) {
        self.matches.clear();
        self.active = None;
        if self.query.is_empty() {
            return;
        }
        let Ok(pattern) = RegexBuilder::new(&regex::escape(&self.query))
            .case_insensitive(true)
            .build()
        else {
            return;
        };
        for (line, text) in document.lines.iter().enumerate() {
            self.matches
                .extend(pattern.find_iter(text).map(|matched| SearchMatch {
                    line,
                    bytes: matched.range(),
                }));
        }
        if !self.matches.is_empty() {
            self.active = Some(
                self.matches
                    .iter()
                    .position(|matched| matched.line >= near)
                    .unwrap_or(0),
            );
        }
    }

    pub fn begin(&mut self) {
        self.editing = Some(self.query.clone());
    }
    pub fn accept(&mut self, document: &Document, near: usize) {
        if let Some(query) = self.editing.take() {
            self.query = query;
            self.reindex(document, near);
        }
    }
    pub fn current(&self) -> Option<&SearchMatch> {
        self.active.and_then(|index| self.matches.get(index))
    }
    pub fn next(&mut self, backwards: bool) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            return None;
        }
        let index = self.active.unwrap_or(0);
        self.active =
            Some((index + if backwards { self.matches.len() - 1 } else { 1 }) % self.matches.len());
        self.current()
    }
    pub fn on_line(&self, source: usize) -> &[SearchMatch] {
        let start = self
            .matches
            .partition_point(|matched| matched.line < source);
        let end = self
            .matches
            .partition_point(|matched| matched.line <= source);
        &self.matches[start..end]
    }
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = &mut self.editing {
            for character in text.chars().filter(|character| !character.is_control()) {
                if input.len() + character.len_utf8() > 1024 {
                    break;
                }
                input.push(character);
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Position {
    pub source: usize,
    pub horizontal: usize,
}

pub struct History {
    pub tree: FileTree,
    pub document: Document,
    pub search: Search,
    pub layout: DiffLayout,
    pub loading: bool,
    pub positions: HashMap<(PathBuf, bool), Position>,
}

#[derive(Clone)]
pub struct Preferences {
    pub collapsed: HashSet<PathBuf>,
    pub active: Option<PathBuf>,
    pub layout: DiffLayout,
    pub full: bool,
    pub query: String,
    pub cursor: Option<(PathBuf, bool)>,
    pub preview_focus: bool,
}

impl History {
    pub fn new(files: &[CommittedFile]) -> Self {
        Self {
            tree: FileTree::new(files),
            document: Document::default(),
            search: Search::default(),
            layout: DiffLayout::Unified,
            loading: true,
            positions: HashMap::new(),
        }
    }
    pub fn split(&self, width: u16, full: bool) -> bool {
        width >= 120 && !full && self.layout == DiffLayout::SideBySide && self.document.can_split()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: impl Into<PathBuf>) -> CommittedFile {
        CommittedFile {
            path: path.into(),
            status: "M".into(),
            original: None,
        }
    }

    #[test]
    fn nested_tree_collapses_and_file_shortcuts_reveal_ancestors() {
        let files = [file("src/a.rs"), file("src/nested/b.rs"), file("root.txt")];
        let mut tree = FileTree::new(&files);
        tree.select_file(1);
        tree.collapse(); // parent nested/
        assert_eq!(tree.selected().unwrap().path, Path::new("src/nested"));
        tree.collapse(); // close nested/
        tree.collapse(); // move to src/
        tree.collapse(); // close src/
        assert_eq!(tree.rows.len(), 2);
        assert!(tree.collapsed.contains(Path::new("src")));
        assert_eq!(tree.step_file(1, false), Some(0));
        assert!(tree.collapsed.contains(Path::new("src/nested")));
        assert_eq!(tree.step_file(0, true), Some(1));
        assert!(!tree.collapsed.contains(Path::new("src/nested")));
        assert_eq!(tree.selected().unwrap().file, Some(1));
        assert_eq!(
            tree.rows
                .iter()
                .find(|row| row.path == Path::new("src") && row.file.is_none())
                .unwrap()
                .count,
            2
        );
    }

    #[test]
    fn same_name_file_and_directory_are_distinct_and_empty_trees_are_safe() {
        let mut tree = FileTree::new(&[file("foo"), file("foo/child")]);
        tree.cursor = 0;
        assert!(tree.toggle());
        assert_eq!(tree.rows.len(), 2);
        tree.select_file(0);
        assert_eq!(tree.selected().unwrap().file, Some(0));
        assert!(tree.collapsed.contains(Path::new("foo")));
        let mut empty = FileTree::new(&[]);
        assert!(!empty.toggle());
        assert_eq!(empty.navigate(1), None);
        assert_eq!(empty.step_file(0, false), None);
        empty.collapse();
        assert_eq!(empty.expand(), None);
    }

    #[cfg(unix)]
    #[test]
    fn tree_retains_non_utf8_path_identity() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"dir/invalid-\xff".to_vec()));
        let mut tree = FileTree::new(&[file(path.clone())]);
        tree.select_file(0);
        assert_eq!(tree.selected().unwrap().path, path);
    }

    #[test]
    fn split_alignment_pairs_replacements_with_real_old_and_new_line_numbers() {
        let document = Document::patch(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -10,4 +10,5 @@ function\n context\n-old1\n-old2\n+new1\n+new2\n+extra\n tail\n",
        );
        assert!(document.can_split());
        assert_eq!(document.rows.len(), 9);
        let paired = &document.rows[document.row_for(5)];
        assert_eq!(paired.left.as_ref().unwrap().text, "old1");
        assert_eq!(paired.right.as_ref().unwrap().text, "new1");
        assert_eq!(paired.left.as_ref().unwrap().number, Some(11));
        assert_eq!(document.row_for(5), document.row_for(7));
        let extra = &document.rows[document.row_for(9)];
        assert!(extra.left.is_none());
        assert_eq!(extra.right.as_ref().unwrap().number, Some(13));
        let tail = document.rows.last().unwrap();
        assert_eq!(tail.left.as_ref().unwrap().number, Some(13));
        assert_eq!(tail.right.as_ref().unwrap().number, Some(14));
        assert_eq!(document.step(7, 1, true), 6);
    }

    #[test]
    fn empty_sides_and_no_newline_annotations_are_not_fabricated() {
        let added = Document::patch("@@ -0,0 +1,2 @@\n+one\n+two\n");
        assert!(added.can_split());
        assert!(added.rows[1].left.is_none());
        assert_eq!(added.rows[2].right.as_ref().unwrap().number, Some(2));
        let removed = Document::patch("@@ -1 +0,0 @@\n-old\n");
        assert!(removed.can_split());
        assert!(removed.rows[1].right.is_none());
        let no_eol = Document::patch(
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        );
        assert!(no_eol.can_split());
        assert_eq!(no_eol.rows.len(), 2);
        assert_eq!(no_eol.row_for(2), no_eol.row_for(4));
        assert_eq!(
            no_eol.rows[1]
                .left
                .as_ref()
                .unwrap()
                .annotation
                .as_ref()
                .unwrap()
                .0,
            2
        );
        assert_eq!(
            no_eol.rows[1]
                .right
                .as_ref()
                .unwrap()
                .annotation
                .as_ref()
                .unwrap()
                .0,
            4
        );
    }

    #[test]
    fn malformed_truncated_and_metadata_only_patches_fall_back_without_losing_text() {
        for text in [
            "@@ -1,2 +1,2 @@\n incomplete\n",
            "@@ -99999999999999999999999999 +1 @@\n-a\n+b\n",
            "@@ -1 +1 @@\n-old\n+new\n[Preview truncated at 256 KiB]",
            "Binary files a/f and b/f differ\n",
            "old mode 100644\nnew mode 100755\n",
        ] {
            let document = Document::patch(text);
            assert!(!document.can_split(), "{text}");
            assert_eq!(
                document.lines,
                text.lines().map(str::to_owned).collect::<Vec<_>>()
            );
            assert!(document.split_note.is_some());
        }
    }

    #[test]
    fn search_is_literal_unicode_aware_wraps_and_can_be_cancelled_or_cleared() {
        let document = Document::full("a.b A.B\n雪 café\na.b\n");
        let mut search = Search {
            query: "a.b".into(),
            ..Search::default()
        };
        search.reindex(&document, 1);
        assert_eq!(search.matches.len(), 3);
        assert_eq!(search.current().unwrap().line, 2);
        assert_eq!(search.next(false).unwrap().line, 0);
        assert_eq!(search.next(true).unwrap().line, 2);
        search.begin();
        search.paste("new query");
        search.editing = None;
        assert_eq!(search.query, "a.b");
        search.query = "CAFÉ".into();
        search.reindex(&document, 0);
        let hit = search.current().unwrap();
        assert_eq!(&document.lines[hit.line][hit.bytes.clone()], "café");
        search.editing = Some(String::new());
        search.accept(&document, 0);
        assert!(search.matches.is_empty());
        assert!(search.next(false).is_none());
    }

    #[test]
    fn layout_falls_back_at_narrow_widths_without_changing_the_preference() {
        let mut history = History::new(&[]);
        history.document = Document::patch("@@ -1 +1 @@\n-old\n+new\n");
        history.layout = DiffLayout::SideBySide;
        assert!(history.split(120, false));
        assert!(!history.split(119, false));
        assert!(!history.split(140, true));
        assert_eq!(history.layout, DiffLayout::SideBySide);
    }
}
