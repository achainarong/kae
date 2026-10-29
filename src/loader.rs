//! Independent interactive/background read lanes. New navigation cancels obsolete
//! read-only work; UI generations still reject late results. No Git mutations.
use std::{
    collections::VecDeque,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use anyhow::Result;

use crate::{
    conflict::Conflict,
    git::{CommitFiles, FileChange, Guard, Hunks, RemoteBranch, Repo, Snapshot, Worktree},
    history::HistoricalPreview,
    rebase::Plan,
};

#[derive(Clone, PartialEq, Eq)]
pub enum Preview {
    File(FileChange),
    Folder {
        heading: String,
        path: PathBuf,
        files: Vec<FileChange>,
    },
    Revision {
        revision: String,
        heading: String,
    },
    Remote {
        name: String,
        branch: String,
        folder: String,
        branches: Vec<String>,
        tracking: Vec<String>,
    },
    RemoteBranch {
        branch: RemoteBranch,
        head: Option<String>,
    },
    Worktree(Worktree),
    Empty(String),
}

pub enum ReadKind {
    Snapshot {
        all_history: bool,
    },
    Refresh(Box<Snapshot>),
    IndexStatus(Vec<PathBuf>),
    Preview(Preview),
    Conflict {
        path: PathBuf,
        guard: Guard,
        branch: String,
    },
    Hunks {
        snapshot: Box<Snapshot>,
        file: FileChange,
        cached: bool,
    },
    Rebase {
        snapshot: Box<Snapshot>,
        base: String,
    },
    Compare(String),
    Review(bool),
    AmendMessage,
    Commit {
        oid: String,
        parent: usize,
    },
    CommitPreview {
        commit: CommitFiles,
        index: usize,
        full: bool,
    },
}

pub enum ReadData {
    IndexStatus {
        scopes: Vec<PathBuf>,
        files: Vec<FileChange>,
    },
    Snapshot(Box<(Repo, Snapshot)>),
    Preview(String),
    Conflict(Box<Conflict>),
    Hunks(Box<Hunks>),
    Rebase(Box<Plan>),
    Output {
        title: String,
        text: String,
    },
    AmendMessage(String),
    Commit(CommitFiles),
    CommitPreview(Box<HistoricalPreview>),
}

pub struct Request {
    pub id: u64,
    pub repo: Repo,
    pub kind: ReadKind,
}
pub struct Response {
    pub id: u64,
    pub result: std::result::Result<ReadData, String>,
}

#[derive(Default)]
struct Pending {
    snapshot: Option<Request>,
    preview: Option<Request>,
    detail: Option<Request>,
    stopped: bool,
    active: Option<Arc<AtomicBool>>,
}

pub struct Loader {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    background: Arc<(Mutex<Pending>, Condvar)>,
    responses: Receiver<Response>,
}

impl Loader {
    pub fn new() -> Self {
        Self::spawn(execute)
    }

    #[cfg(test)]
    pub fn with_reader(
        reader: impl Fn(Repo, ReadKind) -> Result<ReadData> + Send + Sync + 'static,
    ) -> Self {
        Self::spawn(reader)
    }

    fn spawn(reader: impl Fn(Repo, ReadKind) -> Result<ReadData> + Send + Sync + 'static) -> Self {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let background = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (sender, responses) = mpsc::channel();
        let reader = Arc::new(reader);
        let ui = thread::current();
        for lane in [&pending, &background] {
            let shared = Arc::clone(lane);
            let sender = sender.clone();
            let reader = Arc::clone(&reader);
            let ui = ui.clone();
            thread::spawn(move || {
                let mut cache = PreviewCache::default();
                loop {
                    let (request, cancel) = {
                        let (mutex, wake) = &*shared;
                        let mut queue = mutex.lock().expect("loader queue lock");
                        loop {
                            if queue.stopped {
                                return;
                            }
                            if let Some(request) = queue
                                .detail
                                .take()
                                .or_else(|| queue.snapshot.take())
                                .or_else(|| queue.preview.take())
                            {
                                let cancel = Arc::new(AtomicBool::new(false));
                                queue.active = Some(Arc::clone(&cancel));
                                break (request, cancel);
                            }
                            queue = wake.wait(queue).expect("loader queue wait");
                        }
                    };
                    let key = cache_key(&request);
                    let result = crate::git::cancellable_read(Arc::clone(&cancel), || {
                        crate::git::check_read_cancelled()?;
                        if let Some(key) = &key
                            && let Some(data) = cache.get(key)
                        {
                            return Ok(data);
                        }
                        let data = reader(request.repo, request.kind)?;
                        crate::git::check_read_cancelled()?;
                        if let Some(key) = key {
                            cache.insert(key, &data);
                        }
                        Ok(data)
                    })
                    .map_err(|error: anyhow::Error| format!("{error:#}"));
                    shared.0.lock().expect("loader queue lock").active = None;
                    if sender
                        .send(Response {
                            id: request.id,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ui.unpark();
                }
            });
        }
        Self {
            pending,
            background,
            responses,
        }
    }

    pub fn request(&self, request: Request) {
        let background = matches!(
            request.kind,
            ReadKind::Snapshot { .. } | ReadKind::Refresh(_) | ReadKind::IndexStatus(_)
        );
        let (mutex, wake) = if background {
            &*self.background
        } else {
            &*self.pending
        };
        let mut pending = mutex.lock().expect("loader queue lock");
        if let Some(active) = &pending.active {
            active.store(true, Ordering::Relaxed);
        }
        if !background {
            pending.preview = None;
            pending.detail = None;
        }
        match request.kind {
            ReadKind::Snapshot { .. } | ReadKind::Refresh(_) | ReadKind::IndexStatus(_) => {
                pending.snapshot = Some(request)
            }
            ReadKind::Preview(_) | ReadKind::CommitPreview { .. } => {
                pending.preview = Some(request)
            }
            _ => pending.detail = Some(request),
        }
        wake.notify_one();
    }

    pub fn clear(&self) {
        for lane in [&self.pending, &self.background] {
            let mut pending = lane.0.lock().expect("loader queue lock");
            pending.snapshot = None;
            pending.preview = None;
            pending.detail = None;
            if let Some(active) = &pending.active {
                active.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn try_recv(&self) -> std::result::Result<Response, TryRecvError> {
        self.responses.try_recv()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.clear();
        for lane in [&self.pending, &self.background] {
            lane.0.lock().expect("loader queue lock").stopped = true;
            lane.1.notify_one();
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CacheKey(PathBuf, Vec<OsString>);

impl CacheKey {
    fn size(&self) -> usize {
        self.0.capacity()
            + self.1.capacity() * std::mem::size_of::<OsString>()
            + self.1.iter().map(|part| part.capacity()).sum::<usize>()
    }
}

fn object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn cache_key(request: &Request) -> Option<CacheKey> {
    let parts = match &request.kind {
        ReadKind::Preview(Preview::Revision { revision, heading }) if object_id(revision) => {
            vec!["revision".into(), revision.into(), heading.into()]
        }
        ReadKind::Commit { oid, parent } if object_id(oid) => {
            vec!["commit".into(), oid.into(), parent.to_string().into()]
        }
        ReadKind::CommitPreview {
            commit,
            index,
            full,
        } if object_id(&commit.oid) => {
            let file = commit.files.get(*index)?;
            vec![
                "file".into(),
                commit.oid.clone().into(),
                commit.parent.to_string().into(),
                file.path.as_os_str().into(),
                full.to_string().into(),
            ]
        }
        // Mutable worktree/index/ref previews are deliberately not cached.
        _ => return None,
    };
    Some(CacheKey(request.repo.root.clone(), parts))
}

enum Cached {
    Text(String),
    Historical(String, bool),
    Commit(CommitFiles),
}

impl Cached {
    fn size(&self) -> usize {
        match self {
            Self::Text(text) | Self::Historical(text, _) => text.capacity(),
            Self::Commit(commit) => {
                commit.oid.capacity()
                    + commit.subject.capacity()
                    + commit.parents.capacity() * std::mem::size_of::<String>()
                    + commit.parents.iter().map(String::capacity).sum::<usize>()
                    + commit.files.capacity() * std::mem::size_of::<crate::git::CommittedFile>()
                    + commit
                        .files
                        .iter()
                        .map(|file| {
                            file.path.capacity()
                                + file.original.as_ref().map(PathBuf::capacity).unwrap_or(0)
                                + file.status.capacity()
                        })
                        .sum::<usize>()
            }
        }
    }
    fn data(&self) -> ReadData {
        match self {
            Self::Text(text) => ReadData::Preview(text.clone()),
            Self::Historical(text, full) => {
                ReadData::CommitPreview(Box::new(HistoricalPreview::new(text.clone(), *full)))
            }
            Self::Commit(commit) => ReadData::Commit(commit.clone()),
        }
    }
}

#[derive(Default)]
struct PreviewCache {
    entries: VecDeque<(CacheKey, Cached)>,
    bytes: usize,
}

impl PreviewCache {
    fn get(&mut self, key: &CacheKey) -> Option<ReadData> {
        let index = self.entries.iter().position(|(entry, _)| entry == key)?;
        let entry = self.entries.remove(index)?;
        let data = entry.1.data();
        self.entries.push_front(entry);
        Some(data)
    }
    fn insert(&mut self, key: CacheKey, data: &ReadData) {
        const BUDGET: usize = 8 * 1024 * 1024;
        let value = match data {
            ReadData::Preview(text) => Cached::Text(text.clone()),
            ReadData::CommitPreview(preview) => Cached::Historical(
                preview.text.clone(),
                preview.document.split_note == Some("full file"),
            ),
            ReadData::Commit(commit) => Cached::Commit(commit.clone()),
            _ => return,
        };
        let size = key.size() + value.size();
        if size > BUDGET {
            return;
        }
        while self.entries.len() >= 32 || self.bytes + size > BUDGET {
            if let Some((removed_key, removed)) = self.entries.pop_back() {
                self.bytes -= removed_key.size() + removed.size();
            } else {
                break;
            }
        }
        self.bytes += size;
        self.entries.push_front((key, value));
    }
}

fn execute(repo: Repo, kind: ReadKind) -> Result<ReadData> {
    Ok(match kind {
        ReadKind::IndexStatus(scopes) => ReadData::IndexStatus {
            files: repo.index_status(&scopes)?,
            scopes,
        },
        ReadKind::Refresh(previous) => {
            ReadData::Snapshot(Box::new((repo.clone(), repo.refresh_index(&previous)?)))
        }
        ReadKind::Snapshot { all_history } => {
            let repo = Repo::discover(&repo.root)?;
            let snapshot = if all_history {
                repo.snapshot()?
            } else {
                repo.snapshot_scoped(false)?
            };
            ReadData::Snapshot(Box::new((repo, snapshot)))
        }
        ReadKind::Preview(preview) => ReadData::Preview(match preview {
            Preview::File(file) => repo.file_preview(&file)?,
            Preview::Folder {
                heading,
                path,
                files,
            } => repo.folder_preview(&heading, &path, &files)?,
            Preview::Revision { revision, heading } => {
                format!("{heading}\n\n{}", repo.show(&revision)?)
            }
            Preview::Remote {
                name,
                branch,
                folder,
                branches,
                tracking,
            } => format!(
                "Remote: {name}\nCurrent local branch: {branch}\nGroup: {folder}\n\n{} cached remote branches\n{}\n\nLocal tracking:\n{}\n\nf fetch (explicit confirmation) · select a branch to compare\nNetwork actions are never triggered by browsing.\nURLs are omitted to protect credentials.",
                branches.len(),
                branches.join("\n"),
                if tracking.is_empty() {
                    "No local branches tracking this remote.".into()
                } else {
                    tracking.join("\n")
                }
            ),
            Preview::RemoteBranch { branch, head } => {
                let log = repo.text(&["log", "-10", "--format=%h %s", &branch.oid, "--"])?;
                let comparison = if let Some(head) = head {
                    format!(
                        "HEAD-only / remote-only commits: {}\n\n{}",
                        repo.text(&[
                            "rev-list",
                            "--left-right",
                            "--count",
                            &format!("{head}...{}", branch.oid),
                            "--"
                        ])?,
                        repo.text(&[
                            "diff",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--stat",
                            &head,
                            &branch.oid,
                            "--"
                        ])?
                    )
                } else {
                    "Current branch has no commits to compare.".into()
                };
                format!(
                    "Remote: {}\nBranch: {}\nCommit: {}\n\nLatest commits\n{log}\n\nCompared with current HEAD\n{comparison}\n\nf fetch · p pull this branch · P review push destination\nBrowsing uses cached refs; fetch explicitly to update them.",
                    branch.remote, branch.branch, branch.oid
                )
            }
            Preview::Worktree(worktree) => format!(
                "{}\nBranch: {}\nLocked: {}\n\nEnter to open · : for worktree actions\n\n{}",
                worktree.path.display(),
                worktree.branch,
                worktree.locked,
                Repo {
                    root: worktree.path.clone()
                }
                .text(&["status", "--short", "--branch"])?
            ),
            Preview::Empty(text) => text,
        }),
        ReadKind::Conflict {
            path,
            guard,
            branch,
        } => ReadData::Conflict(Box::new(Conflict::load(&repo, path, guard, &branch)?)),
        ReadKind::Hunks {
            snapshot,
            file,
            cached,
        } => ReadData::Hunks(Box::new(Hunks::load(&repo, &snapshot, &file, cached)?)),
        ReadKind::Rebase { snapshot, base } => {
            ReadData::Rebase(Box::new(Plan::load(&repo, &snapshot, &base)?))
        }
        ReadKind::Compare(revision) => ReadData::Output {
            title: "Selected revision → HEAD".into(),
            text: repo.text(&[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                &revision,
                "HEAD",
                "--",
            ])?,
        },
        ReadKind::Review(head) => ReadData::Output {
            title: "Integration result · includes staged and unstaged changes".into(),
            text: repo.text(if head {
                &["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"]
            } else {
                &["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"]
            })?,
        },
        ReadKind::AmendMessage => {
            ReadData::AmendMessage(repo.text(&["log", "-1", "--format=%B"])?)
        }
        ReadKind::Commit { oid, parent } => ReadData::Commit(repo.commit_files(&oid, parent)?),
        ReadKind::CommitPreview {
            commit,
            index,
            full,
        } => ReadData::CommitPreview(Box::new(HistoricalPreview::new(
            repo.historical_preview(&commit, index, full)?,
            full,
        ))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_blocked_background_refresh_does_not_block_navigation() {
        let (started, waiting) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let loader = Loader::spawn(move |_, kind| {
            if matches!(kind, ReadKind::Snapshot { .. }) {
                started.send(()).unwrap();
                blocked.lock().unwrap().recv().unwrap();
            }
            Ok(ReadData::Preview("ready".into()))
        });
        loader.request(Request {
            id: 1,
            repo: Repo { root: ".".into() },
            kind: ReadKind::Snapshot { all_history: false },
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        loader.request(Request {
            id: 2,
            repo: Repo { root: ".".into() },
            kind: ReadKind::Preview(Preview::Empty("interactive".into())),
        });
        let response = loader
            .responses
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(response.id, 2);
        release.send(()).unwrap();
        assert_eq!(
            loader
                .responses
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .id,
            1
        );
    }

    #[test]
    fn new_navigation_interrupts_a_running_read_process() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let child_marker = marker.clone();
        let loader = Loader::spawn(move |repo, kind| {
            if matches!(&kind, ReadKind::Preview(Preview::Empty(text)) if text == "slow") {
                let mut command = std::process::Command::new("sh");
                command
                    .args([
                        "-c",
                        "sleep 10 & printf started > \"$1\"; wait",
                        "read-test",
                    ])
                    .arg(&child_marker);
                repo.run_command(&mut command, None)?;
            }
            Ok(ReadData::Preview("fast".into()))
        });
        let request = |id, text: &str| Request {
            id,
            repo: Repo { root: ".".into() },
            kind: ReadKind::Preview(Preview::Empty(text.into())),
        };
        loader.request(request(1, "slow"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(marker.exists(), "read process never started");
        loader.request(request(2, "fast"));
        let cancelled = loader
            .responses
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(cancelled.id, 1);
        assert!(cancelled.result.err().unwrap().contains("superseded"));
        let fast = loader
            .responses
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(fast.id, 2);
        assert!(fast.result.is_ok());
    }

    #[test]
    fn immutable_previews_are_cached_but_mutable_references_are_not() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let loader = Loader::spawn(move |_, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(ReadData::Preview("immutable content".into()))
        });
        let request = |id, revision: &str| Request {
            id,
            repo: Repo {
                root: "/repo".into(),
            },
            kind: ReadKind::Preview(Preview::Revision {
                revision: revision.into(),
                heading: "heading".into(),
            }),
        };
        let oid = "a".repeat(40);
        for id in 1..=2 {
            loader.request(request(id, &oid));
            assert!(
                loader
                    .responses
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .result
                    .is_ok()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        for id in 3..=4 {
            loader.request(request(id, "refs/heads/main"));
            assert!(
                loader
                    .responses
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .result
                    .is_ok()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        let mutable = Request {
            id: 5,
            repo: Repo {
                root: "/repo".into(),
            },
            kind: ReadKind::Preview(Preview::File(FileChange {
                path: "file".into(),
                status: " M".into(),
                original: None,
            })),
        };
        assert!(cache_key(&mutable).is_none());
    }

    #[test]
    fn preview_cache_is_bounded_and_keeps_historical_modes_separate() {
        let mut cache = PreviewCache::default();
        for index in 0..64 {
            cache.insert(
                CacheKey("/repo".into(), vec![index.to_string().into()]),
                &ReadData::Preview("x".repeat(300_000)),
            );
        }
        assert!(cache.entries.len() <= 32 && cache.bytes <= 8 * 1024 * 1024);
        let key = CacheKey("/repo".into(), vec!["full".into()]);
        cache.insert(
            key.clone(),
            &ReadData::CommitPreview(Box::new(HistoricalPreview::new(
                "literal +content\n".into(),
                true,
            ))),
        );
        assert!(
            matches!(cache.get(&key), Some(ReadData::CommitPreview(preview)) if preview.document.split_note == Some("full file"))
        );
        cache.insert(
            CacheKey("/repo".into(), vec!["too large".into()]),
            &ReadData::Preview("x".repeat(9 * 1024 * 1024)),
        );
        assert!(cache.bytes <= 8 * 1024 * 1024);
    }

    #[test]
    fn coalesces_navigation_while_a_read_is_running() {
        let (started, waiting) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let state = Mutex::new((true, blocked));
        let loader = Loader::spawn(move |_, kind| {
            let mut state = state.lock().unwrap();
            if state.0 {
                state.0 = false;
                started.send(()).unwrap();
                state.1.recv().unwrap();
            }
            let ReadKind::Preview(Preview::Empty(text)) = kind else {
                panic!("unexpected request")
            };
            Ok(ReadData::Preview(text))
        });
        let request = |id| Request {
            id,
            repo: Repo { root: ".".into() },
            kind: ReadKind::Preview(Preview::Empty(id.to_string())),
        };
        loader.request(request(1));
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        for id in 2..=50 {
            loader.request(request(id));
        }
        release.send(()).unwrap();
        assert_eq!(
            loader
                .responses
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            loader
                .responses
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .id,
            50
        );
        assert!(
            loader
                .responses
                .recv_timeout(Duration::from_millis(30))
                .is_err()
        );
    }
}
