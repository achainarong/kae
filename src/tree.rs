//! Working-tree presentation reuses history's byte-preserving tree identities.
use crate::{
    git::{CommittedFile, FileChange},
    history::FileTree,
};
use std::path::Path;

pub fn explorer(files: &[FileChange], root: &Path) -> FileTree {
    let files = files
        .iter()
        .map(|file| CommittedFile {
            path: file.path.clone(),
            status: file.status.clone(),
            original: file.original.clone(),
        })
        .collect::<Vec<_>>();
    FileTree::compact(&files).with_root(root.file_name().unwrap_or(root.as_os_str()).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn compact_chains_keep_parent_navigation_and_counts() {
        let mut tree = explorer(
            &["apps/portal/src/a", "apps/portal/src/b", "outside"].map(|path| FileChange {
                path: path.into(),
                status: " M".into(),
                original: None,
            }),
            Path::new("/work/project"),
        );
        assert_eq!(tree.rows[0].name, "project");
        assert_eq!(tree.rows[0].count, 3);
        assert_eq!(tree.rows[1].name, "apps/portal/src");
        assert_eq!(tree.rows[1].count, 2);
        tree.select_file(0);
        tree.collapse();
        assert_eq!(tree.selected().unwrap().path, Path::new("apps/portal/src"));
        assert!(tree.toggle());
        assert_eq!(tree.rows.len(), 3);
        tree.select_file(1);
        assert_eq!(tree.rows.len(), 5);
        assert_eq!(tree.selected().unwrap().file, Some(1));
    }

    #[test]
    fn repository_root_collapses_everything_and_file_selection_reveals_it() {
        let mut tree = explorer(
            &[FileChange {
                path: "README.md".into(),
                status: " M".into(),
                original: None,
            }],
            Path::new("/work/project"),
        );
        tree.collapse();
        assert!(tree.selected().unwrap().path.as_os_str().is_empty());
        assert!(tree.toggle());
        assert_eq!(tree.rows.len(), 1);
        tree.select_file(0);
        assert_eq!(tree.rows.len(), 2);
        assert!(!tree.collapsed.contains(Path::new("")));
        assert_eq!(tree.selected().unwrap().file, Some(0));
        let mut empty = explorer(&[], Path::new("/work/project"));
        assert_eq!(empty.rows.len(), 1);
        assert_eq!(empty.rows[0].name, "project");
        assert_eq!(empty.rows[0].count, 0);
        empty.collapse();
        empty.expand();
        assert_eq!(empty.rows.len(), 1);
    }
}
