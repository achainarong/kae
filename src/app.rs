use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::{
    config::{self, Config},
    conflict::{Choice, Conflict},
    editor,
    git::{Action, CommitFiles, GitOutput, Hunks, Repo, Snapshot},
    history::{DiffLayout, HistoricalPreview, History, Position, Preferences},
    loader::{Loader, Preview, ReadData, ReadKind, Request},
    rebase::{Plan, RebaseAction},
    theme::Theme,
    tree, ui,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workspace {
    Files,
    History,
    Branches,
    Remotes,
    Stashes,
    Tags,
    Worktrees,
}

impl Workspace {
    pub const ALL: [Self; 7] = [
        Self::Files,
        Self::History,
        Self::Branches,
        Self::Remotes,
        Self::Stashes,
        Self::Tags,
        Self::Worktrees,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::History => "History",
            Self::Branches => "Branches",
            Self::Remotes => "Remotes",
            Self::Stashes => "Stashes",
            Self::Tags => "Tags",
            Self::Worktrees => "Worktrees",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|item| *item == self).unwrap_or(0)
    }
    fn next(self, backwards: bool) -> Self {
        Self::ALL[(self.index() + if backwards { 6 } else { 1 }) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    List,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    Stage,
    Unstage,
    StageAll,
    UnstageAll,
    Hunks,
    Commit,
    Amend,
    Discard,
    Edit,
    Resolve,
    TakeCurrent,
    TakeIncoming,
    DeleteConflict,
    BranchCreate,
    BranchSwitch,
    BranchRename,
    BranchDelete,
    Compare,
    MergePreview,
    Merge,
    CherryPick,
    Revert,
    Rebase,
    Fetch,
    PullFF,
    PullMerge,
    PullRebase,
    Push,
    PushUpstream,
    StashSave,
    StashApply,
    StashPop,
    StashDrop,
    TagCreate,
    TagDelete,
    WorktreeAdd,
    WorktreeOpen,
    WorktreeRemove,
    Continue,
    Skip,
    Abort,
    Review,
    Log,
    Theme,
}

impl CommandId {
    fn needs_confirmation(self) -> bool {
        use CommandId::*;
        // Exhaustive on purpose: a new command must make an explicit safety decision.
        match self {
            Unstage | UnstageAll | Amend | Discard | TakeCurrent | TakeIncoming
            | DeleteConflict | BranchRename | BranchDelete | Merge | CherryPick | Revert
            | Fetch | PullFF | PullMerge | PullRebase | Push | PushUpstream | StashSave
            | StashApply | StashPop | StashDrop | TagDelete | WorktreeRemove | Continue | Skip
            | Abort => true,
            // Hunk application, rebase execution, saves, and staged-version
            // replacement have their own contextual confirmation steps.
            Stage | StageAll | Hunks | Commit | Edit | Resolve | BranchCreate | BranchSwitch
            | Compare | MergePreview | Rebase | TagCreate | WorktreeAdd | WorktreeOpen | Review
            | Log | Theme => false,
        }
    }
    pub const ALL: [Self; 42] = [
        Self::Stage,
        Self::Unstage,
        Self::StageAll,
        Self::UnstageAll,
        Self::Hunks,
        Self::Commit,
        Self::Amend,
        Self::Discard,
        Self::Edit,
        Self::Resolve,
        Self::TakeCurrent,
        Self::TakeIncoming,
        Self::DeleteConflict,
        Self::BranchCreate,
        Self::BranchSwitch,
        Self::BranchRename,
        Self::BranchDelete,
        Self::Compare,
        Self::MergePreview,
        Self::Merge,
        Self::CherryPick,
        Self::Revert,
        Self::Rebase,
        Self::Fetch,
        Self::PullFF,
        Self::PullMerge,
        Self::PullRebase,
        Self::Push,
        Self::PushUpstream,
        Self::StashSave,
        Self::StashApply,
        Self::StashPop,
        Self::StashDrop,
        Self::TagCreate,
        Self::TagDelete,
        Self::WorktreeAdd,
        Self::WorktreeOpen,
        Self::WorktreeRemove,
        Self::Continue,
        Self::Skip,
        Self::Abort,
        Self::Review,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "Change theme",
            Self::Stage => "Stage selected file / folder",
            Self::Unstage => "Unstage selected file / folder",
            Self::StageAll => "Stage all files",
            Self::UnstageAll => "Unstage all files",
            Self::Hunks => "Stage / unstage individual hunks",
            Self::Commit => "Commit staged changes",
            Self::Amend => "Amend HEAD commit",
            Self::Discard => "Discard unstaged changes in selected file",
            Self::Edit => "Open selected file in external editor",
            Self::Resolve => "Resolve selected conflict",
            Self::TakeCurrent => "Conflict: take whole current file",
            Self::TakeIncoming => "Conflict: take whole incoming file",
            Self::DeleteConflict => "Conflict: delete file",
            Self::BranchCreate => "Create and switch to new branch",
            Self::BranchSwitch => "Switch to selected branch",
            Self::BranchRename => "Rename selected branch",
            Self::BranchDelete => "Delete selected branch (merged only)",
            Self::Compare => "Compare selected revision to HEAD",
            Self::MergePreview => "Preview merge from selected revision",
            Self::Merge => "Merge selected revision into current branch",
            Self::CherryPick => "Cherry-pick selected commit",
            Self::Revert => "Revert selected commit",
            Self::Rebase => "Interactive rebase: use selected commit as base",
            Self::Fetch => "Fetch selected remote",
            Self::PullFF => "Pull: fast-forward only",
            Self::PullMerge => "Pull: merge",
            Self::PullRebase => "Pull: rebase",
            Self::Push => "Push current branch (never force)",
            Self::PushUpstream => "Push and set upstream",
            Self::StashSave => "Stash changes including untracked files",
            Self::StashApply => "Apply selected stash (keep stash)",
            Self::StashPop => "Pop selected stash",
            Self::StashDrop => "Drop selected stash",
            Self::TagCreate => "Create tag at HEAD",
            Self::TagDelete => "Delete selected local tag",
            Self::WorktreeAdd => "Add detached worktree at HEAD",
            Self::WorktreeOpen => "Open selected worktree",
            Self::WorktreeRemove => "Remove selected clean worktree",
            Self::Continue => "Continue current merge / rebase / operation",
            Self::Skip => "Skip current replayed commit",
            Self::Abort => "Abort current operation",
            Self::Review => "Review full integration diff against HEAD",
            Self::Log => "Show last Git operation output",
        }
    }

    pub fn available(self, app: &App) -> bool {
        use CommandId::*;
        match self {
            Stage | Unstage => app.workspace == Workspace::Files && !app.snapshot.files.is_empty(),
            Hunks | Discard | Edit => app.workspace == Workspace::Files && app.file_selected(),
            Resolve | TakeCurrent | TakeIncoming | DeleteConflict => {
                app.workspace == Workspace::Files
                    && app.file_selected()
                    && app
                        .snapshot
                        .files
                        .get(app.selected)
                        .is_some_and(|file| file.is_conflict())
            }
            BranchSwitch | BranchRename | BranchDelete => {
                app.workspace == Workspace::Branches && !app.snapshot.branches.is_empty()
            }
            Compare | MergePreview | Merge => {
                matches!(
                    app.workspace,
                    Workspace::History | Workspace::Branches | Workspace::Tags
                ) && app.revision().is_ok()
            }
            CherryPick | Revert | Rebase => {
                app.workspace == Workspace::History && !app.snapshot.commits.is_empty()
            }
            StashApply | StashPop | StashDrop => {
                app.workspace == Workspace::Stashes && !app.snapshot.stashes.is_empty()
            }
            TagDelete => app.workspace == Workspace::Tags && !app.snapshot.tags.is_empty(),
            WorktreeOpen | WorktreeRemove => {
                app.workspace == Workspace::Worktrees && !app.snapshot.worktrees.is_empty()
            }
            Continue | Abort => app.snapshot.operation.is_some(),
            Skip => app
                .snapshot
                .operation
                .is_some_and(|operation| operation != crate::git::Operation::Merge),
            _ => true,
        }
    }
}

pub enum Task {
    Git(Action),
    Hunk(Hunks),
    Rebase(Plan),
    Preview(Action),
    SaveConflict(Box<Conflict>),
}

pub struct CommitView {
    pub commit: CommitFiles,
    pub selected: usize,
    pub focus: Focus,
    pub full: bool,
    pub preview: String,
    pub scroll: usize,
    pub horizontal: usize,
    pub history: History,
}

impl CommitView {
    fn new(commit: CommitFiles, preferences: Option<Preferences>) -> Self {
        let mut history = History::new(&commit.files);
        let mut selected = history
            .tree
            .selected()
            .and_then(|row| row.file)
            .unwrap_or(0);
        let mut full = false;
        let mut focus = Focus::List;
        if let Some(preferences) = preferences {
            selected = preferences
                .active
                .and_then(|path| commit.files.iter().position(|file| file.path == path))
                .unwrap_or(selected);
            history
                .tree
                .restore_collapsed(preferences.collapsed, selected, preferences.cursor);
            history.layout = preferences.layout;
            history.search.query = preferences.query;
            full = preferences.full;
            focus = if preferences.preview_focus {
                Focus::Preview
            } else {
                Focus::List
            };
        }
        Self {
            commit,
            selected,
            focus,
            full,
            preview: String::new(),
            scroll: 0,
            horizontal: 0,
            history,
        }
    }

    fn preferences(&self) -> Preferences {
        Preferences {
            collapsed: self.history.tree.collapsed.clone(),
            active: self
                .commit
                .files
                .get(self.selected)
                .map(|file| file.path.clone()),
            layout: self.history.layout,
            full: self.full,
            query: self.history.search.query.clone(),
            cursor: self
                .history
                .tree
                .selected()
                .map(|row| (row.path.clone(), row.file.is_none())),
            preview_focus: self.focus == Focus::Preview,
        }
    }

    fn remember(&mut self) {
        if !self.history.loading
            && let Some(file) = self.commit.files.get(self.selected)
        {
            self.history.positions.insert(
                (file.path.clone(), self.full),
                Position {
                    source: self.scroll,
                    horizontal: self.horizontal,
                },
            );
        }
    }

    fn select(&mut self, index: usize) -> bool {
        if self.selected == index {
            return false;
        }
        self.remember();
        self.selected = index;
        self.history.tree.select_file(index);
        true
    }

    fn start_loading(&mut self) {
        let position = self
            .commit
            .files
            .get(self.selected)
            .and_then(|file| self.history.positions.get(&(file.path.clone(), self.full)))
            .copied()
            .unwrap_or_default();
        self.scroll = position.source;
        self.horizontal = position.horizontal;
        self.preview = "Loading file…".into();
        self.history.loading = true;
        self.history.document = Default::default();
        self.history
            .search
            .reindex(&self.history.document, self.scroll);
    }

    fn loaded(&mut self, preview: HistoricalPreview, width: u16) {
        self.preview = preview.text;
        self.history.document = preview.document;
        self.history.loading = false;
        self.scroll = self
            .scroll
            .min(self.history.document.lines.len().saturating_sub(1));
        self.history
            .search
            .reindex(&self.history.document, self.scroll);
        self.jump_to_match(width);
    }

    fn jump_to_match(&mut self, width: u16) {
        if let Some(matched) = self.history.search.current() {
            self.scroll = matched.line;
            if let Some(line) = self.history.document.lines.get(matched.line) {
                let display_width =
                    |text: &str| ratatui::text::Span::raw(ui::safe_text(text)).width();
                let mut column = display_width(&line[..matched.bytes.start]);
                if self.history.split(width, self.full)
                    && let Some(row) = self
                        .history
                        .document
                        .rows
                        .get(self.history.document.row_for(matched.line))
                {
                    for cell in row.left.iter().chain(row.right.iter()) {
                        if cell.source == matched.line {
                            column = display_width(
                                &cell.text[..matched.bytes.start.saturating_sub(cell.prefix)],
                            );
                            break;
                        }
                        if let Some((source, annotation)) = &cell.annotation
                            && *source == matched.line
                        {
                            column = display_width(&cell.text)
                                + 2
                                + display_width(&annotation[..matched.bytes.start]);
                            break;
                        }
                    }
                }
                self.horizontal = column.saturating_sub(8);
            }
        }
    }

    fn edit_search(&mut self, key: KeyEvent, width: u16) {
        let search = &mut self.history.search;
        match key.code {
            KeyCode::Esc => search.editing = None,
            KeyCode::Enter => {
                search.accept(&self.history.document, self.scroll);
                if !search.query.is_empty() {
                    self.focus = Focus::Preview;
                }
                self.jump_to_match(width);
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut search.editing {
                    input.pop();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = &mut search.editing {
                    input.clear();
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && !character.is_control() =>
            {
                if let Some(input) = &mut search.editing
                    && input.len() + character.len_utf8() <= 1024
                {
                    input.push(character);
                }
            }
            _ => {}
        }
    }
}

pub enum PromptKind {
    Command(CommandId),
    Reword(Box<Plan>),
}

pub enum Modal {
    Theme {
        names: Vec<String>,
        selected: usize,
        original: Theme,
    },
    Loading(String),
    Help {
        scroll: u16,
    },
    Palette {
        filter: String,
        selected: usize,
    },
    Prompt {
        kind: PromptKind,
        caption: String,
        input: String,
    },
    Confirm {
        task: Task,
        detail: String,
    },
    Output {
        title: String,
        text: String,
        scroll: u16,
        horizontal: u16,
    },
    Conflict(Box<Conflict>),
    Inline {
        conflict: Box<Conflict>,
        editor: Box<editor::ResultEditor>,
    },
    Commit(Box<CommitView>),
    Hunks(Box<Hunks>),
    Rebase(Box<Plan>),
}

struct WorkerResult {
    label: String,
    output: std::result::Result<GitOutput, String>,
    snapshot: std::result::Result<Snapshot, String>,
    preview: bool,
    conflict: Option<Box<Conflict>>,
}

pub struct App {
    pub repo: Repo,
    pub snapshot: Snapshot,
    pub workspace: Workspace,
    pub selected: usize,
    pub focus: Focus,
    pub preview: String,
    pub scroll: u16,
    pub horizontal: u16,
    pub modal: Option<Modal>,
    pub notice: String,
    pub error: Option<String>,
    pub busy: Option<String>,
    pub quit_requested: bool,
    pub last_output: String,
    pub loaded: bool,
    pub preview_loading: bool,
    pub confirm_scroll: u16,
    pub screen_width: u16,
    pub theme: Theme,
    pub config: Config,
    pub config_root: Option<PathBuf>,
    pub explorer: crate::history::FileTree,
    history_preferences: Option<Preferences>,
    loader: Loader,
    next_read: u64,
    snapshot_read: Option<u64>,
    preview_read: Option<u64>,
    detail_read: Option<u64>,
    commit_read: Option<u64>,
    deferred_snapshot: Option<Box<(Repo, Snapshot)>>,
    last_refresh: Instant,
    worker: Option<Receiver<WorkerResult>>,
    edit_path: Option<PathBuf>,
    quit: bool,
}

impl App {
    pub fn new(path: &Path) -> Result<Self> {
        let repo = Repo {
            root: std::env::current_dir()?.join(path),
        };
        let snapshot = Snapshot::loading();
        let explorer = tree::explorer(&[], &repo.root);
        let config_root = config::directory();
        let loaded_config = config_root.as_deref().map(Config::load).transpose();
        let (config, mut config_error) = match loaded_config {
            Ok(config) => (config.unwrap_or_default(), None),
            Err(error) => (Config::default(), Some(error.to_string())),
        };
        let theme = match config_root
            .as_deref()
            .map(|root| config.resolve(root, &config.theme))
            .transpose()
        {
            Ok(theme) => theme.unwrap_or_default(),
            Err(error) => {
                config_error = Some(error.to_string());
                Theme::default()
            }
        };
        let mut app = Self {
            repo,
            snapshot,
            workspace: Workspace::Files,
            selected: 0,
            focus: Focus::List,
            preview: String::new(),
            scroll: 0,
            horizontal: 0,
            modal: None,
            notice: "Ready · : command search · ? help".into(),
            error: config_error,
            busy: None,
            quit_requested: false,
            last_output: "No operations yet.".into(),
            loaded: false,
            preview_loading: false,
            confirm_scroll: 0,
            screen_width: 120,
            theme,
            config,
            config_root,
            explorer,
            history_preferences: None,
            loader: Loader::new(),
            next_read: 0,
            snapshot_read: None,
            preview_read: None,
            detail_read: None,
            commit_read: None,
            deferred_snapshot: None,
            last_refresh: Instant::now(),
            worker: None,
            edit_path: None,
            quit: false,
        };
        app.preview = "Loading repository… Navigation and quit remain available.".into();
        app.refresh();
        Ok(app)
    }

    #[cfg(test)]
    pub fn ready(path: &Path) -> Result<Self> {
        let mut app = Self::new(path)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while (!app.loaded || app.preview_loading) && Instant::now() < deadline {
            app.poll_loading();
            if app.snapshot_read.is_none() && !app.loaded {
                anyhow::bail!(
                    "{}",
                    app.error.as_deref().unwrap_or("Failed to load repository")
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            app.loaded && !app.preview_loading,
            "Repository loading timed out"
        );
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let paste = editor::PasteMode::enable()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
        }
        let mut redraw = true;
        while !self.quit {
            redraw |= self.poll_worker();
            redraw |= self.poll_loading();
            self.auto_refresh(Instant::now());
            if shutdown.load(Ordering::Relaxed) && !self.quit_requested {
                self.quit_requested = true;
                redraw = true;
            }
            if self.quit_requested && self.busy.is_none() {
                self.quit = true;
                continue;
            }
            if let Some(path) = self.edit_path.take() {
                paste.suspend()?;
                ratatui::restore();
                let result = editor::open(&path);
                *terminal = ratatui::init();
                paste.resume()?;
                terminal.clear()?;
                if let Err(error) = result {
                    self.fail(error);
                }
                self.refresh();
                redraw = true;
            }
            if redraw {
                self.update_width(terminal.size()?.width);
                terminal.draw(|frame| ui::draw(frame, self))?;
                redraw = false;
            }
            if event::poll(Duration::from_millis(80))? {
                redraw = true;
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let size = terminal.size()?;
                        self.update_width(size.width);
                        let (width, height) = ui::minimum_size(self);
                        if (size.width < width || size.height < height)
                            && !matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                            && !(key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL))
                        {
                            self.notice = format!(
                                "Resize to at least {width} × {height} to use this workspace."
                            );
                        } else {
                            self.handle_key(key);
                        }
                    }
                    Event::Paste(text) => {
                        let size = terminal.size()?;
                        let (width, height) = ui::minimum_size(self);
                        if size.width >= width && size.height >= height {
                            self.handle_paste(&text);
                        }
                    }
                    Event::Resize(width, _) => {
                        self.update_width(width);
                    }
                    Event::Mouse(mouse) if self.modal.is_none() => match mouse.kind {
                        event::MouseEventKind::ScrollDown => self.navigate(3),
                        event::MouseEventKind::ScrollUp => self.navigate(-3),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn update_width(&mut self, width: u16) {
        if let Some(Modal::Commit(view)) = &mut self.modal
            && view.history.split(width, view.full)
                != view.history.split(self.screen_width, view.full)
            && view
                .history
                .search
                .current()
                .is_some_and(|matched| matched.line == view.scroll)
        {
            view.jump_to_match(width);
        }
        self.screen_width = width;
    }

    pub fn items(&self) -> Vec<String> {
        match self.workspace {
            Workspace::Files => self
                .snapshot
                .files
                .iter()
                .map(|file| {
                    format!(
                        "{} {}{}",
                        file.status,
                        file.path.display(),
                        if file.is_conflict() { "  !" } else { "" }
                    )
                })
                .collect(),
            Workspace::History => self
                .snapshot
                .commits
                .iter()
                .map(|commit| {
                    let mut row = format!(
                        "{}{} {} {}",
                        commit.graph, commit.short, commit.subject, commit.refs
                    );
                    for connector in &commit.connectors {
                        row.push('\n');
                        row.push_str(connector);
                    }
                    row
                })
                .collect(),
            Workspace::Branches => self
                .snapshot
                .branches
                .iter()
                .map(|branch| {
                    format!(
                        "{} {} {}",
                        if branch.current { "●" } else { " " },
                        branch.name,
                        branch.tracking
                    )
                })
                .collect(),
            Workspace::Remotes => self.snapshot.remotes.clone(),
            Workspace::Stashes => self
                .snapshot
                .stashes
                .iter()
                .map(|stash| format!("{} {}", stash.name, stash.subject))
                .collect(),
            Workspace::Tags => self.snapshot.tags.clone(),
            Workspace::Worktrees => self
                .snapshot
                .worktrees
                .iter()
                .map(|worktree| {
                    format!(
                        "{} · {}{}",
                        worktree.path.display(),
                        worktree.branch,
                        if worktree.locked { " [locked]" } else { "" }
                    )
                })
                .collect(),
        }
    }

    pub fn palette(&self, filter: &str) -> Vec<CommandId> {
        CommandId::ALL
            .into_iter()
            .chain([CommandId::Log])
            .chain([CommandId::Theme])
            .filter(|command| {
                command.available(self)
                    && command
                        .label()
                        .to_lowercase()
                        .contains(&filter.to_lowercase())
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.busy.is_some() {
                self.quit_requested = true;
            } else if matches!(&self.modal, Some(Modal::Conflict(conflict)) if conflict.dirty())
                || matches!(&self.modal, Some(Modal::Inline { conflict, editor }) if conflict.dirty() || editor.text() != conflict.result())
                || matches!(&self.modal, Some(Modal::Confirm { task: Task::SaveConflict(conflict), .. }) if conflict.dirty())
            {
                self.error = Some(
                    "Unsaved conflict choices. Save, or q then y to discard the draft.".into(),
                );
            } else {
                self.quit = true;
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.handle_modal(modal, key);
            return;
        }
        if self.busy.is_some() {
            if key.code == KeyCode::Char('q') {
                self.quit_requested = true;
                return;
            }
            // The mutation worker owns writes, not the UI. Keep read-only
            // navigation live; do not queue mutations against an old snapshot.
            if !matches!(
                key.code,
                KeyCode::Char('?')
                    | KeyCode::Char('1'..='7')
                    | KeyCode::Char('[' | ']' | 'j' | 'k' | 'h' | 'l')
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Esc
            ) {
                self.notice =
                    "Git is running; navigation is available, further mutations are locked.".into();
                return;
            }
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('!') => self.output("Last error / operation", self.last_output.clone()),
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.modal = Some(Modal::Palette {
                    filter: String::new(),
                    selected: 0,
                })
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char(']') => self.switch(self.workspace.next(false)),
            KeyCode::Char('[') => self.switch(self.workspace.next(true)),
            KeyCode::Char(number @ '1'..='7') => {
                self.switch(Workspace::ALL[number as usize - '1' as usize])
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::List {
                    Focus::Preview
                } else {
                    Focus::List
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::Char('h') | KeyCode::Left
                if self.workspace == Workspace::Files && self.focus == Focus::List =>
            {
                self.explorer.collapse();
                self.load_preview();
            }
            KeyCode::Char('l') | KeyCode::Right
                if self.workspace == Workspace::Files && self.focus == Focus::List =>
            {
                if self.file_selected() {
                    self.focus = Focus::Preview;
                } else {
                    if let Some(index) = self.explorer.expand() {
                        self.selected = index;
                    }
                    self.load_preview();
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.horizontal = self.horizontal.saturating_sub(4)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.horizontal = self.horizontal.saturating_add(4)
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(12).min(
                    self.preview
                        .lines()
                        .count()
                        .saturating_sub(1)
                        .min(u16::MAX as usize) as u16,
                )
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(12),
            KeyCode::Home => {
                self.scroll = 0;
                self.horizontal = 0;
                if self.focus == Focus::List {
                    if self.workspace == Workspace::Files {
                        if let Some(index) = self.explorer.navigate(isize::MIN) {
                            self.selected = index;
                        }
                    } else {
                        self.selected = 0;
                    }
                    self.load_preview();
                }
            }
            KeyCode::Esc => self.error = None,
            KeyCode::Char(' ')
                if self.workspace == Workspace::Files && self.focus == Focus::List =>
            {
                self.toggle_staging();
            }
            KeyCode::Char('s') if self.workspace == Workspace::Files => {
                self.invoke(CommandId::Stage)
            }
            KeyCode::Char('u') if self.workspace == Workspace::Files => {
                self.invoke(CommandId::Unstage)
            }
            KeyCode::Char('S') => self.invoke(CommandId::StageAll),
            KeyCode::Char('U') => self.invoke(CommandId::UnstageAll),
            KeyCode::Char('v') if self.workspace == Workspace::Files => {
                self.invoke(CommandId::Hunks)
            }
            KeyCode::Char('c') => self.invoke(CommandId::Commit),
            KeyCode::Char('a') => self.invoke(CommandId::Amend),
            KeyCode::Char('e') if self.workspace == Workspace::Files => {
                self.invoke(CommandId::Edit)
            }
            KeyCode::Char('m') => self.invoke(CommandId::MergePreview),
            KeyCode::Char('M') => self.invoke(CommandId::Merge),
            KeyCode::Char('b') => self.invoke(CommandId::BranchCreate),
            KeyCode::Char('i') => self.invoke(CommandId::Rebase),
            KeyCode::Char('f') => self.invoke(CommandId::Fetch),
            KeyCode::Char('p') => self.invoke(CommandId::PullFF),
            KeyCode::Char('P') => self.invoke(CommandId::Push),
            KeyCode::Char('C') => self.invoke(CommandId::Continue),
            KeyCode::Char('A') => self.invoke(CommandId::Abort),
            KeyCode::Char('~') => self.invoke(CommandId::Log),
            KeyCode::Enter => match self.workspace {
                Workspace::Files if self.focus == Focus::List && !self.file_selected() => {
                    self.explorer.toggle();
                    self.load_preview();
                }
                Workspace::History => {
                    self.history_preferences = None;
                    if let Some(commit) = self.snapshot.commits.get(self.selected) {
                        self.detail(
                            ReadKind::Commit {
                                oid: commit.oid.clone(),
                                parent: 0,
                            },
                            "Loading commit files…",
                        );
                    }
                }
                Workspace::Files
                    if self
                        .snapshot
                        .files
                        .get(self.selected)
                        .is_some_and(|file| file.is_conflict()) =>
                {
                    self.invoke(CommandId::Resolve)
                }
                Workspace::Branches => self.invoke(CommandId::BranchSwitch),
                Workspace::Worktrees => self.invoke(CommandId::WorktreeOpen),
                _ => self.focus = Focus::Preview,
            },
            _ => {}
        }
    }

    fn handle_modal(&mut self, modal: Modal, key: KeyEvent) {
        let closing = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));
        match modal {
            Modal::Theme {
                names,
                mut selected,
                original,
            } => {
                if closing {
                    self.theme = original;
                    return;
                }
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(names.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Enter => {
                        if let (Some(root), Some(name)) = (&self.config_root, names.get(selected)) {
                            let result = self.config.resolve(root, name).and_then(|theme| {
                                Config::save_theme(root, name)?;
                                Ok(theme)
                            });
                            match result {
                                Ok(theme) => {
                                    self.theme = theme;
                                    self.config.theme = name.clone();
                                    self.notice = format!("Theme: {name}");
                                    return;
                                }
                                Err(error) => self.fail(error),
                            }
                        }
                    }
                    _ => {}
                }
                if let (Some(root), Some(name)) = (&self.config_root, names.get(selected)) {
                    match self.config.resolve(root, name) {
                        Ok(theme) => {
                            self.theme = theme;
                            if key.code != KeyCode::Enter {
                                self.error = None;
                            }
                        }
                        Err(error) => {
                            self.theme = original.clone();
                            self.fail(error);
                        }
                    }
                }
                self.modal = Some(Modal::Theme {
                    names,
                    selected,
                    original,
                });
            }
            Modal::Loading(label) => {
                if closing {
                    self.history_preferences = None;
                    self.detail_read = None;
                    self.request_preview(false);
                } else {
                    self.modal = Some(Modal::Loading(label));
                }
            }
            Modal::Help { mut scroll } => {
                if closing || key.code == KeyCode::Char('?') {
                    return;
                }
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        scroll = scroll.saturating_add(1).min(100)
                    }
                    KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll = scroll.saturating_add(8).min(100),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(8),
                    KeyCode::Home => scroll = 0,
                    _ => {}
                }
                self.modal = Some(Modal::Help { scroll });
            }
            Modal::Palette {
                mut filter,
                mut selected,
            } => {
                match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Down => {
                        selected = (selected + 1).min(self.palette(&filter).len().saturating_sub(1))
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Backspace => {
                        filter.pop();
                        selected = 0;
                    }
                    KeyCode::Char(character) => {
                        filter.push(character);
                        selected = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(command) = self.palette(&filter).get(selected).copied() {
                            self.invoke(command);
                        }
                        return;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Palette { filter, selected });
            }
            Modal::Prompt {
                kind,
                caption,
                mut input,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        if let PromptKind::Reword(plan) = kind {
                            self.modal = Some(Modal::Rebase(plan));
                        }
                        return;
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.clear()
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.push(character)
                    }
                    KeyCode::Enter => {
                        if input.trim().is_empty() {
                            self.error = Some("Please enter a non-empty value.".into());
                        } else {
                            match kind {
                                PromptKind::Command(command) => {
                                    if let Err(error) = self.prepare(command, Some(input)) {
                                        self.fail(error);
                                    }
                                }
                                PromptKind::Reword(mut plan) => {
                                    plan.steps[plan.selected].message = input;
                                    plan.steps[plan.selected].action = RebaseAction::Reword;
                                    self.modal = Some(Modal::Rebase(plan));
                                }
                            }
                            return;
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Prompt {
                    kind,
                    caption,
                    input,
                });
            }
            Modal::Confirm { task, detail } => match key.code {
                KeyCode::Char('y') => {
                    self.confirm_scroll = 0;
                    self.start(task);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.confirm_scroll = 0;
                    match task {
                        Task::Rebase(plan) => self.modal = Some(Modal::Rebase(Box::new(plan))),
                        Task::SaveConflict(conflict) => {
                            self.modal = Some(Modal::Conflict(conflict))
                        }
                        Task::Hunk(hunks) => self.modal = Some(Modal::Hunks(Box::new(hunks))),
                        _ => {}
                    }
                }
                _ => {
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.confirm_scroll = self.confirm_scroll.saturating_add(1)
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.confirm_scroll = self.confirm_scroll.saturating_sub(1)
                        }
                        KeyCode::PageDown => {
                            self.confirm_scroll = self.confirm_scroll.saturating_add(8)
                        }
                        KeyCode::PageUp => {
                            self.confirm_scroll = self.confirm_scroll.saturating_sub(8)
                        }
                        KeyCode::Home => self.confirm_scroll = 0,
                        _ => {}
                    }
                    self.modal = Some(Modal::Confirm { task, detail });
                }
            },
            Modal::Output {
                title,
                text,
                mut scroll,
                mut horizontal,
            } => {
                if closing {
                    return;
                }
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        scroll = scroll.saturating_add(1).min(
                            text.lines()
                                .count()
                                .saturating_sub(1)
                                .min(u16::MAX as usize) as u16,
                        )
                    }
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll = scroll.saturating_add(12),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(12),
                    KeyCode::Left | KeyCode::Char('h') => horizontal = horizontal.saturating_sub(4),
                    KeyCode::Right | KeyCode::Char('l') => {
                        horizontal = horizontal.saturating_add(4)
                    }
                    KeyCode::Home => {
                        scroll = 0;
                        horizontal = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Output {
                    title,
                    text,
                    scroll,
                    horizontal,
                });
            }
            Modal::Conflict(mut conflict) => {
                if conflict.confirm_close {
                    match key.code {
                        KeyCode::Char('y') => {
                            self.refresh();
                            return;
                        }
                        KeyCode::Char('n') | KeyCode::Esc => conflict.confirm_close = false,
                        _ => {}
                    }
                } else if closing {
                    if !conflict.dirty() {
                        self.refresh();
                        return;
                    }
                    conflict.confirm_close = true;
                } else {
                    let count = conflict.regions().len();
                    match key.code {
                        KeyCode::Char('1') => conflict.choose(Choice::Current),
                        KeyCode::Char('2') => conflict.choose(Choice::Incoming),
                        KeyCode::Char('3') => conflict.choose(Choice::CurrentThenIncoming),
                        KeyCode::Char('4') => conflict.choose(Choice::IncomingThenCurrent),
                        KeyCode::Char('u') => conflict.undo(),
                        KeyCode::Char('n') | KeyCode::Char(']') => {
                            conflict.selected = (conflict.selected + 1) % count.max(1);
                            conflict.scroll = 0;
                        }
                        KeyCode::Char('p') | KeyCode::Char('[') => {
                            conflict.selected =
                                (conflict.selected + count.max(1) - 1) % count.max(1);
                            conflict.scroll = 0;
                        }
                        KeyCode::Char('b') => conflict.show_base = !conflict.show_base,
                        KeyCode::Char('j') | KeyCode::Down => {
                            conflict.scroll = conflict.scroll.saturating_add(1)
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            conflict.scroll = conflict.scroll.saturating_sub(1)
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            conflict.horizontal = conflict.horizontal.saturating_sub(4)
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            conflict.horizontal = conflict.horizontal.saturating_add(4)
                        }
                        KeyCode::Char('s') => {
                            self.confirm_save(conflict);
                            return;
                        }
                        KeyCode::Char('a') => {
                            if conflict.dirty() || conflict.unresolved() > 0 {
                                self.error = Some(
                                    "Resolve all regions and press s to save before staging."
                                        .into(),
                                );
                            } else {
                                let action = Action {
                                    label: "Stage resolved file".into(),
                                    args: vec![
                                        OsString::from("add"),
                                        OsString::from("--"),
                                        conflict.path.as_os_str().to_owned(),
                                    ],
                                    input: None,
                                    clean: false,
                                    idle: false,
                                    guard: conflict.guard.clone(),
                                };
                                self.start(Task::Git(action));
                                return;
                            }
                        }
                        KeyCode::Char('e') => {
                            match editor::ResultEditor::new(&conflict.result()) {
                                Ok(mut editor) => {
                                    editor.set_theme(&self.theme);
                                    self.modal = Some(Modal::Inline {
                                        conflict,
                                        editor: Box::new(editor),
                                    })
                                }
                                Err(error) => {
                                    self.fail(error);
                                    self.modal = Some(Modal::Conflict(conflict));
                                }
                            }
                            return;
                        }
                        KeyCode::Char('E') => {
                            if conflict.dirty() {
                                self.error = Some(
                                    "Save your choices before opening the external editor.".into(),
                                );
                            } else {
                                match self.repo.safe_file(&conflict.path) {
                                    Ok(path) => self.edit_path = Some(path),
                                    Err(error) => self.fail(error),
                                };
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                self.modal = Some(Modal::Conflict(conflict));
            }
            Modal::Inline {
                mut conflict,
                mut editor,
            } => {
                let save =
                    key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Esc || save {
                    match conflict.edit_result(editor.text()) {
                        Ok(()) if save => self.confirm_save(conflict),
                        Ok(()) => self.modal = Some(Modal::Conflict(conflict)),
                        Err(error) => {
                            self.fail(error);
                            self.modal = Some(Modal::Inline { conflict, editor });
                        }
                    }
                    return;
                }
                if let KeyCode::Char(character) = key.code
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && (!editor::safe_editor_text(&character.to_string())
                        || editor.text().len() + character.len_utf8() > crate::git::PREVIEW_LIMIT)
                {
                    self.error = Some("Unsupported character or inline size limit reached.".into());
                } else if editor.area.input(key) && editor.text().len() > crate::git::PREVIEW_LIMIT
                {
                    editor.area.undo();
                    self.error = Some("Result exceeds the 256 KiB inline editing limit.".into());
                }
                self.modal = Some(Modal::Inline { conflict, editor });
            }
            Modal::Commit(mut view) => {
                if view.history.search.editing.is_some() {
                    view.edit_search(key, self.screen_width);
                    self.modal = Some(Modal::Commit(view));
                    return;
                }
                if closing {
                    self.commit_read = None;
                    self.request_preview(false);
                    return;
                }
                let mut reload = false;
                let split = view.history.split(self.screen_width, view.full);
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => {
                        view.focus = if view.focus == Focus::List {
                            Focus::Preview
                        } else {
                            Focus::List
                        }
                    }
                    KeyCode::Enter if view.focus == Focus::List => {
                        if !view.history.tree.toggle() {
                            view.focus = Focus::Preview;
                        }
                    }
                    KeyCode::Enter => {}
                    KeyCode::Down | KeyCode::Char('j') if view.focus == Focus::List => {
                        if let Some(index) = view.history.tree.navigate(1) {
                            reload = view.select(index);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') if view.focus == Focus::List => {
                        if let Some(index) = view.history.tree.navigate(-1) {
                            reload = view.select(index);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        view.scroll = view.history.document.step(view.scroll, 1, split);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        view.scroll = view.history.document.step(view.scroll, -1, split)
                    }
                    KeyCode::PageDown => {
                        view.scroll = view.history.document.step(view.scroll, 12, split);
                    }
                    KeyCode::PageUp => {
                        view.scroll = view.history.document.step(view.scroll, -12, split)
                    }
                    KeyCode::Right | KeyCode::Char('l') if view.focus == Focus::List => {
                        if view
                            .history
                            .tree
                            .selected()
                            .is_some_and(|row| row.file.is_some())
                        {
                            view.focus = Focus::Preview;
                        } else if let Some(index) = view.history.tree.expand() {
                            reload = view.select(index);
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') if view.focus == Focus::List => {
                        view.history.tree.collapse()
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        view.horizontal = view.horizontal.saturating_add(4)
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        view.horizontal = view.horizontal.saturating_sub(4)
                    }
                    KeyCode::Home => {
                        if view.focus == Focus::List {
                            if let Some(index) = view.history.tree.navigate(isize::MIN) {
                                reload = view.select(index);
                            }
                        } else {
                            view.scroll = 0;
                            view.horizontal = 0;
                        }
                    }
                    KeyCode::End if view.focus == Focus::List => {
                        if let Some(index) = view.history.tree.navigate(isize::MAX) {
                            reload = view.select(index);
                        }
                    }
                    KeyCode::End => view.scroll = view.history.document.last(split),
                    KeyCode::Char('[') | KeyCode::Char(']') => {
                        if let Some(index) = view
                            .history
                            .tree
                            .step_file(view.selected, key.code == KeyCode::Char('['))
                        {
                            reload = view.select(index);
                        }
                    }
                    KeyCode::Char('t') => {
                        view.remember();
                        view.full = !view.full;
                        reload = true;
                    }
                    KeyCode::Char('d') => {
                        view.history.layout = if view.history.layout == DiffLayout::Unified {
                            DiffLayout::SideBySide
                        } else {
                            DiffLayout::Unified
                        };
                        if view
                            .history
                            .search
                            .current()
                            .is_some_and(|matched| matched.line == view.scroll)
                        {
                            view.jump_to_match(self.screen_width);
                        }
                    }
                    KeyCode::Char('/') => view.history.search.begin(),
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        view.history.search.next(key.code == KeyCode::Char('N'));
                        view.jump_to_match(self.screen_width);
                    }
                    KeyCode::Char('}') if !view.full => {
                        if let Some(line) = view
                            .history
                            .document
                            .hunks
                            .iter()
                            .find(|index| **index > view.scroll)
                        {
                            view.scroll = *line;
                        }
                    }
                    KeyCode::Char('{') if !view.full => {
                        if let Some(line) = view
                            .history
                            .document
                            .hunks
                            .iter()
                            .rev()
                            .find(|index| **index < view.scroll)
                        {
                            view.scroll = *line;
                        }
                    }
                    KeyCode::Char('P') if view.commit.parents.len() > 1 => {
                        self.history_preferences = Some(view.preferences());
                        self.commit_read = None;
                        self.detail(
                            ReadKind::Commit {
                                oid: view.commit.oid.clone(),
                                parent: (view.commit.parent + 1) % view.commit.parents.len(),
                            },
                            "Loading other parent…",
                        );
                        return;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Commit(view));
                if reload {
                    self.load_commit_preview();
                }
            }
            Modal::Hunks(mut hunks) => {
                if closing {
                    return;
                }
                match key.code {
                    KeyCode::Char('n') => {
                        hunks.selected = (hunks.selected + 1) % hunks.bodies.len();
                        hunks.scroll = 0;
                    }
                    KeyCode::Char('p') => {
                        hunks.selected = hunks.selected.saturating_sub(1);
                        hunks.scroll = 0;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        hunks.scroll = hunks.scroll.saturating_add(1)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        hunks.scroll = hunks.scroll.saturating_sub(1)
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        hunks.horizontal = hunks.horizontal.saturating_sub(4)
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        hunks.horizontal = hunks.horizontal.saturating_add(4)
                    }
                    KeyCode::Char('s') => {
                        self.modal = Some(Modal::Confirm {
                            detail: format!(
                                "{} selected hunk in {:?}?\nThe index is updated; working files are not changed.",
                                if hunks.cached { "Unstage" } else { "Stage" },
                                hunks.path
                            ),
                            task: Task::Hunk(*hunks),
                        });
                        return;
                    }
                    KeyCode::Tab => {
                        if let Some(file) = self.snapshot.files.get(self.selected) {
                            self.detail(
                                ReadKind::Hunks {
                                    snapshot: Box::new(self.snapshot.clone()),
                                    file: file.clone(),
                                    cached: !hunks.cached,
                                },
                                "Loading hunks…",
                            );
                            return;
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Hunks(hunks));
            }
            Modal::Rebase(mut plan) => {
                if closing {
                    return;
                }
                let index = plan.selected;
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        plan.selected = (index + 1).min(plan.steps.len() - 1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => plan.selected = index.saturating_sub(1),
                    KeyCode::Char('J') if index + 1 < plan.steps.len() => {
                        plan.steps.swap(index, index + 1);
                        plan.selected += 1;
                    }
                    KeyCode::Char('K') if index > 0 => {
                        plan.steps.swap(index, index - 1);
                        plan.selected -= 1;
                    }
                    KeyCode::Char('p') => plan.steps[index].action = RebaseAction::Pick,
                    KeyCode::Char('s') => plan.steps[index].action = RebaseAction::Squash,
                    KeyCode::Char('f') => plan.steps[index].action = RebaseAction::Fixup,
                    KeyCode::Char('d') => plan.steps[index].action = RebaseAction::Drop,
                    KeyCode::Char('e') => plan.steps[index].action = RebaseAction::Edit,
                    KeyCode::Char('r') => {
                        let input = plan.steps[index].message.clone();
                        self.modal = Some(Modal::Prompt {
                            kind: PromptKind::Reword(plan),
                            caption: "New commit message (Esc returns to plan)".into(),
                            input,
                        });
                        return;
                    }
                    KeyCode::Enter => {
                        match plan.validate() {
                            Ok(()) => {
                                self.modal = Some(Modal::Confirm {
                                    detail: format!(
                                        "Rewrite {} commits on {}.\n{}\nOriginal HEAD will be preserved under refs/kae/backup/.\nNo push is performed.",
                                        plan.steps.len(),
                                        self.snapshot.branch,
                                        if plan.published {
                                            "WARNING: commits are reachable from remote-tracking branches. Coordinate with collaborators before rewriting shared history."
                                        } else {
                                            "Remote-tracking refs do not contain these commits (fetch first if uncertain)."
                                        }
                                    ),
                                    task: Task::Rebase(*plan),
                                })
                            }
                            Err(error) => {
                                self.fail(error);
                                self.modal = Some(Modal::Rebase(plan));
                            }
                        }
                        return;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Rebase(plan));
            }
        }
    }

    fn switch(&mut self, workspace: Workspace) {
        self.workspace = workspace;
        self.selected = 0;
        if workspace == Workspace::Files {
            self.explorer.select_file(0);
        }
        self.focus = Focus::List;
        self.load_preview();
    }

    fn navigate(&mut self, delta: isize) {
        if self.focus == Focus::Preview {
            self.scroll = self.scroll.saturating_add_signed(delta as i16).min(
                self.preview
                    .lines()
                    .count()
                    .saturating_sub(1)
                    .min(u16::MAX as usize) as u16,
            );
        } else {
            if self.workspace == Workspace::Files {
                if let Some(index) = self.explorer.navigate(delta) {
                    self.selected = index;
                }
                self.load_preview();
                return;
            }
            self.selected = self
                .selected
                .saturating_add_signed(delta)
                .min(self.items().len().saturating_sub(1));
            self.load_preview();
        }
    }

    fn load_preview(&mut self) {
        self.request_preview(true);
    }

    pub fn file_selected(&self) -> bool {
        self.explorer
            .selected()
            .is_some_and(|row| row.file == Some(self.selected))
            && self.snapshot.files.get(self.selected).is_some()
    }

    fn toggle_staging(&mut self) {
        let Some(row) = self.explorer.selected() else {
            return;
        };
        let files: Vec<_> = if let Some(index) = row.file {
            self.snapshot.files.get(index).into_iter().collect()
        } else {
            self.snapshot
                .files
                .iter()
                .filter(|file| file.path.starts_with(&row.path))
                .collect()
        };
        if files.iter().any(|file| file.status.as_bytes()[1] != b' ') {
            self.invoke(CommandId::Stage);
        } else if files
            .iter()
            .any(|file| !matches!(file.status.as_bytes()[0], b' ' | b'?'))
        {
            self.invoke(CommandId::Unstage);
        } else {
            self.notice = "No changes to stage or unstage in this selection.".into();
        }
    }

    fn prepare_folder(&mut self, command: CommandId) -> Result<()> {
        let row = self.explorer.selected().context("Select a folder")?;
        ensure!(row.file.is_none(), "Select a folder");
        let files = self
            .snapshot
            .files
            .iter()
            .filter(|file| file.path.starts_with(&row.path))
            .collect::<Vec<_>>();
        ensure!(!files.is_empty(), "No changed files in this folder");
        // Never silently mark unresolved conflicts as resolved in a bulk action.
        ensure!(
            command != CommandId::Stage || !files.iter().any(|file| file.is_conflict()),
            "Resolve and stage conflicted files individually before staging this folder"
        );
        let args: &[&str] = if command == CommandId::Stage {
            &["add", "--all"]
        } else if self.snapshot.head.is_some() {
            &["restore", "--staged"]
        } else {
            &["rm", "--cached", "-f"]
        };
        let mut action = Action::new(&self.snapshot, command.label(), args);
        action.args.push("--".into());
        let mut paths = std::collections::BTreeSet::new();
        for file in &files {
            // Untracked files have no index entry to restore/remove.
            if command == CommandId::Unstage && file.status == "??" {
                continue;
            }
            paths.insert(file.path.clone());
            if let Some(original) = &file.original
                && (command == CommandId::Unstage
                    || !matches!(file.status.as_bytes()[0], b'R' | b'C'))
            {
                paths.insert(original.clone());
            }
        }
        ensure!(!paths.is_empty(), "No staged files in this folder");
        let detail = format!(
            "{} folder {:?}?\n{} changed files; {} paths (including rename sources):\n{}\n\nWorking files are not deleted. Staging can replace previous staged-only content; unstaging can discard staged-only content.\nRename sources outside this folder are included explicitly.",
            if command == CommandId::Stage {
                "Stage"
            } else {
                "Unstage"
            },
            if row.path.as_os_str().is_empty() {
                &self.repo.root
            } else {
                &row.path
            },
            files.len(),
            paths.len(),
            paths
                .iter()
                .map(|path| format!("  {:?}", path))
                .collect::<Vec<_>>()
                .join("\n")
        );
        action
            .args
            .extend(paths.into_iter().map(|path| path.into_os_string()));
        self.modal = Some(Modal::Confirm {
            task: Task::Git(action),
            detail,
        });
        Ok(())
    }

    fn preview_target(&self) -> Preview {
        if self.workspace == Workspace::Files
            && !self.file_selected()
            && let Some(row) = self.explorer.selected()
        {
            if self.snapshot.files.is_empty() {
                return Preview::Empty(
                    "Working tree is clean.\n\nPress 2 for history or : for commands.".into(),
                );
            }
            return Preview::Empty(format!(
                "{}\n\n{} changed files\n\nEnter expand / collapse\ns stage changed files\nu review unstaging\n\nSelect a file to see its diff.",
                if row.path.as_os_str().is_empty() {
                    &self.repo.root
                } else {
                    &row.path
                }
                .display(),
                row.count
            ));
        }
        let empty = || Preview::Empty("Nothing here yet. Use : for commands.".into());
        match self.workspace {
            Workspace::Files => self
                .snapshot
                .files
                .get(self.selected)
                .cloned()
                .map(Preview::File)
                .unwrap_or_else(|| {
                    Preview::Empty(
                        "Working tree is clean.\n\nPress 2 for history or : for commands.".into(),
                    )
                }),
            Workspace::History => self
                .snapshot
                .commits
                .get(self.selected)
                .map(|commit| Preview::Revision {
                    revision: commit.oid.clone(),
                    heading: format!(
                        "{} · {}\nEnter to browse changed files",
                        commit.author, commit.age
                    ),
                })
                .unwrap_or_else(empty),
            Workspace::Branches => self
                .snapshot
                .branches
                .get(self.selected)
                .map(|branch| Preview::Revision {
                    revision: branch.oid.clone(),
                    heading: format!(
                        "Branch: {}\nUpstream: {} {}",
                        branch.name, branch.upstream, branch.tracking
                    ),
                })
                .unwrap_or_else(empty),
            Workspace::Remotes => self
                .snapshot
                .remotes
                .get(self.selected)
                .map(|remote| Preview::Remote {
                    name: remote.clone(),
                    branch: self.snapshot.branch.clone(),
                })
                .unwrap_or_else(|| {
                    Preview::Empty(
                        "No remotes configured. Add one with git remote add, then refresh.".into(),
                    )
                }),
            Workspace::Stashes => self
                .snapshot
                .stashes
                .get(self.selected)
                .map(|stash| Preview::Revision {
                    revision: stash.oid.clone(),
                    heading: stash.subject.clone(),
                })
                .unwrap_or_else(empty),
            Workspace::Tags => self
                .snapshot
                .tags
                .get(self.selected)
                .map(|tag| Preview::Revision {
                    revision: format!("refs/tags/{tag}"),
                    heading: tag.clone(),
                })
                .unwrap_or_else(empty),
            Workspace::Worktrees => self
                .snapshot
                .worktrees
                .get(self.selected)
                .cloned()
                .map(Preview::Worktree)
                .unwrap_or_else(empty),
        }
    }

    fn selected_key(&self) -> Option<OsString> {
        match self.workspace {
            Workspace::Files => self
                .snapshot
                .files
                .get(self.selected)
                .map(|file| file.path.as_os_str().to_owned()),
            Workspace::History => self
                .snapshot
                .commits
                .get(self.selected)
                .map(|commit| commit.oid.clone().into()),
            Workspace::Branches => self
                .snapshot
                .branches
                .get(self.selected)
                .map(|branch| branch.name.clone().into()),
            Workspace::Remotes => self
                .snapshot
                .remotes
                .get(self.selected)
                .cloned()
                .map(Into::into),
            Workspace::Stashes => self
                .snapshot
                .stashes
                .get(self.selected)
                .map(|stash| stash.oid.clone().into()),
            Workspace::Tags => self
                .snapshot
                .tags
                .get(self.selected)
                .cloned()
                .map(Into::into),
            Workspace::Worktrees => self
                .snapshot
                .worktrees
                .get(self.selected)
                .map(|tree| tree.path.as_os_str().to_owned()),
        }
    }

    fn request_preview(&mut self, reset: bool) {
        if !self.loaded {
            return;
        }
        if reset {
            self.scroll = 0;
            self.horizontal = 0;
            self.preview = "Loading preview…".into();
        }
        self.preview_loading = true;
        self.preview_read = Some(self.read(ReadKind::Preview(self.preview_target())));
    }

    fn revision(&self) -> Result<String> {
        match self.workspace {
            Workspace::History => self
                .snapshot
                .commits
                .get(self.selected)
                .map(|commit| commit.oid.clone()),
            Workspace::Branches => self
                .snapshot
                .branches
                .get(self.selected)
                .map(|branch| format!("refs/heads/{}", branch.name)),
            Workspace::Tags => self
                .snapshot
                .tags
                .get(self.selected)
                .map(|tag| format!("refs/tags/{tag}")),
            _ => None,
        }
        .context("Select a commit, branch or tag first.")
    }

    fn remote(&self) -> Result<String> {
        let upstream = self
            .snapshot
            .branches
            .iter()
            .find(|branch| branch.current)
            .map(|branch| branch.upstream.as_str())
            .unwrap_or_default();
        let remote = if self.workspace == Workspace::Remotes {
            self.snapshot.remotes.get(self.selected)
        } else {
            self.snapshot
                .remotes
                .iter()
                .filter(|remote| upstream.starts_with(&format!("{remote}/")))
                .max_by_key(|remote| remote.len())
                .or_else(|| {
                    self.snapshot
                        .remotes
                        .iter()
                        .find(|remote| remote.as_str() == "origin")
                })
                .or_else(|| self.snapshot.remotes.first())
        };
        remote
            .cloned()
            .context("No remote configured. Add one with git remote add, then refresh.")
    }

    fn invoke(&mut self, command: CommandId) {
        if self.busy.is_some() {
            self.notice = "Wait for the current Git operation before making another change.".into();
            return;
        }
        self.error = None;
        if !self.loaded {
            self.error = Some("Wait for repository loading to finish.".into());
            return;
        }
        if !command.available(self) {
            self.error = Some("Select an appropriate item in its workspace first.".into());
            return;
        }
        if let Err(error) = self.prepare(command, None) {
            self.fail(error);
        }
    }

    fn prompt(&mut self, command: CommandId, caption: &str, input: String) {
        self.modal = Some(Modal::Prompt {
            kind: PromptKind::Command(command),
            caption: caption.into(),
            input,
        });
    }

    fn prepare(&mut self, command: CommandId, input: Option<String>) -> Result<()> {
        use CommandId::*;
        if command == Theme {
            let root = self
                .config_root
                .as_ref()
                .context("Set HOME or XDG_CONFIG_HOME to use theme configuration")?;
            let names = Config::themes(root);
            let selected = names
                .iter()
                .position(|name| *name == self.config.theme)
                .unwrap_or(0);
            self.modal = Some(Modal::Theme {
                names,
                selected,
                original: self.theme.clone(),
            });
            return Ok(());
        }
        if matches!(command, Stage | Unstage) && !self.file_selected() {
            return self.prepare_folder(command);
        }
        if matches!(
            command,
            Hunks | Discard | Edit | Resolve | TakeCurrent | TakeIncoming | DeleteConflict
        ) {
            ensure!(
                self.file_selected(),
                "Select a file, not a folder, for this action"
            );
        }
        let new = |args: &[&str]| Action::new(&self.snapshot, command.label(), args);
        let mut confirm = false;
        let mut detail = command.label().to_owned();
        let action = match command {
            Theme => unreachable!(),
            Log => {
                self.output("Last Git operation", self.last_output.clone());
                return Ok(());
            }
            Edit => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                self.edit_path = Some(self.repo.safe_file(&file.path)?);
                return Ok(());
            }
            Resolve => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                self.detail(
                    ReadKind::Conflict {
                        path: file.path.clone(),
                        guard: self.snapshot.guard.clone(),
                        branch: self.snapshot.branch.clone(),
                    },
                    "Loading conflict…",
                );
                return Ok(());
            }
            Hunks => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                let cached = file.status.as_bytes()[1] == b' ';
                self.detail(
                    ReadKind::Hunks {
                        snapshot: Box::new(self.snapshot.clone()),
                        file: file.clone(),
                        cached,
                    },
                    "Loading hunks…",
                );
                return Ok(());
            }
            Rebase => {
                self.detail(
                    ReadKind::Rebase {
                        snapshot: Box::new(self.snapshot.clone()),
                        base: self.revision()?,
                    },
                    "Loading rebase plan…",
                );
                return Ok(());
            }
            WorktreeOpen => {
                let path = self
                    .snapshot
                    .worktrees
                    .get(self.selected)
                    .context("Select a worktree")?
                    .path
                    .clone();
                *self = Self::new(&path)?;
                return Ok(());
            }
            Compare => {
                let revision = self.revision()?;
                self.detail(ReadKind::Compare(revision), "Loading comparison…");
                return Ok(());
            }
            Review => {
                self.detail(
                    ReadKind::Review(self.snapshot.head.is_some()),
                    "Loading integration diff…",
                );
                return Ok(());
            }
            MergePreview => {
                let revision = self.revision()?;
                let action = new(&["merge-tree", "--write-tree", "HEAD", &revision]);
                self.start(Task::Preview(action));
                return Ok(());
            }
            Stage | Unstage | Discard | TakeCurrent | TakeIncoming | DeleteConflict => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                let mut action = match command {
                    Stage => {
                        if !matches!(file.status.as_bytes()[0], b' ' | b'?') && file.status.as_bytes()[1] != b' ' {
                            confirm = true;
                            detail = format!("Replace the staged version of {:?} with its current working content?\nThe previous staged-only content may no longer be recoverable through normal Git history.", file.path);
                        }
                        new(&["add"])
                    }
                    Unstage if self.snapshot.head.is_some() => new(&["restore", "--staged"]),
                    Unstage => new(&["rm", "--cached", "-f"]),
                    Discard => {
                        ensure!(!file.is_conflict(), "Use conflict side choices instead of discarding an unmerged file.");
                        confirm = true;
                        detail = format!("Discard UNSTAGED changes in {:?}?\n{}\nThis cannot be undone by Kae. Staged content is retained.", file.path, if file.status == "??" { "The untracked file will be permanently deleted." } else { "The working file will be restored from the index." });
                        if file.status == "??" { new(&["clean", "-f"]) } else { new(&["restore", "--worktree"]) }
                    }
                    TakeCurrent | TakeIncoming => {
                        confirm = true;
                        detail = format!("Replace the WHOLE working file {:?} with the {} index side.\nAny manual resolution in this file will be lost.\nDuring a rebase, current is the onto tree; incoming is the replayed commit.\nThe file will NOT be staged automatically.", file.path, if command == TakeCurrent { "current (stage 2)" } else { "incoming (stage 3)" });
                        new(&["checkout", if command == TakeCurrent { "--ours" } else { "--theirs" }])
                    }
                    DeleteConflict => { confirm = true; detail = format!("Delete conflicted file {:?} from the working tree AND stage its deletion?\nManual edits will be lost. Git will refuse unsafe removal without force.", file.path); new(&["rm"]) }
                    _ => unreachable!(),
                }.path(&file.path);
                if (command == Unstage
                    || (command == Stage && !matches!(file.status.as_bytes()[0], b'R' | b'C')))
                    && let Some(original) = &file.original
                {
                    action.args.push(original.as_os_str().to_owned());
                }
                action
            }
            StageAll => {
                if self.snapshot.files.iter().any(|file| {
                    !matches!(file.status.as_bytes()[0], b' ' | b'?')
                        && file.status.as_bytes()[1] != b' '
                }) {
                    confirm = true;
                    detail = "Stage all files, replacing any previously staged versions with working content?\nPrevious staged-only content can be lost. Working files are not deleted.".into();
                }
                new(&["add", "--all"])
            }
            UnstageAll if self.snapshot.head.is_some() => new(&["restore", "--staged", "--", "."]),
            UnstageAll => new(&["rm", "--cached", "-r", "-f", "--", "."]),
            Commit | Amend => {
                let Some(message) = input else {
                    if command == Amend {
                        self.detail(ReadKind::AmendMessage, "Loading commit message…");
                    } else {
                        self.prompt(command, "Commit message", String::new());
                    }
                    return Ok(());
                };
                if command == Amend {
                    confirm = true;
                    detail = "Amend HEAD with the staged changes and this message?\nThis rewrites history, possibly already shared. No push will be performed.".into();
                    new(&["commit", "--amend", "-m", &message])
                } else {
                    new(&["commit", "-m", &message])
                }
            }
            BranchCreate | BranchRename | StashSave | TagCreate | WorktreeAdd => {
                let Some(input) = input else {
                    let caption = match command {
                        BranchCreate => "New branch name (creates and switches)",
                        BranchRename => "New name for selected branch",
                        StashSave => "Stash message (includes untracked, not ignored files)",
                        TagCreate => "New lightweight tag at HEAD",
                        _ => "New worktree path (detached HEAD)",
                    };
                    self.prompt(command, caption, String::new());
                    return Ok(());
                };
                match command {
                    BranchCreate => new(&["switch", "-c", &input]).clean(),
                    BranchRename => {
                        let branch = &self
                            .snapshot
                            .branches
                            .get(self.selected)
                            .context("Select a branch")?
                            .name;
                        confirm = true;
                        detail = format!(
                            "Rename local branch {branch} to {input}?\nThe old local branch name will be removed. Remote refs are unchanged."
                        );
                        new(&["branch", "-m", "--", branch, &input]).idle()
                    }
                    StashSave => {
                        confirm = true;
                        detail = "Stash tracked and untracked changes, then remove them from the working tree?\nIgnored files are excluded. Review the resulting stash before dropping it.".into();
                        new(&["stash", "push", "--include-untracked", "-m", &input])
                    }
                    TagCreate => new(&["tag", "--", &input, "HEAD"]),
                    WorktreeAdd => new(&["worktree", "add", "--detach", "--", &input, "HEAD"]),
                    _ => unreachable!(),
                }
            }
            BranchSwitch | BranchDelete => {
                let branch = &self
                    .snapshot
                    .branches
                    .get(self.selected)
                    .context("Select a branch")?
                    .name;
                if command == BranchSwitch {
                    new(&["switch", "--", branch]).clean()
                } else {
                    confirm = true;
                    detail = format!(
                        "Delete local branch {branch}?\nOnly branches Git considers merged can be deleted. Remote branches are unchanged."
                    );
                    new(&["branch", "-d", "--", branch]).idle()
                }
            }
            Merge | CherryPick | Revert => {
                let revision = self.revision()?;
                confirm = true;
                detail = format!(
                    "{}\nSource: {}\nDestination: {}\nRequires a clean tree. Conflicts will remain available for resolution.",
                    command.label(),
                    self.items()
                        .get(self.selected)
                        .map(String::as_str)
                        .unwrap_or(&revision),
                    self.snapshot.branch
                );
                match command {
                    Merge => new(&[
                        "merge",
                        "--ff",
                        "--no-edit",
                        "--no-autostash",
                        "--",
                        &revision,
                    ])
                    .clean(),
                    CherryPick => new(&["cherry-pick", "--", &revision]).clean(),
                    _ => new(&["revert", "--no-edit", "--", &revision]).clean(),
                }
            }
            Fetch | PullFF | PullMerge | PullRebase | Push | PushUpstream => {
                let remote = self.remote()?;
                detail = format!(
                    "{}\nRemote: {remote}\nBranch: {}\nNo force-push will be used.",
                    command.label(),
                    self.snapshot.branch
                );
                match command {
                    Fetch => new(&["fetch", "--no-prune", "--no-prune-tags", "--", &remote]),
                    PullFF => {
                        confirm = true;
                        new(&[
                            "pull",
                            "--ff-only",
                            "--no-rebase",
                            "--no-autostash",
                            "--",
                            &remote,
                        ])
                        .clean()
                    }
                    PullMerge => {
                        confirm = true;
                        new(&[
                            "pull",
                            "--ff",
                            "--no-rebase",
                            "--no-edit",
                            "--no-autostash",
                            "--",
                            &remote,
                        ])
                        .clean()
                    }
                    PullRebase => {
                        confirm = true;
                        detail.push_str("\nLocal commits may be rewritten by rebase.");
                        new(&["pull", "--ff", "--rebase", "--no-autostash", "--", &remote]).clean()
                    }
                    Push | PushUpstream => {
                        ensure!(
                            !self.snapshot.branch.starts_with("detached @"),
                            "Switch to a branch before pushing."
                        );
                        confirm = true;
                        let upstream = self
                            .snapshot
                            .branches
                            .iter()
                            .find(|branch| branch.current)
                            .map(|branch| branch.upstream.as_str())
                            .unwrap_or_default();
                        let destination = if command == Push {
                            upstream
                                .strip_prefix(&format!("{remote}/"))
                                .unwrap_or(&self.snapshot.branch)
                        } else {
                            &self.snapshot.branch
                        };
                        let refspec = format!("HEAD:refs/heads/{destination}");
                        detail.push_str(&format!("\nDestination remote branch: {destination}"));
                        (if command == Push {
                            new(&[
                                "push",
                                "--no-follow-tags",
                                "--recurse-submodules=no",
                                "--",
                                &remote,
                                &refspec,
                            ])
                        } else {
                            new(&[
                                "push",
                                "--no-follow-tags",
                                "--recurse-submodules=no",
                                "--set-upstream",
                                "--",
                                &remote,
                                &refspec,
                            ])
                        })
                        .idle()
                    }
                    _ => unreachable!(),
                }
            }
            StashApply | StashPop | StashDrop => {
                let stash = self
                    .snapshot
                    .stashes
                    .get(self.selected)
                    .context("Select a stash")?;
                confirm = true;
                detail = format!(
                    "{}\n{} {}\n{}",
                    command.label(),
                    stash.name,
                    stash.subject,
                    if command == StashDrop {
                        "Permanently removes this stash reference."
                    } else {
                        "Requires a clean tree. Conflicts may need resolution; Git retains a conflicting pop."
                    }
                );
                let action = new(&[
                    "stash",
                    match command {
                        StashApply => "apply",
                        StashPop => "pop",
                        _ => "drop",
                    },
                    &stash.name,
                ]);
                if command == StashDrop {
                    action
                } else {
                    action.clean()
                }
            }
            TagDelete => {
                let tag = self
                    .snapshot
                    .tags
                    .get(self.selected)
                    .context("Select a tag")?;
                confirm = true;
                detail = format!("Delete local tag {tag}? Remote tags are unchanged.");
                new(&["tag", "-d", "--", tag])
            }
            WorktreeRemove => {
                let worktree = self
                    .snapshot
                    .worktrees
                    .get(self.selected)
                    .context("Select a worktree")?;
                ensure!(
                    worktree.path.canonicalize()? != self.repo.root.canonicalize()?,
                    "Cannot remove the worktree currently open in Kae."
                );
                confirm = true;
                detail = format!(
                    "Remove worktree directory {:?}?\nGit will refuse dirty, locked or main worktrees. No force flag is used.",
                    worktree.path
                );
                new(&["worktree", "remove"]).path(&worktree.path)
            }
            Continue | Skip | Abort => {
                let operation = self
                    .snapshot
                    .operation
                    .context("No operation in progress")?;
                confirm = true;
                detail = if command == Abort {
                    format!(
                        "Abort {}?\nResolution progress and operation changes may be discarded as Git restores the pre-operation state. Copy any manual edits you want to retain first.",
                        operation.command()
                    )
                } else if command == Continue {
                    format!(
                        "Continue {}?\nThis may create commits, replay remaining commits, or complete a history rewrite.\nAll conflicted files must be resolved and staged first.",
                        operation.command()
                    )
                } else {
                    "Skip the current replayed commit?\nIts changes and resolution progress will be discarded.".into()
                };
                new(&[
                    operation.command(),
                    match command {
                        Continue => "--continue",
                        Skip => "--skip",
                        _ => "--abort",
                    },
                ])
            }
        };
        if confirm || command.needs_confirmation() {
            self.modal = Some(Modal::Confirm {
                task: Task::Git(action),
                detail,
            });
        } else {
            self.start(Task::Git(action));
        }
        Ok(())
    }

    fn start(&mut self, task: Task) {
        if self.busy.is_some() {
            return;
        }
        self.cancel_reads();
        let repo = self.repo.clone();
        let index_snapshot = matches!(&task, Task::Git(action) if action.updates_only_index())
            .then(|| self.snapshot.clone());
        let label = match &task {
            Task::Git(action) | Task::Preview(action) => action.label.clone(),
            Task::Hunk(_) => "Apply selected hunk".into(),
            Task::Rebase(_) => "Interactive rebase".into(),
            Task::SaveConflict(_) => "Save conflict resolution (not staged)".into(),
        };
        self.busy = Some(label.clone());
        self.error = None;
        let (sender, receiver) = mpsc::channel();
        self.worker = Some(receiver);
        thread::spawn(move || {
            let preview = matches!(task, Task::Preview(_));
            let mut saved_conflict = None;
            let output = match task {
                Task::Git(action) => repo.execute(&action),
                Task::Preview(action) => repo.merge_preview(&action),
                Task::Hunk(hunks) => hunks.apply(&repo),
                Task::Rebase(plan) => plan.execute(&repo),
                Task::SaveConflict(mut conflict) => {
                    let result = conflict.save(&repo).map(|()| GitOutput {
                        success: true,
                        stdout: b"Resolution saved to working tree. Not staged.\n".to_vec(),
                        stderr: Vec::new(),
                        truncated: false,
                        stdout_fingerprint: 0,
                    });
                    saved_conflict = Some(conflict);
                    result
                }
            }
            .map_err(|error| format!("{error:#}"));
            let snapshot = match index_snapshot {
                Some(previous) => repo.refresh_index(&previous),
                None => repo.snapshot(),
            }
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(WorkerResult {
                label,
                output,
                snapshot,
                preview,
                conflict: saved_conflict,
            });
        });
    }

    pub fn poll_worker(&mut self) -> bool {
        let result = match self.worker.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => {
                self.busy = None;
                self.worker = None;
                self.error = Some("Git worker stopped unexpectedly. Refresh and inspect repository state before retrying.".into());
                return true;
            }
            _ => return false,
        };
        self.busy = None;
        self.worker = None;
        match result.snapshot {
            Ok(snapshot) => self.apply_snapshot(self.repo.clone(), snapshot),
            Err(error) => self.error = Some(format!("Could not refresh repository: {error}")),
        }
        match result.output {
            Ok(output) => {
                self.last_output = format!("{}\n\n{}", result.label, output.text());
                if !output.success || result.preview {
                    self.output(
                        if result.preview {
                            "Merge preview · no working-tree/index changes; exit 1 means conflicts"
                        } else {
                            "Git operation stopped — inspect output and repository state"
                        },
                        self.last_output.clone(),
                    );
                } else {
                    self.notice = format!(
                        "{} {} · ~ shows output",
                        result.label,
                        if self.snapshot.operation.is_some() {
                            "paused"
                        } else {
                            "completed"
                        }
                    );
                    if self.snapshot.operation.is_some() {
                        self.notice.push_str(" · C continue / A abort");
                    }
                }
            }
            Err(error) => {
                self.last_output = format!("{}\n\n{error}", result.label);
                self.error = Some(error);
            }
        }
        if let Some(conflict) = result.conflict {
            if conflict.dirty() {
                self.quit_requested = false;
            }
            self.modal = Some(Modal::Conflict(conflict));
        }
        true
    }

    fn refresh(&mut self) {
        if self.busy.is_none() && self.snapshot_read.is_none() {
            self.snapshot_read = Some(self.read(ReadKind::Snapshot));
        }
    }

    pub fn auto_refresh(&mut self, now: Instant) {
        if self.busy.is_none()
            && self.modal.is_none()
            && (self.loaded || self.error.is_none())
            && now.saturating_duration_since(self.last_refresh) >= Duration::from_secs(2)
        {
            self.refresh();
        }
    }

    pub fn loading(&self) -> bool {
        self.snapshot_read.is_some()
            || self.preview_read.is_some()
            || self.detail_read.is_some()
            || self.commit_read.is_some()
    }

    fn read(&mut self, kind: ReadKind) -> u64 {
        self.next_read = self.next_read.wrapping_add(1);
        let id = self.next_read;
        self.loader.request(Request {
            id,
            repo: self.repo.clone(),
            kind,
        });
        id
    }

    fn detail(&mut self, kind: ReadKind, label: &str) {
        self.preview_read = None;
        self.preview_loading = false;
        self.commit_read = None;
        self.detail_read = Some(self.read(kind));
        self.modal = Some(Modal::Loading(label.into()));
    }

    fn confirm_save(&mut self, conflict: Box<Conflict>) {
        if !conflict.dirty() {
            self.modal = Some(Modal::Conflict(conflict));
            return;
        }
        self.modal = Some(Modal::Confirm {
            detail: format!(
                "Save the resolution over working file {:?}?\nThis replaces its on-disk contents, but does NOT stage or continue Git.\n{}\nExternal changes are checked again before writing.",
                conflict.path,
                if conflict.unresolved() > 0 {
                    "Unresolved markers remain. This is a partial save."
                } else {
                    "All regions are resolved."
                }
            ),
            task: Task::SaveConflict(conflict),
        });
    }

    fn cancel_reads(&mut self) {
        self.history_preferences = None;
        self.loader.clear();
        self.snapshot_read = None;
        self.preview_read = None;
        self.detail_read = None;
        self.commit_read = None;
        self.preview_loading = false;
        self.deferred_snapshot = None;
    }

    fn apply_snapshot(&mut self, repo: Repo, snapshot: Snapshot) {
        let key = self.selected_key();
        let index = self.selected;
        let cursor = (self.loaded && !self.snapshot.files.is_empty())
            .then(|| {
                self.explorer
                    .selected()
                    .map(|row| (row.path.clone(), row.file.is_none()))
            })
            .flatten();
        let collapsed = self.explorer.collapsed.clone();
        self.repo = repo;
        self.snapshot = snapshot;
        self.loaded = true;
        let count = self.items().len();
        let retained = key.and_then(|key| {
            (0..count).find(|candidate| {
                self.selected = *candidate;
                self.selected_key().as_ref() == Some(&key)
            })
        });
        self.selected = retained.unwrap_or_else(|| index.min(count.saturating_sub(1)));
        self.explorer = tree::explorer(&self.snapshot.files, &self.repo.root);
        if self.workspace == Workspace::Files {
            let cursor = if cursor.as_ref().is_some_and(|(_, directory)| *directory) {
                cursor
            } else {
                self.snapshot
                    .files
                    .get(self.selected)
                    .map(|file| (file.path.clone(), false))
            };
            self.explorer
                .restore_collapsed(collapsed, self.selected, cursor);
        } else {
            self.explorer.restore_collapsed(collapsed, 0, cursor);
        }
        if self.workspace == Workspace::Files
            && let Some(index) = self.explorer.selected().and_then(|row| row.file)
        {
            self.selected = index;
        }
        self.last_refresh = Instant::now();
        self.request_preview(retained.is_none());
    }

    fn load_commit_preview(&mut self) {
        if let Some(Modal::Commit(view)) = &mut self.modal {
            view.start_loading();
            let kind = ReadKind::CommitPreview {
                commit: view.commit.clone(),
                index: view.selected,
                full: view.full,
            };
            self.commit_read = Some(self.read(kind));
        }
    }

    pub fn poll_loading(&mut self) -> bool {
        let mut changed = false;
        loop {
            let response = match self.loader.try_recv() {
                Ok(response) => response,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.cancel_reads();
                    self.loader = Loader::new();
                    self.fail(anyhow::anyhow!("Repository reader stopped unexpectedly. Press r to retry; no Git action was executed."));
                    changed = true;
                    break;
                }
            };
            if self.snapshot_read == Some(response.id) {
                self.snapshot_read = None;
                self.last_refresh = Instant::now();
                match response.result {
                    Ok(ReadData::Snapshot(data)) => {
                        if self.modal.is_some() || self.busy.is_some() {
                            self.deferred_snapshot = Some(data);
                        } else {
                            let (repo, snapshot) = *data;
                            self.apply_snapshot(repo, snapshot);
                        }
                    }
                    Err(error) => {
                        if !self.loaded {
                            self.snapshot.branch = "Unavailable".into();
                            self.preview =
                                format!("Could not open repository:\n{error}\n\nr retry · q quit");
                        }
                        self.last_output = error.clone();
                        self.error = Some(error);
                    }
                    _ => {}
                }
                changed = true;
            } else if self.preview_read == Some(response.id) {
                self.preview_read = None;
                self.preview_loading = false;
                self.preview = match response.result {
                    Ok(ReadData::Preview(text)) => text,
                    Err(error) => format!("Could not load preview:\n{error}"),
                    _ => String::new(),
                };
                self.scroll = self.scroll.min(
                    self.preview
                        .lines()
                        .count()
                        .saturating_sub(1)
                        .min(u16::MAX as usize) as u16,
                );
                changed = true;
            } else if self.commit_read == Some(response.id) {
                self.commit_read = None;
                if let Some(Modal::Commit(view)) = &mut self.modal {
                    let preview = match response.result {
                        Ok(ReadData::CommitPreview(preview)) => *preview,
                        Err(error) => HistoricalPreview::new(
                            format!("Could not load file:\n{error}"),
                            view.full,
                        ),
                        _ => HistoricalPreview::new(String::new(), view.full),
                    };
                    view.loaded(preview, self.screen_width);
                    changed = true;
                }
            } else if self.detail_read == Some(response.id) {
                self.detail_read = None;
                if !matches!(self.modal, Some(Modal::Loading(_))) {
                    continue;
                }
                self.modal = None;
                match response.result {
                    Ok(ReadData::Conflict(conflict)) => {
                        self.modal = Some(Modal::Conflict(conflict))
                    }
                    Ok(ReadData::Hunks(hunks)) => self.modal = Some(Modal::Hunks(hunks)),
                    Ok(ReadData::Rebase(plan)) => self.modal = Some(Modal::Rebase(plan)),
                    Ok(ReadData::AmendMessage(message)) => {
                        self.prompt(CommandId::Amend, "Commit message", message)
                    }
                    Ok(ReadData::Output { title, text }) => self.output(&title, text),
                    Ok(ReadData::Commit(commit)) => {
                        self.modal = Some(Modal::Commit(Box::new(CommitView::new(
                            commit,
                            self.history_preferences.take(),
                        ))));
                        self.load_commit_preview();
                    }
                    Err(error) => {
                        self.history_preferences = None;
                        self.last_output = error.clone();
                        self.error = Some(error);
                    }
                    _ => {}
                }
                changed = true;
            }
        }
        if self.modal.is_none()
            && self.busy.is_none()
            && let Some(data) = self.deferred_snapshot.take()
        {
            let (repo, snapshot) = *data;
            self.apply_snapshot(repo, snapshot);
            changed = true;
        }
        changed
    }

    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.modal {
            Some(Modal::Commit(view)) => view.history.search.paste(text),
            Some(Modal::Inline { editor, .. }) => {
                if let Err(error) = editor.paste(text) {
                    self.fail(error);
                }
            }
            Some(Modal::Prompt { input, .. })
                if input.len() + text.len() <= crate::git::PREVIEW_LIMIT =>
            {
                input.extend(text.chars().filter(|character| {
                    !character.is_control() || matches!(character, '\n' | '\t')
                }));
            }
            Some(Modal::Palette { filter, selected }) if filter.len() + text.len() <= 4096 => {
                filter.extend(text.chars().filter(|character| !character.is_control()));
                *selected = 0;
            }
            _ => {} // Pasted y, commands, and shortcuts never execute actions.
        }
    }

    fn output(&mut self, title: &str, text: String) {
        self.modal = Some(Modal::Output {
            title: title.into(),
            text,
            scroll: 0,
            horizontal: 0,
        });
    }
    fn fail(&mut self, error: anyhow::Error) {
        self.last_output = format!("{error:#}");
        self.error = Some(format!("{error:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_remains_available_while_a_preview_reader_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        Repo {
            root: dir.path().into(),
        }
        .bytes(&["init", "-b", "main"])
        .unwrap();
        std::fs::write(dir.path().join("a"), "alpha").unwrap();
        std::fs::write(dir.path().join("b"), "beta").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        let (started, waiting) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = std::sync::Mutex::new(blocked);
        app.loader = Loader::with_reader(move |_, _| {
            started.send(()).unwrap();
            blocked.lock().unwrap().recv().unwrap();
            Ok(ReadData::Preview("slow preview".into()))
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Preview);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Help { .. })));
        release.send(()).unwrap();
    }

    #[test]
    fn navigation_stays_live_during_a_delayed_mutation_but_writes_are_not_queued() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        std::fs::write(dir.path().join("a"), "alpha").unwrap();
        std::fs::write(dir.path().join("b"), "beta").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        let (sender, receiver) = mpsc::channel();
        app.cancel_reads();
        app.worker = Some(receiver);
        app.busy = Some("Delayed stage".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.snapshot.files[app.selected].path, Path::new("b"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.preview_loading && Instant::now() < deadline {
            app.poll_loading();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(app.preview.contains("beta"));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.horizontal, 4);
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.workspace, Workspace::History);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('s'),
            KeyCode::Char('u'),
            KeyCode::Char('c'),
            KeyCode::Enter,
        ] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            assert!(app.modal.is_none());
            assert_eq!(app.busy.as_deref(), Some("Delayed stage"));
        }
        assert!(repo.bytes(&["ls-files"]).unwrap().is_empty());
        // Complete the simulated worker. The final snapshot must retain the
        // selection made during the wait, not jump to the original action target.
        let output = repo
            .run(&crate::git::strings(&["add", "--", "a"]), None)
            .unwrap();
        sender
            .send(WorkerResult {
                label: "Stage a".into(),
                output: Ok(output),
                snapshot: Ok(repo.snapshot().unwrap()),
                preview: false,
                conflict: None,
            })
            .unwrap();
        assert!(app.poll_worker());
        assert!(app.busy.is_none());
        assert_eq!(app.snapshot.files[app.selected].path, Path::new("b"));
        assert_eq!(repo.text(&["ls-files"]).unwrap(), "a");
    }

    #[test]
    fn destructive_command_policy_is_explicit() {
        use CommandId::*;
        for command in [
            Discard,
            Amend,
            BranchRename,
            BranchDelete,
            TakeCurrent,
            TakeIncoming,
            DeleteConflict,
            StashSave,
            StashPop,
            StashDrop,
            TagDelete,
            WorktreeRemove,
            Abort,
            Skip,
            Continue,
            Unstage,
            UnstageAll,
        ] {
            assert!(
                command.needs_confirmation(),
                "{:?} bypasses confirmation",
                command
            );
        }
    }

    #[test]
    fn empty_repository_navigation_and_palette_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::Down,
            KeyCode::Char(']'),
        ] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(app.selected, 0);
        assert_eq!(app.workspace, Workspace::History);
        assert!(app.palette("force push").is_empty());
        assert!(app.palette("fetch").contains(&CommandId::Fetch));
    }
}
