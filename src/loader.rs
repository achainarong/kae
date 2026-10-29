//! One read worker with coalesced pending requests. UI generations decide which
//! results are still relevant; this worker never performs Git mutations.
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use anyhow::Result;

use crate::{
    conflict::Conflict,
    git::{CommitFiles, FileChange, Guard, Hunks, Repo, Snapshot, Worktree},
    rebase::Plan,
};

#[derive(Clone)]
pub enum Preview {
    File(FileChange),
    Revision { revision: String, heading: String },
    Remote { name: String, branch: String },
    Worktree(Worktree),
    Empty(String),
}

pub enum ReadKind {
    Snapshot,
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
    Snapshot(Box<(Repo, Snapshot)>),
    Preview(String),
    Conflict(Box<Conflict>),
    Hunks(Box<Hunks>),
    Rebase(Box<Plan>),
    Output { title: String, text: String },
    AmendMessage(String),
    Commit(CommitFiles),
    CommitPreview(String),
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
}

pub struct Loader {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    responses: Receiver<Response>,
}

impl Loader {
    pub fn new() -> Self {
        Self::spawn(execute)
    }

    #[cfg(test)]
    pub fn with_reader(
        reader: impl Fn(Repo, ReadKind) -> Result<ReadData> + Send + 'static,
    ) -> Self {
        Self::spawn(reader)
    }

    fn spawn(reader: impl Fn(Repo, ReadKind) -> Result<ReadData> + Send + 'static) -> Self {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let shared = Arc::clone(&pending);
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let request = {
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
                            break request;
                        }
                        queue = wake.wait(queue).expect("loader queue wait");
                    }
                };
                let result =
                    reader(request.repo, request.kind).map_err(|error| format!("{error:#}"));
                if sender
                    .send(Response {
                        id: request.id,
                        result,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self { pending, responses }
    }

    pub fn request(&self, request: Request) {
        let (mutex, wake) = &*self.pending;
        let mut pending = mutex.lock().expect("loader queue lock");
        match request.kind {
            ReadKind::Snapshot => pending.snapshot = Some(request),
            ReadKind::Preview(_) | ReadKind::CommitPreview { .. } => {
                pending.preview = Some(request)
            }
            _ => pending.detail = Some(request),
        }
        wake.notify_one();
    }

    pub fn clear(&self) {
        let mut pending = self.pending.0.lock().expect("loader queue lock");
        pending.snapshot = None;
        pending.preview = None;
        pending.detail = None;
    }

    pub fn try_recv(&self) -> std::result::Result<Response, TryRecvError> {
        self.responses.try_recv()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.pending.0.lock().expect("loader queue lock").stopped = true;
        self.pending.1.notify_one();
    }
}

fn execute(repo: Repo, kind: ReadKind) -> Result<ReadData> {
    Ok(match kind {
        ReadKind::Snapshot => {
            let repo = Repo::discover(&repo.root)?;
            let snapshot = repo.snapshot()?;
            ReadData::Snapshot(Box::new((repo, snapshot)))
        }
        ReadKind::Preview(preview) => ReadData::Preview(match preview {
            Preview::File(file) => repo.file_preview(&file)?,
            Preview::Revision { revision, heading } => {
                format!("{heading}\n\n{}", repo.show(&revision)?)
            }
            Preview::Remote { name, branch } => format!(
                "Remote: {name}\nCurrent branch: {branch}\n\n{}\n\nf fetch · p fast-forward pull · P push\n: for merge/rebase pull and upstream setup.\n\nAuthentication must already be configured.\nURLs are omitted to protect credentials.",
                repo.text(&["status", "--short", "--branch"])?
            ),
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
        } => ReadData::CommitPreview(repo.historical_preview(&commit, index, full)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
