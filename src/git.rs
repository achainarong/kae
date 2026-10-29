use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsString,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    thread,
};

use anyhow::{Context, Result, ensure};

use crate::graph::{self, Commit};

pub const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
pub const PREVIEW_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Repo {
    pub root: PathBuf,
}

#[derive(Debug)]
pub struct GitOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub stdout_fingerprint: u64,
}

impl GitOutput {
    pub fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.stdout).into_owned();
        if !self.stderr.is_empty() {
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&self.stderr));
        }
        if self.truncated {
            text.push_str("\n[Output truncated at 2 MiB per stream]");
        }
        redact_credentials(&text)
    }
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub status: String,
    pub path: PathBuf,
    pub original: Option<PathBuf>,
}

impl FileChange {
    pub fn is_conflict(&self) -> bool {
        matches!(
            self.status.as_str(),
            "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU"
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    Apply,
    CherryPick,
    Revert,
}

impl Operation {
    pub fn command(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::Apply => "am",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge in progress",
            Self::Rebase => "Rebase in progress",
            Self::Apply => "Apply in progress",
            Self::CherryPick => "Cherry-pick in progress",
            Self::Revert => "Revert in progress",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub oid: String,
    pub upstream: String,
    pub tracking: String,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub struct Stash {
    pub name: String,
    pub oid: String,
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub locked: bool,
}

/// Optimistic guard: reject an action if HEAD, index, operation or changed-file
/// contents changed while a preview / confirmation was open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    head: Option<String>,
    symbolic_head: Option<String>,
    operation: Option<Operation>,
    status: Vec<u8>,
    index: u64,
    refs: Vec<u8>,
    remote_config: u64,
    files: Vec<(PathBuf, u64)>,
}

#[derive(Clone, Debug)]
pub struct RemoteBranch {
    pub remote: String,
    pub branch: String,
    pub oid: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub all_history: bool,
    pub branch: String,
    pub head: Option<String>,
    pub operation: Option<Operation>,
    pub files: Vec<FileChange>,
    pub commits: Vec<Commit>,
    pub branches: Vec<Branch>,
    pub remotes: Vec<String>,
    pub remote_branches: Vec<RemoteBranch>,
    pub stashes: Vec<Stash>,
    pub tags: Vec<String>,
    pub worktrees: Vec<Worktree>,
    pub guard: Guard,
}

impl Snapshot {
    pub fn loading() -> Self {
        Self {
            all_history: false,
            branch: "Loading…".into(),
            head: None,
            operation: None,
            files: Vec::new(),
            commits: Vec::new(),
            branches: Vec::new(),
            remotes: Vec::new(),
            remote_branches: Vec::new(),
            stashes: Vec::new(),
            tags: Vec::new(),
            worktrees: Vec::new(),
            guard: Guard::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommittedFile {
    pub status: String,
    pub path: PathBuf,
    pub original: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct CommitFiles {
    pub oid: String,
    pub subject: String,
    pub parents: Vec<String>,
    pub parent: usize,
    pub files: Vec<CommittedFile>,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub label: String,
    pub args: Vec<OsString>,
    pub input: Option<Vec<u8>>,
    pub clean: bool,
    pub idle: bool,
    pub guard: Guard,
}

impl Action {
    pub fn new(snapshot: &Snapshot, label: impl Into<String>, args: &[&str]) -> Self {
        Self {
            label: label.into(),
            args: args.iter().map(OsString::from).collect(),
            input: None,
            clean: false,
            idle: false,
            guard: snapshot.guard.clone(),
        }
    }

    pub fn path(mut self, path: &Path) -> Self {
        self.args.push(OsString::from("--"));
        self.args.push(path.as_os_str().to_owned());
        self
    }

    pub fn clean(mut self) -> Self {
        self.clean = true;
        self.idle = true;
        self
    }

    pub fn idle(mut self) -> Self {
        self.idle = true;
        self
    }

    pub fn updates_only_index(&self) -> bool {
        self.args.first().is_some_and(|command| {
            command == "add"
                || (command == "restore"
                    && self.args.iter().any(|arg| arg == "--staged")
                    && !self.args.iter().any(|arg| arg == "--worktree"))
                || (command == "rm" && self.args.iter().any(|arg| arg == "--cached"))
        })
    }
}

impl Repo {
    pub fn discover(path: &Path) -> Result<Self> {
        let probe = Self {
            root: path.to_owned(),
        };
        let root = probe
            .bytes(&["rev-parse", "--show-toplevel"])
            .context("Not a working Git repository. Run kae -C /path/to/repository.")?;
        Ok(Self {
            root: PathBuf::from(os_bytes(trim_lf(&root))?),
        })
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .args([
                "--no-pager",
                "-c",
                "color.ui=false",
                "-c",
                "rebase.updateRefs=false",
                "-C",
            ])
            .arg(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .stdin(Stdio::null());
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_LITERAL_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ] {
            command.env_remove(variable);
        }
        if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
            command.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
        }
        command
    }

    pub fn run(&self, args: &[OsString], input: Option<&[u8]>) -> Result<GitOutput> {
        self.run_command(&mut self.command_for(args), input)
    }

    fn command_for(&self, args: &[OsString]) -> Command {
        let mut command = self.command();
        // Do not propagate literal-pathspec mode into workflow commands such
        // as stash: Git's own child commands use explicit pathspec magic.
        // In particular, a global literal flag makes stash -u leave files behind.
        if args.first().is_some_and(|arg| {
            [
                "add",
                "diff",
                "diff-tree",
                "show",
                "restore",
                "checkout",
                "rm",
                "clean",
                "ls-files",
            ]
            .iter()
            .any(|name| arg == name)
        }) {
            command.arg("--literal-pathspecs");
        }
        command.args(args);
        command
    }

    pub fn run_command(&self, command: &mut Command, input: Option<&[u8]>) -> Result<GitOutput> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .context("Could not start Git. Is git available on PATH?")?;
        let stdout = child.stdout.take().context("Missing Git stdout")?;
        let stderr = child.stderr.take().context("Missing Git stderr")?;
        // Drain both pipes concurrently, even once the display limit is reached.
        let out = thread::spawn(move || bounded_read(stdout));
        let err = thread::spawn(move || bounded_read(stderr));
        let write_result = if let Some(input) = input {
            child
                .stdin
                .take()
                .context("Missing Git stdin")?
                .write_all(input)
        } else {
            Ok(())
        };
        let status = child.wait()?;
        let (stdout, out_cut, stdout_fingerprint) = out
            .join()
            .map_err(|_| anyhow::anyhow!("Git stdout reader failed"))??;
        let (stderr, err_cut, _) = err
            .join()
            .map_err(|_| anyhow::anyhow!("Git stderr reader failed"))??;
        if status.success() {
            write_result.context("Could not send input to Git")?;
        }
        Ok(GitOutput {
            success: status.success(),
            stdout,
            stderr,
            truncated: out_cut || err_cut,
            stdout_fingerprint,
        })
    }

    pub fn bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.checked(&args.iter().map(OsString::from).collect::<Vec<_>>(), None)
    }

    pub fn checked(&self, args: &[OsString], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let output = self.run(args, input)?;
        ensure!(output.success, "{}", output.text().trim());
        ensure!(
            !output.truncated,
            "Git output exceeded the safety limit. Narrow the selection or use Git directly."
        );
        Ok(output.stdout)
    }

    pub fn text(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes(args)?)
            .trim_end()
            .to_owned())
    }

    pub fn git_path(&self, name: &str) -> Result<PathBuf> {
        let path = self.bytes(&["rev-parse", "--git-path", name])?;
        Ok(self.root.join(os_bytes(trim_lf(&path))?))
    }

    pub fn operation(&self) -> Result<Option<Operation>> {
        // All operation markers are worktree-local. Resolve that directory once,
        // including for linked worktrees, rather than spawning Git per marker.
        let git_dir = self.git_path("")?;
        for (marker, operation) in [
            ("rebase-merge", Operation::Rebase),
            ("rebase-apply/rebasing", Operation::Rebase),
            ("rebase-apply", Operation::Apply),
            ("MERGE_HEAD", Operation::Merge),
            ("CHERRY_PICK_HEAD", Operation::CherryPick),
            ("REVERT_HEAD", Operation::Revert),
        ] {
            if git_dir.join(marker).exists() {
                return Ok(Some(operation));
            }
        }
        if let Ok(todo) = fs::read_to_string(git_dir.join("sequencer/todo")) {
            if todo.lines().any(|line| line.starts_with("revert ")) {
                return Ok(Some(Operation::Revert));
            }
            if todo.lines().any(|line| line.starts_with("pick ")) {
                return Ok(Some(Operation::CherryPick));
            }
        }
        Ok(None)
    }

    pub fn head(&self) -> Result<Option<String>> {
        let output = self.run(&strings(&["rev-parse", "--verify", "HEAD"]), None)?;
        if output.success {
            Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()))
        } else {
            // Only a genuinely unborn branch is an acceptable missing HEAD.
            self.bytes(&["symbolic-ref", "--quiet", "HEAD"])?;
            Ok(None)
        }
    }

    pub fn guard(&self) -> Result<Guard> {
        let status = self.bytes(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        let files = parse_status(&status)?
            .iter()
            .map(|file| {
                Ok((
                    file.path.clone(),
                    file_fingerprint(&self.root.join(&file.path))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let symbolic = self.run(&strings(&["symbolic-ref", "--quiet", "HEAD"]), None)?;
        let config = self.run(
            &strings(&["config", "--null", "--get-regexp", "^(remote\\.|branch\\.)"]),
            None,
        )?;
        ensure!(
            !config.truncated
                && (config.success || (config.stdout.is_empty() && config.stderr.is_empty())),
            "Could not read remote / upstream configuration safely."
        );
        let mut remote_config = DefaultHasher::new();
        config.stdout.hash(&mut remote_config);
        let index = self.run(&strings(&["ls-files", "--stage", "-z"]), None)?;
        ensure!(index.success, "Could not inspect index: {}", index.text());
        Ok(Guard {
            head: self.head()?,
            symbolic_head: symbolic.success.then(|| {
                String::from_utf8_lossy(&symbolic.stdout)
                    .trim_end()
                    .to_owned()
            }),
            operation: self.operation()?,
            status,
            index: index.stdout_fingerprint,
            refs: self.bytes(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
                "refs/stash",
            ])?,
            remote_config: remote_config.finish(),
            files,
        })
    }

    pub fn verify(&self, guard: &Guard) -> Result<()> {
        ensure!(
            &self.guard()? == guard,
            "Repository changed since this view was loaded. Refresh and review the action again."
        );
        Ok(())
    }

    pub fn execute(&self, action: &Action) -> Result<GitOutput> {
        self.verify(&action.guard)?;
        if action.clean {
            ensure!(
                action.guard.status.is_empty(),
                "This action needs a clean working tree and index. Commit or stash your changes first."
            );
        }
        if action.idle {
            ensure!(
                action.guard.operation.is_none(),
                "Finish or abort the current operation first."
            );
        }
        if action.args.first().is_some_and(|arg| arg == "add") {
            let files = parse_status(&action.guard.status)?;
            let paths = action
                .args
                .iter()
                .skip_while(|arg| *arg != "--")
                .skip(1)
                .collect::<Vec<_>>();
            for file in files.iter().filter(|file| file.is_conflict()) {
                if paths.is_empty()
                    || paths
                        .iter()
                        .any(|path| path.as_os_str() == file.path.as_os_str())
                {
                    let path = self.root.join(&file.path);
                    if path.is_file() && !path.is_symlink() {
                        ensure!(
                            !crate::conflict::contains_markers(&fs::read(path)?),
                            "Conflict markers remain in {}. Resolve and save before staging.",
                            file.path.display()
                        );
                    }
                }
            }
        }
        if action.args.iter().any(|arg| arg == "--continue") {
            self.ensure_resolved()?;
        }
        if action.args.first().is_some_and(|arg| arg == "worktree")
            && action.args.get(1).is_some_and(|arg| arg == "remove")
        {
            let path = action.args.last().context("Missing worktree path")?;
            let target = Repo::discover(&self.root.join(path))?;
            ensure!(
                target.operation()?.is_none(),
                "The target worktree has an operation in progress. Finish or abort it before removing the worktree."
            );
        }
        let mut command = self.command_for(&action.args);
        crate::editor::configure_git_editor(self, &mut command)?;
        let mut output = self.run_command(&mut command, action.input.as_deref())?;
        if action.guard.operation == Some(Operation::Rebase)
            && self.operation()? != Some(Operation::Rebase)
            && let Err(error) = crate::editor::cleanup_plan(self)
        {
            output.stderr.extend_from_slice(
                format!("\nCould not remove completed Kae rebase plan: {error}\n").as_bytes(),
            );
        }
        Ok(output)
    }

    pub fn merge_preview(&self, action: &Action) -> Result<GitOutput> {
        self.verify(&action.guard)?;
        let source = action
            .args
            .last()
            .context("Missing merge source")?
            .to_str()
            .context("Invalid merge source")?;
        let mut output = self.run(&action.args, None)?;
        let tree = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        if tree.len() >= 40 && tree.chars().all(|character| character.is_ascii_hexdigit()) {
            let commits = self.text(&[
                "log",
                "--oneline",
                "--max-count=100",
                &format!("HEAD..{source}"),
                "--",
            ])?;
            let diff = self.run(
                &strings(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--stat",
                    "--patch",
                    "HEAD",
                    &tree,
                    "--",
                ]),
                None,
            )?;
            let mut preview = format!("Source: {source}\nDestination: HEAD\n\nINCOMING COMMITS (up to 100)\n{commits}\n\nMERGE ANALYSIS\n").into_bytes();
            preview.extend_from_slice(&output.stdout);
            preview.extend_from_slice(b"\nPROPOSED RESULT AGAINST HEAD\n");
            preview.extend_from_slice(&diff.stdout);
            output.stdout = preview;
            output.truncated |= diff.truncated;
        }
        Ok(output)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_scoped(true)
    }

    pub fn snapshot_scoped(&self, all_history: bool) -> Result<Snapshot> {
        let guard = self.guard()?;
        self.snapshot_with_guard(guard, all_history)
    }

    /// Index updates cannot change history. Reuse it only after a fresh, full
    /// guard confirms that refs, HEAD, operation, and upstream config still match.
    /// External changes fall back to the normal complete snapshot.
    pub fn refresh_index(&self, previous: &Snapshot) -> Result<Snapshot> {
        let guard = self.guard()?;
        let old = &previous.guard;
        if guard.head != old.head
            || guard.symbolic_head != old.symbolic_head
            || guard.operation != old.operation
            || guard.refs != old.refs
            || guard.remote_config != old.remote_config
        {
            return self.snapshot_with_guard(guard, previous.all_history);
        }
        let mut snapshot = previous.clone();
        snapshot.files = parse_status(&guard.status)?;
        snapshot.guard = guard;
        Ok(snapshot)
    }

    fn snapshot_with_guard(&self, guard: Guard, all_history: bool) -> Result<Snapshot> {
        let files = parse_status(&guard.status)?;
        let branch = self
            .text(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .unwrap_or_else(|_| {
                format!(
                    "detached @ {}",
                    guard
                        .head
                        .as_deref()
                        .unwrap_or("unknown")
                        .chars()
                        .take(10)
                        .collect::<String>()
                )
            });
        let mut branches = Vec::new();
        for record in self.text(&["for-each-ref", "--sort=refname", "--format=%(refname:short)%00%(objectname)%00%(upstream:short)%00%(upstream:track)%00%(HEAD)", "refs/heads"])?.lines() {
            let fields: Vec<_> = record.split('\0').collect();
            ensure!(fields.len() == 5, "Malformed branch record");
            branches.push(Branch { name: fields[0].into(), oid: fields[1].into(), upstream: fields[2].into(), tracking: fields[3].into(), current: fields[4] == "*" });
        }
        let mut log_args = vec!["log", "--graph", "--date-order", "--max-count=300"];
        if all_history {
            log_args.extend(["--branches", "--remotes", "--tags"]);
        }
        if guard.head.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--format=%x1e%H%x00%h%x00%d%x00%s%x00%an%x00%ar", "--"]);
        let commits = if !all_history && guard.head.is_none() {
            Vec::new()
        } else {
            graph::parse(&self.bytes(&log_args)?)?
        };
        let stashes = self
            .text(&["stash", "list", "--format=%gd%x00%H%x00%gs"])?
            .lines()
            .map(|line| {
                let fields: Vec<_> = line.splitn(3, '\0').collect();
                ensure!(fields.len() == 3, "Malformed stash record");
                Ok(Stash {
                    name: fields[0].into(),
                    oid: fields[1].into(),
                    subject: fields[2].into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let remotes = self
            .text(&["remote"])?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut remote_branches = Vec::new();
        for record in self
            .text(&[
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname)%00%(objectname)%00%(symref)",
                "refs/remotes",
            ])?
            .lines()
        {
            let fields = record.split('\0').collect::<Vec<_>>();
            ensure!(fields.len() == 3, "Malformed remote branch record");
            if !fields[2].is_empty() {
                continue;
            }
            let Some(name) = fields[0].strip_prefix("refs/remotes/") else {
                continue;
            };
            if let Some(remote) = remotes
                .iter()
                .filter(|remote| name.starts_with(&format!("{remote}/")))
                .max_by_key(|remote| remote.len())
            {
                remote_branches.push(RemoteBranch {
                    remote: remote.clone(),
                    branch: name[remote.len() + 1..].into(),
                    oid: fields[1].into(),
                });
            }
        }
        Ok(Snapshot {
            all_history,
            branch,
            head: guard.head.clone(),
            operation: guard.operation,
            files,
            commits,
            branches,
            stashes,
            remotes,
            remote_branches,
            tags: self
                .text(&["tag", "--list", "--sort=-creatordate"])?
                .lines()
                .map(str::to_owned)
                .collect(),
            worktrees: parse_worktrees(&self.bytes(&["worktree", "list", "--porcelain", "-z"])?)?,
            guard,
        })
    }

    pub fn file_preview(&self, file: &FileChange) -> Result<String> {
        if file.status == "??" {
            let path = self.root.join(&file.path);
            let metadata = path.symlink_metadata()?;
            if metadata.file_type().is_symlink() {
                return Ok(format!(
                    "Untracked symbolic link → {}",
                    path.read_link()?.display()
                ));
            }
            ensure!(metadata.is_file(), "No preview for this file type");
            let mut bytes = Vec::new();
            File::open(&path)?
                .take(PREVIEW_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)?;
            return Ok(format!(
                "Untracked file — {}\n\n{}",
                file.path.display(),
                preview_text(&bytes)
            ));
        }
        let mut sections = Vec::new();
        if file.is_conflict() {
            sections.push("UNRESOLVED CONFLICT — Enter to open the resolution workspace".into());
        }
        for (cached, title) in [(true, "STAGED"), (false, "WORKING TREE")] {
            let output = self.diff(&file.path, cached)?;
            if !output.is_empty() {
                sections.push(format!("{title}\n\n{}", preview_text(&output)));
            }
        }
        if sections.is_empty() {
            sections.push("No text diff (file mode, binary or submodule change).".into());
        }
        Ok(sections.join("\n\n"))
    }

    pub fn diff(&self, path: &Path, cached: bool) -> Result<Vec<u8>> {
        let mut args = strings(&["diff", "--no-ext-diff", "--no-textconv", "--no-color"]);
        if cached {
            args.push("--cached".into());
        }
        args.extend([OsString::from("--"), path.as_os_str().to_owned()]);
        self.checked(&args, None)
    }

    pub fn show(&self, revision: &str) -> Result<String> {
        self.text(&[
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            "--patch",
            "--format=fuller",
            revision,
            "--",
        ])
    }

    pub fn commit_files(&self, oid: &str, parent: usize) -> Result<CommitFiles> {
        let oid = self.text(&["rev-parse", "--verify", &format!("{oid}^{{commit}}")])?;
        let parents = self
            .text(&["show", "-s", "--format=%P", &oid, "--"])?
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let parent = parent.min(parents.len().saturating_sub(1));
        let mut args = strings(&[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            "-M",
        ]);
        if let Some(base) = parents.get(parent) {
            args.push(base.into());
        } else {
            args.push("--root".into());
        }
        args.extend([oid.clone().into(), "--".into()]);
        let output = self.checked(&args, None)?;
        let mut fields = output
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty());
        let mut files = Vec::new();
        while let Some(status) = fields.next() {
            let status = std::str::from_utf8(status)?.to_owned();
            let first = PathBuf::from(os_bytes(fields.next().context("Missing historical path")?)?);
            let (path, original) = if status.starts_with(['R', 'C']) {
                (
                    PathBuf::from(os_bytes(
                        fields.next().context("Missing renamed destination")?,
                    )?),
                    Some(first),
                )
            } else {
                (first, None)
            };
            files.push(CommittedFile {
                status,
                path,
                original,
            });
        }
        Ok(CommitFiles {
            subject: self.text(&["show", "-s", "--format=%s", &oid, "--"])?,
            oid,
            parents,
            parent,
            files,
        })
    }

    pub fn historical_preview(
        &self,
        commit: &CommitFiles,
        index: usize,
        full: bool,
    ) -> Result<String> {
        let Some(file) = commit.files.get(index) else {
            return Ok("No files changed against this parent.".into());
        };
        if full {
            let revision = if file.status == "D" {
                commit
                    .parents
                    .get(commit.parent)
                    .context("Deleted file has no parent")?
            } else {
                &commit.oid
            };
            let mut object = OsString::from(format!("{revision}:"));
            object.push(&file.path);
            let bytes = self.checked(
                &[
                    "show".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    object,
                ],
                None,
            )?;
            return Ok(preview_text(&bytes));
        }
        let mut args = if let Some(base) = commit.parents.get(commit.parent) {
            strings(&[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "-M",
                base,
                &commit.oid,
            ])
        } else {
            strings(&[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--no-ext-diff",
                "--no-textconv",
                "-p",
                "-r",
                &commit.oid,
            ])
        };
        args.push("--".into());
        args.push(exact_file_pathspec(&file.path)?);
        if let Some(original) = &file.original {
            args.push(exact_file_pathspec(original)?);
        }
        // Explicit, escaped pathspecs must not inherit literal-pathspec mode.
        // An ordinary literal path also selects descendants when a file was
        // replaced by a directory, leaking unrelated patches into this view.
        let output = self.run_command(self.command().args(&args), None)?;
        ensure!(output.success, "{}", output.text().trim());
        ensure!(
            !output.truncated,
            "Git diff exceeded the safety limit; use Git directly for this file."
        );
        let output = output.stdout;
        Ok(if output.is_empty() {
            "No textual changes for this file.".into()
        } else {
            preview_text(&output)
        })
    }

    pub fn ensure_resolved(&self) -> Result<()> {
        ensure!(
            !parse_status(&self.guard()?.status)?
                .iter()
                .any(FileChange::is_conflict),
            "Resolve and stage every conflicted file before continuing."
        );
        Ok(())
    }

    /// Reject symlinked parent directories before opening a user-editable path.
    pub fn safe_file(&self, relative: &Path) -> Result<PathBuf> {
        ensure!(
            !relative.is_absolute(),
            "Expected a repository-relative path"
        );
        let mut path = self.root.clone();
        for component in relative.components() {
            ensure!(
                matches!(component, Component::Normal(_)),
                "Unsafe relative path"
            );
            path.push(component);
            if let Ok(metadata) = path.symlink_metadata() {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Use Git's whole-file side choice for symbolic links; Kae will not follow symlinks while editing."
                );
            }
        }
        Ok(path)
    }
}

fn exact_file_pathspec(path: &Path) -> Result<OsString> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let (first, rest) = bytes.split_first().context("Empty historical file path")?;
    // A quoted one-byte character class forces Git's whole-path glob matcher;
    // without a wildcard Git treats even a :(literal) path as a directory prefix.
    let mut pattern = b":(top,glob)[\\".to_vec();
    pattern.push(*first);
    pattern.push(b']');
    for byte in rest {
        if matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\') {
            pattern.push(b'\\');
        }
        pattern.push(*byte);
    }
    os_bytes(&pattern)
}

pub fn strings(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn redact_credentials(text: &str) -> String {
    static USERINFO: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)([a-z][a-z0-9+.-]*://)[^\s/@'\"<>]+@"#)
            .expect("valid URL credential pattern")
    });
    static QUERY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)([?&](?:access_token|token|password|passwd|auth|api_key|key|secret|private_token)=)[^&\s'\"<>]+"#).expect("valid query credential pattern")
    });
    QUERY
        .replace_all(
            &USERINFO.replace_all(text, "${1}[redacted]@"),
            "${1}[redacted]",
        )
        .into_owned()
}

fn bounded_read(mut reader: impl Read) -> std::io::Result<(Vec<u8>, bool, u64)> {
    let mut stored = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    let mut fingerprint = DefaultHasher::new();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(stored.len()));
        fingerprint.write(&buffer[..count]);
        stored.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok((stored, truncated, fingerprint.finish()))
}

pub fn file_fingerprint(path: &Path) -> Result<u64> {
    let mut hash = DefaultHasher::new();
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    metadata.permissions().readonly().hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode().hash(&mut hash);
    }
    if metadata.file_type().is_symlink() {
        path.read_link()?.hash(&mut hash);
    } else if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut buffer = [0; 8192];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.write(&buffer[..count]);
        }
    } else {
        metadata.len().hash(&mut hash);
    }
    Ok(hash.finish())
}

pub fn parse_status(bytes: &[u8]) -> Result<Vec<FileChange>> {
    let mut records = bytes.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        ensure!(
            record.len() >= 4 && record[2] == b' ',
            "Malformed Git status record"
        );
        let status = std::str::from_utf8(&record[..2])?.to_owned();
        let path = PathBuf::from(os_bytes(&record[3..])?);
        let original = if record[..2].iter().any(|byte| matches!(byte, b'R' | b'C')) {
            let original = records
                .next()
                .filter(|record| !record.is_empty())
                .context("Incomplete Git rename record")?;
            Some(PathBuf::from(os_bytes(original)?))
        } else {
            None
        };
        files.push(FileChange {
            status,
            path,
            original,
        });
    }
    Ok(files)
}

fn parse_worktrees(bytes: &[u8]) -> Result<Vec<Worktree>> {
    let mut worktrees: Vec<Worktree> = Vec::new();
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            worktrees.push(Worktree {
                path: PathBuf::from(os_bytes(path)?),
                branch: "detached / unborn".into(),
                locked: false,
            });
        } else if let Some(branch) = field.strip_prefix(b"branch ") {
            if let Some(worktree) = worktrees.last_mut() {
                worktree.branch = String::from_utf8_lossy(branch)
                    .trim_start_matches("refs/heads/")
                    .into();
            }
        } else if field.starts_with(b"locked")
            && let Some(worktree) = worktrees.last_mut()
        {
            worktree.locked = true;
        }
    }
    Ok(worktrees)
}

#[cfg(unix)]
pub fn os_bytes(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
pub fn os_bytes(bytes: &[u8]) -> Result<OsString> {
    Ok(OsString::from(
        std::str::from_utf8(bytes).context("Non-UTF-8 Git path")?,
    ))
}

fn trim_lf(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

pub fn preview_text(bytes: &[u8]) -> String {
    if bytes.iter().take(PREVIEW_LIMIT).any(|byte| *byte == 0) {
        return "Binary content — no text preview.".into();
    }
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(PREVIEW_LIMIT)]).into_owned();
    if bytes.len() > PREVIEW_LIMIT {
        text.push_str("\n[Preview truncated at 256 KiB]");
    }
    text
}

#[derive(Clone, Debug)]
pub struct Hunks {
    pub path: PathBuf,
    pub cached: bool,
    pub header: Vec<u8>,
    pub bodies: Vec<Vec<u8>>,
    pub selected: usize,
    pub scroll: u16,
    pub horizontal: u16,
    pub guard: Guard,
}

impl Hunks {
    pub fn load(repo: &Repo, snapshot: &Snapshot, file: &FileChange, cached: bool) -> Result<Self> {
        ensure!(
            !file.is_conflict() && file.original.is_none() && file.status != "??",
            "Use whole-file staging for conflicts, renames and untracked files."
        );
        repo.verify(&snapshot.guard)?;
        Self::parse(
            file.path.clone(),
            cached,
            &repo.diff(&file.path, cached)?,
            snapshot.guard.clone(),
        )
    }

    pub fn parse(path: PathBuf, cached: bool, diff: &[u8], guard: Guard) -> Result<Self> {
        let mut header = Vec::new();
        let mut bodies: Vec<Vec<u8>> = Vec::new();
        for line in diff.split_inclusive(|byte| *byte == b'\n') {
            if line.starts_with(b"@@ ") {
                bodies.push(Vec::new());
            }
            if let Some(body) = bodies.last_mut() {
                body.extend_from_slice(line);
            } else {
                header.extend_from_slice(line);
            }
        }
        ensure!(
            !bodies.is_empty(),
            "No text hunks on this side. Toggle staged / unstaged or stage the whole file."
        );
        ensure!(
            !header
                .windows(b"new file mode".len())
                .any(|part| part == b"new file mode")
                && !header
                    .windows(b"deleted file mode".len())
                    .any(|part| part == b"deleted file mode"),
            "Use whole-file staging for additions and deletions."
        );
        Ok(Self {
            path,
            cached,
            header,
            bodies,
            selected: 0,
            scroll: 0,
            horizontal: 0,
            guard,
        })
    }

    pub fn apply(&self, repo: &Repo) -> Result<GitOutput> {
        repo.verify(&self.guard)?;
        let mut patch = self.header.clone();
        patch.extend_from_slice(&self.bodies[self.selected]);
        let mut args = strings(&["apply", "--cached", "--recount", "--whitespace=nowarn"]);
        if self.cached {
            args.push("--reverse".into());
        }
        let mut check = args.clone();
        check.push("--check".into());
        repo.checked(&check, Some(&patch))?;
        repo.run(&args, Some(&patch))
    }
}
