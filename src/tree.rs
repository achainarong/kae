//! Working-tree presentation reuses history's byte-preserving tree identities.
use crate::{
    git::{CommittedFile, FileChange, Snapshot},
    history::FileTree,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub fn folder_status(
    files: &[FileChange],
    pending: &HashMap<PathBuf, bool>,
) -> HashMap<PathBuf, (usize, usize)> {
    let mut totals = HashMap::new();
    for file in files {
        let status = file.status.as_bytes();
        let (staged, working) = if let Some(staged) = pending.get(&file.path) {
            (usize::from(*staged), usize::from(!*staged))
        } else {
            (
                usize::from(!matches!(status[0], b' ' | b'?')),
                usize::from(status[1] != b' '),
            )
        };
        for path in file.path.ancestors().skip(1) {
            let total = totals.entry(path.to_path_buf()).or_insert((0, 0));
            total.0 += staged;
            total.1 += working;
        }
    }
    totals
}

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

pub fn remotes(snapshot: &Snapshot) -> FileTree {
    let files = snapshot
        .remote_branches
        .iter()
        .map(|branch| CommittedFile {
            path: format!("{}/{}", branch.remote, branch.branch).into(),
            status: String::new(),
            original: None,
        })
        .collect::<Vec<_>>();
    let mut tree = FileTree::new(&files)
        .with_directories(&snapshot.remotes.iter().map(Into::into).collect::<Vec<_>>());
    tree.cursor = 0;
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn folder_status_aggregates_once_and_tracks_optimistic_changes() {
        let files =
            [("src/a", "M "), ("src/nested/b", " M"), ("other/c", "??")].map(|(path, status)| {
                FileChange {
                    path: path.into(),
                    status: status.into(),
                    original: None,
                }
            });
        let totals = folder_status(&files, &HashMap::new());
        assert_eq!(totals[Path::new("")], (1, 2));
        assert_eq!(totals[Path::new("src")], (1, 1));
        let pending = HashMap::from([(PathBuf::from("src/nested/b"), true)]);
        let totals = folder_status(&files, &pending);
        assert_eq!(totals[Path::new("src")], (2, 0));
        assert_eq!(totals[Path::new("")], (2, 1));
    }
    #[test]
    fn remote_names_are_independent_roots_even_with_slashes_or_ref_collisions() {
        let mut snapshot = Snapshot::loading();
        snapshot.remotes = vec!["origin".into(), "origin/team".into(), "empty".into()];
        snapshot.remote_branches = [("origin", "z"), ("origin", "team"), ("origin/team", "main")]
            .map(|(remote, branch)| crate::git::RemoteBranch {
                remote: remote.into(),
                branch: branch.into(),
                oid: "abc".into(),
            })
            .into();
        let mut tree = remotes(&snapshot);
        let root = tree
            .rows
            .iter()
            .find(|row| row.path == Path::new("origin") && row.file.is_none())
            .unwrap();
        assert_eq!((root.depth, root.count), (0, 2));
        let nested = tree
            .rows
            .iter()
            .find(|row| row.path == Path::new("origin/team") && row.file.is_none())
            .unwrap();
        assert_eq!((nested.depth, nested.count), (0, 1));
        assert_eq!(nested.name, "origin/team");
        tree.set_filter("origin/team/main");
        assert_eq!(tree.rows.len(), 2);
        assert!(
            tree.rows
                .iter()
                .all(|row| row.path.starts_with("origin/team"))
        );
    }
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
