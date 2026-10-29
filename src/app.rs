use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
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
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::DefaultTerminal;

use crate::{
    config::{self, Config},
    conflict::{Choice, Conflict},
    editor,
    git::{Action, CommitFiles, GitOutput, Hunks, IndexRequest, Repo, Snapshot},
    history::{DiffLayout, Document, HistoricalPreview, History, Position, Preferences, Search},
    loader::{Loader, Preview, ReadData, ReadKind, Request},
    rebase::{Plan, RebaseAction},
    theme::Theme,
    tree, ui,
};

struct MouseCapture;

// Unsupported terminals ignore the enhancement; Ctrl-J remains a newline fallback.
struct KeyboardMode;

impl KeyboardMode {
    fn enable() -> Result<Self> {
        let mode = Self;
        mode.resume()?;
        Ok(mode)
    }

    fn resume(&self) -> Result<()> {
        crossterm::execute!(
            std::io::stdout(),
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        Ok(())
    }

    fn suspend(&self) -> Result<()> {
        crossterm::execute!(std::io::stdout(), event::PopKeyboardEnhancementFlags)?;
        Ok(())
    }
}

impl Drop for KeyboardMode {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    }
}

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
    ToggleTree,
}

impl CommandId {
    fn needs_confirmation(self) -> bool {
        use CommandId::*;
        // Exhaustive on purpose: a new command must make an explicit safety decision.
        match self {
            Amend | Discard | TakeCurrent | TakeIncoming | DeleteConflict | BranchRename
            | BranchDelete | Merge | CherryPick | Revert | Fetch | PullFF | PullMerge
            | PullRebase | Push | PushUpstream | StashSave | StashApply | StashPop | StashDrop
            | TagDelete | WorktreeRemove | Continue | Skip | Abort => true,
            // Rebase execution and saves have contextual confirmation steps.
            Stage | Unstage | StageAll | UnstageAll | Hunks | Commit | Edit | Resolve
            | BranchCreate | BranchSwitch | Compare | MergePreview | Rebase | TagCreate
            | WorktreeAdd | WorktreeOpen | Review | Log | Theme | ToggleTree => false,
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
            Self::ToggleTree => "Toggle file tree",
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
            ToggleTree => app.workspace == Workspace::Files,
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
    pub filter_editing: bool,
    pub show_tree: bool,
    pub list_state: RefCell<ratatui::widgets::ListState>,
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
        let mut focus = if commit.files.len() == 1 {
            Focus::Preview
        } else {
            Focus::List
        };
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
            filter_editing: false,
            show_tree: commit.files.len() != 1,
            list_state: RefCell::new(ratatui::widgets::ListState::default()),
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

    fn edit_filter(&mut self, key: KeyEvent) -> bool {
        let mut query = self.history.tree.filter.clone();
        match key.code {
            KeyCode::Esc => {
                query.clear();
                self.filter_editing = false;
            }
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Backspace => {
                query.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => query.clear(),
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !character.is_control()
                    && query.len() + character.len_utf8() <= 1024 =>
            {
                query.push(character)
            }
            _ => return false,
        }
        self.history.tree.set_filter(&query);
        self.history
            .tree
            .selected()
            .and_then(|row| row.file)
            .is_some_and(|index| self.select(index))
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
    CommitMessage {
        command: CommandId,
        editor: Box<editor::ResultEditor>,
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

struct IndexResult {
    output: std::result::Result<GitOutput, String>,
}

pub struct App {
    pub manual_list_scroll: Cell<bool>,
    index_queue: VecDeque<IndexRequest>,
    index_worker: Option<Receiver<IndexResult>>,
    index_session: bool,
    index_scopes: Vec<PathBuf>,
    index_status_read: Option<u64>,
    guard_stale: bool,
    guard_read: Option<u64>,
    after_guard: Option<(CommandId, Option<String>)>,
    pub remote_tree: crate::history::FileTree,
    pub filters: [ListFilter; 7],
    pub preview_search: Search,
    pub geometry: RefCell<ui::PaneGeometry>,
    pub tab_hits: RefCell<Vec<(ratatui::layout::Rect, Workspace)>>,
    pub list_state: RefCell<ratatui::widgets::ListState>,
    pub file_tree_override: Option<bool>,
    pub pending_staging: HashMap<PathBuf, bool>,
    pub folder_status: HashMap<PathBuf, (usize, usize)>,
    last_input: Instant,
    refresh_round: u8,
    force_preview_refresh: bool,
    preview_valid: bool,
    dragging: bool,
    layout_save: Option<Instant>,
    pub history_graph: bool,
    pub history_all: bool,
    pub mouse_position: Option<(u16, u16)>,
    pub hover_started: Option<Instant>,
    pub show_path: bool,
    pub explorer_state: RefCell<ratatui::widgets::ListState>,
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
    history_preferences: Option<(Preferences, bool)>,
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

#[derive(Clone, Default)]
pub struct ListFilter {
    pub query: String,
    pub editing: bool,
}

impl App {
    fn index_command(command: CommandId) -> bool {
        matches!(
            command,
            CommandId::Stage | CommandId::Unstage | CommandId::StageAll | CommandId::UnstageAll
        )
    }

    fn enqueue_index(&mut self, command: CommandId) -> Result<()> {
        ensure!(self.loaded, "Wait for repository loading to finish");
        ensure!(
            self.busy.is_none() || self.index_session,
            "Wait for the current Git operation"
        );
        ensure!(command.available(self), "Select a file or folder first");
        let staged = matches!(command, CommandId::Stage | CommandId::StageAll);
        let all = matches!(command, CommandId::StageAll | CommandId::UnstageAll);
        let (path, bulk) = if all {
            (PathBuf::from("."), true)
        } else {
            let row = self
                .explorer
                .selected()
                .context("Select a file or folder")?;
            (
                if row.path.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    row.path.clone()
                },
                row.file.is_none(),
            )
        };
        let mut request = IndexRequest {
            staged,
            bulk,
            scopes: vec![path],
            rename_sources: Vec::new(),
        };
        let files = self
            .snapshot
            .files
            .iter()
            .filter(|file| request.covers(&file.path))
            .collect::<Vec<_>>();
        ensure!(
            !staged
                || !bulk
                || !files.iter().any(|file| file.is_conflict()
                    && self.pending_staging.get(&file.path) != Some(&true)),
            "Resolve and stage conflicted files individually before staging this folder or root"
        );
        let status_originals = files
            .iter()
            .filter_map(|file| file.original.clone())
            .collect::<Vec<_>>();
        request.merge(IndexRequest {
            staged,
            bulk,
            scopes: if staged {
                Vec::new()
            } else {
                status_originals.clone()
            },
            rename_sources: if staged {
                status_originals.clone()
            } else {
                Vec::new()
            },
        });
        request.scopes.sort();
        request.scopes.dedup();
        self.cancel_reads();
        self.guard_stale = true;
        self.index_session = true;
        self.error = None;
        self.busy = Some("Updating index".into());
        for file in &self.snapshot.files {
            if request.covers(&file.path)
                || file
                    .original
                    .as_ref()
                    .is_some_and(|path| request.covers(path))
            {
                self.pending_staging.insert(file.path.clone(), staged);
            }
        }
        self.folder_status = tree::folder_status(&self.snapshot.files, &self.pending_staging);
        let mut dirty = IndexRequest {
            staged,
            bulk,
            scopes: std::mem::take(&mut self.index_scopes),
            rename_sources: Vec::new(),
        };
        dirty.merge(request.clone());
        dirty.merge(IndexRequest {
            staged,
            bulk,
            scopes: status_originals,
            rename_sources: Vec::new(),
        });
        self.index_scopes = dirty.scopes;
        if let Some(last) = self.index_queue.back_mut() {
            if last.scopes == request.scopes && last.bulk == request.bulk {
                *last = request;
                return Ok(());
            }
            if last.staged == staged && last.bulk == bulk {
                last.merge(request);
                return Ok(());
            }
        }
        self.index_queue.push_back(request);
        Ok(())
    }

    fn poll_index(&mut self) -> bool {
        if !self.index_session {
            return false;
        }
        let mut changed = false;
        if let Some(receiver) = &self.index_worker {
            let result = match receiver.try_recv() {
                Ok(result) => result.output,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => Err("Index worker stopped unexpectedly".into()),
            };
            self.index_worker = None;
            changed = true;
            let failure = match result {
                Ok(output) => {
                    self.last_output = format!("Index update\n\n{}", output.text());
                    (!output.success).then(|| self.last_output.clone())
                }
                Err(error) => {
                    self.last_output = error.clone();
                    Some(error)
                }
            };
            if let Some(error) = failure {
                self.index_queue.clear();
                self.error = Some(format!(
                    "{error}\nQueued staging changes were cancelled; refreshing actual status."
                ));
            }
        }
        if self.index_worker.is_none() {
            if let Some(mut request) = self.index_queue.pop_front() {
                while self
                    .index_queue
                    .front()
                    .is_some_and(|next| next.staged == request.staged && next.bulk == request.bulk)
                {
                    request.merge(self.index_queue.pop_front().expect("queued index request"));
                }
                let repo = self.repo.clone();
                let ui = thread::current();
                let (sender, receiver) = mpsc::channel();
                self.index_worker = Some(receiver);
                thread::spawn(move || {
                    let output = repo
                        .update_index(&request)
                        .map_err(|error| format!("{error:#}"));
                    let _ = sender.send(IndexResult { output });
                    ui.unpark();
                });
                return true;
            }
            if self.index_status_read.is_none() {
                self.index_status_read =
                    Some(self.read(ReadKind::IndexStatus(self.index_scopes.clone())));
                return true;
            }
        }
        changed
    }

    fn reconcile_index(&mut self, scopes: Vec<PathBuf>, files: Vec<crate::git::FileChange>) {
        let request = IndexRequest {
            staged: false,
            bulk: false,
            scopes,
            rename_sources: Vec::new(),
        };
        let previous = self.explorer.clone();
        let selected_path = self
            .snapshot
            .files
            .get(self.selected)
            .map(|file| file.path.clone());
        self.snapshot.files.retain(|file| {
            !request.covers(&file.path)
                && !file
                    .original
                    .as_ref()
                    .is_some_and(|path| request.covers(path))
        });
        self.snapshot.files.extend(files);
        self.snapshot
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.snapshot
            .files
            .dedup_by(|left, right| left.path == right.path);
        self.pending_staging.clear();
        self.folder_status = tree::folder_status(&self.snapshot.files, &self.pending_staging);
        self.explorer = tree::explorer(&self.snapshot.files, &self.repo.root);
        self.explorer.restore_view(&previous);
        if self.workspace == Workspace::Files {
            self.selected = self
                .explorer
                .selected()
                .and_then(|row| row.file)
                .or_else(|| {
                    selected_path.and_then(|path| {
                        self.snapshot
                            .files
                            .iter()
                            .position(|file| file.path == path)
                    })
                })
                .unwrap_or(0);
            if !self.files_tree_visible() && self.snapshot.files.len() == 1 {
                self.explorer.select_file(0);
                self.selected = 0;
                self.focus = Focus::Preview;
            }
        }
        self.last_refresh = Instant::now();
        if self.error.is_none() {
            self.notice = "Index updated".into();
        }
        self.request_preview(false);
    }

    pub fn tooltip_visible(&self) -> bool {
        self.show_path
            || (self.hovered_row().is_some()
                && self
                    .hover_started
                    .is_some_and(|started| started.elapsed() >= Duration::from_millis(300)))
    }
    fn hovered_row(&self) -> Option<usize> {
        if self.modal.is_some() || self.workspace != Workspace::Files || !self.files_tree_visible()
        {
            return None;
        }
        let (x, y) = self.mouse_position?;
        let geometry = self.geometry.borrow();
        let inner = geometry.list.inner(ratatui::layout::Margin::new(1, 1));
        if !inner.contains(ratatui::layout::Position::new(x, y)) {
            return None;
        }
        geometry.rows.get(usize::from(y - inner.y)).copied()
    }

    fn mouse_event(&mut self, mouse: MouseEvent) -> bool {
        let previous = (self.tooltip_visible(), self.show_path, self.hovered_row());
        self.handle_mouse(mouse);
        let current = (self.tooltip_visible(), self.show_path, self.hovered_row());
        let changed = mouse.kind != MouseEventKind::Moved
            || previous.0 != current.0
            || (previous.0 && previous != current);
        if changed {
            self.last_input = Instant::now();
        }
        changed
    }
    pub fn visible_items(&self) -> Vec<(usize, String)> {
        let query = self.filters[self.workspace.index()].query.to_lowercase();
        self.items()
            .into_iter()
            .enumerate()
            .filter(|(_, item)| {
                matches!(self.workspace, Workspace::Files | Workspace::Remotes)
                    || item.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn apply_filter(&mut self) {
        let query = self.filters[self.workspace.index()].query.clone();
        match self.workspace {
            Workspace::Files => {
                self.explorer.set_filter(&query);
                if let Some(index) = self.explorer.selected().and_then(|row| row.file) {
                    self.selected = index;
                }
            }
            Workspace::Remotes => {
                self.remote_tree.set_filter(&query);
                self.selected = self.remote_tree.cursor;
            }
            _ => {
                let indices = self
                    .visible_items()
                    .into_iter()
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                if !indices.contains(&self.selected) {
                    self.selected = indices.first().copied().unwrap_or_else(|| {
                        if self.items().is_empty() {
                            0
                        } else {
                            usize::MAX
                        }
                    });
                }
            }
        }
        self.load_preview();
    }

    fn edit_filter(&mut self, key: KeyEvent) {
        let filter = &mut self.filters[self.workspace.index()];
        match key.code {
            KeyCode::Esc => {
                filter.query.clear();
                filter.editing = false;
            }
            KeyCode::Enter => filter.editing = false,
            KeyCode::Backspace => {
                filter.query.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                filter.query.clear()
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !character.is_control()
                    && filter.query.len() + character.len_utf8() <= 1024 =>
            {
                filter.query.push(character)
            }
            _ => return,
        }
        self.apply_filter();
    }

    fn edit_preview_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.preview_search.editing = None;
                self.preview_search.query.clear();
                self.preview_search
                    .reindex(&Document::full(&self.preview), usize::from(self.scroll));
            }
            KeyCode::Enter => {
                self.preview_search
                    .accept(&Document::full(&self.preview), usize::from(self.scroll));
                self.jump_preview_match();
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut self.preview_search.editing {
                    input.pop();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = &mut self.preview_search.editing {
                    input.clear();
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && !character.is_control() =>
            {
                if let Some(input) = &mut self.preview_search.editing
                    && input.len() + character.len_utf8() <= 1024
                {
                    input.push(character);
                }
            }
            _ => {}
        }
    }

    fn jump_preview_match(&mut self) {
        if let Some(matched) = self.preview_search.current() {
            self.scroll = matched.line.min(usize::from(u16::MAX)) as u16;
            if let Some(line) = self.preview.lines().nth(matched.line) {
                self.horizontal =
                    ratatui::text::Span::raw(ui::safe_text(&line[..matched.bytes.start]))
                        .width()
                        .saturating_sub(8)
                        .min(usize::from(u16::MAX)) as u16;
            }
        }
    }

    pub fn selected_remote_branch(&self) -> Option<&crate::git::RemoteBranch> {
        self.remote_tree
            .selected()
            .and_then(|row| row.file)
            .and_then(|index| self.snapshot.remote_branches.get(index))
    }

    fn selected_remote(&self) -> Option<&String> {
        if let Some(branch) = self.selected_remote_branch() {
            return self
                .snapshot
                .remotes
                .iter()
                .find(|remote| **remote == branch.remote);
        }
        let row = self.remote_tree.selected()?;
        self.snapshot
            .remotes
            .iter()
            .filter(|remote| row.path.starts_with(Path::new(remote)))
            .max_by_key(|remote| remote.len())
    }

    pub fn files_tree_visible(&self) -> bool {
        !self.filters[Workspace::Files.index()].query.is_empty()
            || self.snapshot.files.is_empty()
            || self
                .file_tree_override
                .unwrap_or(self.snapshot.files.len() != 1)
    }

    fn toggle_file_tree(&mut self) {
        self.file_tree_override = Some(!self.files_tree_visible());
        if !self.files_tree_visible() {
            self.explorer.select_file(self.selected);
            self.focus = Focus::Preview;
            self.load_preview();
        } else {
            self.focus = Focus::List;
        }
    }

    pub fn pane_percent(&self, kind: Option<ui::PaneKind>, horizontal: bool) -> u16 {
        let panes = &self.config.panes;
        match (kind, horizontal) {
            (Some(ui::PaneKind::Files), true) => panes.files_width,
            (Some(ui::PaneKind::History), true) => panes.history_width,
            (Some(ui::PaneKind::Commit), true) => panes.commit_width,
            (Some(ui::PaneKind::Files), false) => panes.files_height,
            (Some(ui::PaneKind::History), false) => panes.history_height,
            (Some(ui::PaneKind::Commit), false) => panes.commit_height,
            _ => 32,
        }
        .clamp(15, 85)
    }

    fn resize_pane(&mut self, kind: Option<ui::PaneKind>, horizontal: bool, percent: u16) {
        let panes = &mut self.config.panes;
        let value = match (kind, horizontal) {
            (Some(ui::PaneKind::Files), true) => &mut panes.files_width,
            (Some(ui::PaneKind::History), true) => &mut panes.history_width,
            (Some(ui::PaneKind::Commit), true) => &mut panes.commit_width,
            (Some(ui::PaneKind::Files), false) => &mut panes.files_height,
            (Some(ui::PaneKind::History), false) => &mut panes.history_height,
            (Some(ui::PaneKind::Commit), false) => &mut panes.commit_height,
            _ => return,
        };
        *value = percent.clamp(15, 85);
        self.mouse_position = None;
        self.layout_save = Some(Instant::now());
    }

    fn save_layout(&mut self) {
        self.layout_save = None;
        if let Some(root) = &self.config_root {
            if let Err(error) = Config::save_panes(root, &self.config.panes) {
                self.error = Some(format!(
                    "Pane sizes apply this session, but could not be saved: {error:#}"
                ));
            }
        } else {
            self.notice = "Pane sizes apply this session; HOME/XDG_CONFIG_HOME is unset.".into();
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.modal.is_some() && !matches!(self.modal, Some(Modal::Commit(_))) {
            return;
        }
        let geometry = self.geometry.borrow().clone();
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        if self.modal.is_none() && mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let workspace = self
                .tab_hits
                .borrow()
                .iter()
                .find(|(area, _)| area.contains(position))
                .map(|(_, workspace)| *workspace);
            if let Some(workspace) = workspace {
                self.switch(workspace);
                return;
            }
        }
        match mouse.kind {
            MouseEventKind::Up(MouseButton::Left) => {
                if self.dragging {
                    self.dragging = false;
                    self.save_layout();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                let horizontal = geometry.area.width >= 100;
                let (coordinate, start, total) = if horizontal {
                    (mouse.column, geometry.area.x, geometry.area.width)
                } else {
                    (mouse.row, geometry.area.y, geometry.area.height)
                };
                if total > 0 {
                    self.resize_pane(
                        geometry.kind,
                        horizontal,
                        (u32::from(coordinate.saturating_sub(start)) * 100 / u32::from(total))
                            .min(100) as u16,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if geometry.kind.is_some() && geometry.divider.contains(position) =>
            {
                self.dragging = true;
                self.mouse_position = None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.manual_list_scroll.set(false);
                self.mouse_position = None;
                self.filters[self.workspace.index()].editing = false;
                let inner = geometry.list.inner(ratatui::layout::Margin::new(1, 1));
                let row = if inner.contains(position) {
                    geometry.rows.get(usize::from(mouse.row - inner.y)).copied()
                } else {
                    None
                };
                if let Some(Modal::Commit(view)) = &mut self.modal {
                    view.filter_editing = false;
                    if geometry.preview.contains(position) {
                        view.focus = Focus::Preview;
                    }
                    if let Some(row) = row {
                        view.focus = Focus::List;
                        view.history.tree.cursor = row;
                        if let Some(index) = view.history.tree.selected().and_then(|row| row.file) {
                            if view.select(index) {
                                self.load_commit_preview();
                            }
                        } else if geometry.disclosures.contains(&(mouse.column, mouse.row)) {
                            view.history.tree.toggle();
                        }
                    }
                } else if let Some(row) = row {
                    self.focus = Focus::List;
                    if self.workspace == Workspace::Files {
                        self.explorer.cursor = row;
                        if let Some(index) = self.explorer.selected().and_then(|row| row.file) {
                            self.selected = index;
                        } else if geometry.disclosures.contains(&(mouse.column, mouse.row)) {
                            self.explorer.toggle();
                        }
                    } else if self.workspace == Workspace::Remotes {
                        self.remote_tree.cursor = row;
                        self.selected = row;
                        if geometry.disclosures.contains(&(mouse.column, mouse.row)) {
                            self.remote_tree.toggle();
                            self.selected = self.remote_tree.cursor;
                        }
                    } else {
                        self.selected = row;
                    }
                    self.load_preview();
                } else if geometry.preview.contains(position) {
                    self.focus = Focus::Preview;
                }
            }
            MouseEventKind::Moved if !self.dragging => {
                let previous = self.hovered_row();
                self.show_path = false;
                self.mouse_position = Some((mouse.column, mouse.row));
                let current = self.hovered_row();
                if previous != current {
                    self.hover_started = current.map(|_| Instant::now());
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                if !geometry.list.contains(position) && !geometry.preview.contains(position) {
                    return;
                }
                let down = mouse.kind == MouseEventKind::ScrollDown;
                if geometry.list.contains(position)
                    && (matches!(self.modal, Some(Modal::Commit(_)))
                        || (self.modal.is_none() && self.workspace == Workspace::Files))
                {
                    let height = usize::from(geometry.list.height.saturating_sub(2));
                    let (state, count) = if let Some(Modal::Commit(view)) = &self.modal {
                        (&view.list_state, view.history.tree.rows.len())
                    } else {
                        (&self.explorer_state, self.explorer.rows.len())
                    };
                    let mut state = state.borrow_mut();
                    *state.offset_mut() = state
                        .offset()
                        .saturating_add_signed(if down { 3 } else { -3 })
                        .min(count.saturating_sub(height.max(1)));
                    self.manual_list_scroll.set(true);
                    self.mouse_position = None;
                    self.show_path = false;
                    return;
                }
                let code = if down { KeyCode::Down } else { KeyCode::Up };
                if let Some(Modal::Commit(view)) = &mut self.modal {
                    view.focus = if geometry.preview.contains(position) {
                        Focus::Preview
                    } else {
                        Focus::List
                    };
                    let manual_scroll = self.manual_list_scroll.get();
                    for _ in 0..3 {
                        self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
                    }
                    self.manual_list_scroll.set(manual_scroll);
                } else {
                    self.focus = if geometry.preview.contains(position) {
                        Focus::Preview
                    } else {
                        Focus::List
                    };
                    self.navigate(if down { 3 } else { -3 });
                }
            }
            _ => {}
        }
    }

    fn optimistic_staging(&mut self, task: &Task) {
        self.pending_staging.clear();
        if let Task::Hunk(hunks) = task {
            self.pending_staging
                .insert(hunks.path.clone(), !hunks.cached);
            return;
        }
        let Task::Git(action) = task else {
            return;
        };
        if !action.updates_only_index() {
            return;
        }
        let staged = action.args.first().is_some_and(|arg| arg == "add");
        let paths = action
            .args
            .iter()
            .position(|arg| arg == "--")
            .map(|index| &action.args[index + 1..]);
        for file in &self.snapshot.files {
            if paths.is_none_or(|paths| {
                paths.iter().any(|path| {
                    path == "."
                        || Path::new(path) == file.path
                        || file.original.as_deref() == Some(Path::new(path))
                })
            }) {
                self.pending_staging.insert(file.path.clone(), staged);
            }
        }
    }

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
            manual_list_scroll: Cell::new(false),
            index_queue: VecDeque::new(),
            index_worker: None,
            index_session: false,
            index_scopes: Vec::new(),
            index_status_read: None,
            guard_stale: false,
            guard_read: None,
            after_guard: None,
            remote_tree: tree::remotes(&snapshot),
            filters: std::array::from_fn(|_| ListFilter::default()),
            preview_search: Search::default(),
            geometry: RefCell::new(ui::PaneGeometry::default()),
            tab_hits: RefCell::new(Vec::new()),
            list_state: RefCell::new(ratatui::widgets::ListState::default()),
            file_tree_override: None,
            pending_staging: HashMap::new(),
            folder_status: HashMap::new(),
            last_input: Instant::now(),
            refresh_round: 0,
            force_preview_refresh: false,
            preview_valid: false,
            dragging: false,
            layout_save: None,
            history_graph: false,
            history_all: false,
            mouse_position: None,
            hover_started: None,
            show_path: false,
            explorer_state: RefCell::new(ratatui::widgets::ListState::default()),
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
        crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;
        let _mouse = MouseCapture;
        let paste = editor::PasteMode::enable()?;
        let keyboard = KeyboardMode::enable()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
        }
        let mut redraw = true;
        let mut tooltip_visible = false;
        while !self.quit {
            redraw |= self.poll_worker();
            redraw |= self.poll_loading();
            let visible = self.tooltip_visible();
            if visible != tooltip_visible {
                redraw = true;
                tooltip_visible = visible;
            }
            if self
                .layout_save
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(350))
                && !self.dragging
            {
                self.save_layout();
                redraw = true;
            }
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
                crossterm::execute!(std::io::stdout(), event::DisableMouseCapture)?;
                paste.suspend()?;
                keyboard.suspend()?;
                ratatui::restore();
                let result = editor::open(&path);
                *terminal = ratatui::init();
                paste.resume()?;
                keyboard.resume()?;
                crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;
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
            if event::poll(Duration::ZERO)? {
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
                        self.dragging = false;
                        self.mouse_position = None;
                        self.update_width(width);
                    }
                    Event::Mouse(mouse) => redraw = self.mouse_event(mouse),
                    _ => {}
                }
            } else {
                thread::park_timeout(Duration::from_millis(8));
            }
        }
        if self.layout_save.is_some() {
            self.save_layout();
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
                    if !self.history_graph {
                        return format!(
                            "{} {} · {} · {}",
                            commit.subject, commit.refs, commit.short, commit.age
                        );
                    }
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
            Workspace::Remotes => self
                .remote_tree
                .rows
                .iter()
                .map(|row| {
                    format!(
                        "{}{} {}{}",
                        "  ".repeat(row.depth),
                        if row.file.is_some() {
                            " "
                        } else if self.remote_tree.collapsed.contains(&row.path) {
                            "▸"
                        } else {
                            "▾"
                        },
                        row.name.to_string_lossy(),
                        if row.file.is_none() {
                            format!("/ ({})", row.count)
                        } else {
                            String::new()
                        }
                    )
                })
                .collect(),
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
            .chain([CommandId::ToggleTree])
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
        self.manual_list_scroll.set(false);
        self.last_input = Instant::now();
        if self.dragging {
            self.dragging = false;
            self.save_layout();
        }
        self.mouse_position = None;
        if key.code != KeyCode::Char('w') {
            self.show_path = false;
        }
        if matches!(self.modal, Some(Modal::CommitMessage { .. })) {
            if let Some(modal) = self.modal.take() {
                self.handle_modal(modal, key);
            }
            return;
        }
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
        if self.modal.is_none() && self.filters[self.workspace.index()].editing {
            self.edit_filter(key);
            return;
        }
        if self.modal.is_none() && self.preview_search.editing.is_some() {
            self.edit_preview_search(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
            )
        {
            let horizontal = matches!(key.code, KeyCode::Left | KeyCode::Right);
            let geometry = self.geometry.borrow().clone();
            if (self.modal.is_none() || matches!(self.modal, Some(Modal::Commit(_))))
                && geometry.kind.is_some()
                && geometry.divider.width > 0
                && horizontal == (geometry.area.width >= 100)
            {
                let amount = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
                    -2
                } else {
                    2
                };
                self.resize_pane(
                    geometry.kind,
                    horizontal,
                    self.pane_percent(geometry.kind, horizontal)
                        .saturating_add_signed(amount),
                );
                return;
            }
        }
        if key.code == KeyCode::Char('T') {
            if let Some(Modal::Commit(view)) = &mut self.modal
                && view.history.search.editing.is_none()
                && !view.filter_editing
            {
                view.show_tree = !view.show_tree;
                view.focus = if view.show_tree {
                    Focus::List
                } else {
                    Focus::Preview
                };
                return;
            } else if self.modal.is_none() && self.workspace == Workspace::Files {
                self.toggle_file_tree();
                return;
            }
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
            let index_input = self.index_session
                && matches!(key.code, KeyCode::Char(' ' | 's' | 'u' | 'S' | 'U'));
            if !index_input
                && !matches!(
                    key.code,
                    KeyCode::Char('?')
                        | KeyCode::Char('1'..='7')
                        | KeyCode::Char('[' | ']' | 'j' | 'k' | 'h' | 'l')
                        | KeyCode::Char('/' | 'n' | 'N')
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
                )
            {
                self.notice =
                    "Git is running; navigation is available, further mutations are locked.".into();
                return;
            }
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('!') => self.output("Last error / operation", self.last_output.clone()),
            KeyCode::Char(':') => {
                self.modal = Some(Modal::Palette {
                    filter: String::new(),
                    selected: 0,
                })
            }
            KeyCode::Char('/') => {
                if self.focus == Focus::Preview {
                    self.preview_search.begin();
                } else {
                    self.filters[self.workspace.index()].editing = true;
                }
            }
            KeyCode::Char('n' | 'N')
                if self.focus == Focus::Preview && !self.preview_search.query.is_empty() =>
            {
                self.preview_search.next(key.code == KeyCode::Char('N'));
                self.jump_preview_match();
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char(']') => self.switch(self.workspace.next(false)),
            KeyCode::Char('[') => self.switch(self.workspace.next(true)),
            KeyCode::Char(number @ '1'..='7') => {
                self.switch(Workspace::ALL[number as usize - '1' as usize])
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if (self.workspace == Workspace::Files && !self.files_tree_visible())
                    || self.focus == Focus::List
                {
                    Focus::Preview
                } else {
                    Focus::List
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::Left | KeyCode::Char('h')
                if self.workspace == Workspace::Remotes && self.focus == Focus::List =>
            {
                self.remote_tree.collapse();
                self.selected = self.remote_tree.cursor;
                self.load_preview();
            }
            KeyCode::Right | KeyCode::Char('l')
                if self.workspace == Workspace::Remotes && self.focus == Focus::List =>
            {
                self.remote_tree.expand();
                self.selected = self.remote_tree.cursor;
                self.load_preview();
            }
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
                    } else if self.workspace == Workspace::Remotes {
                        self.remote_tree.navigate(isize::MIN);
                        self.selected = self.remote_tree.cursor;
                    } else {
                        self.selected = self
                            .visible_items()
                            .first()
                            .map(|(index, _)| *index)
                            .unwrap_or_else(|| {
                                if self.items().is_empty() {
                                    0
                                } else {
                                    usize::MAX
                                }
                            });
                    }
                    self.load_preview();
                }
            }
            KeyCode::Char('w') if self.workspace == Workspace::Files => {
                self.show_path = !self.show_path;
            }
            KeyCode::Char('g') if self.workspace == Workspace::History => {
                self.history_graph = !self.history_graph;
            }
            KeyCode::Char('B') if self.workspace == Workspace::History && self.busy.is_none() => {
                self.history_all = !self.history_all;
                self.snapshot_read = Some(self.read(ReadKind::Snapshot {
                    all_history: self.history_all,
                }));
            }
            KeyCode::Esc => {
                self.error = None;
                if !self.filters[self.workspace.index()].query.is_empty() {
                    self.filters[self.workspace.index()].query.clear();
                    self.apply_filter();
                }
                self.preview_search = Search::default();
            }
            KeyCode::Char(' ')
                if self.workspace == Workspace::Files
                    && (self.focus == Focus::List || !self.files_tree_visible()) =>
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
                Workspace::Remotes if self.focus == Focus::List => {
                    self.remote_tree.toggle();
                    self.selected = self.remote_tree.cursor;
                    self.load_preview();
                }
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
                    self.guard_read = None;
                    self.after_guard = None;
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
            Modal::CommitMessage {
                command,
                mut editor,
            } => {
                match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        editor.area.insert_newline();
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        editor.area.insert_newline();
                    }
                    KeyCode::Enter => {
                        let message = editor.text();
                        if message.trim().is_empty() {
                            self.error = Some("Please enter a commit message.".into());
                        } else {
                            self.error = None;
                            if let Err(error) = self.prepare(command, Some(message)) {
                                self.fail(error);
                            }
                            return;
                        }
                    }
                    _ => {
                        editor.area.input(key);
                    }
                }
                self.modal = Some(Modal::CommitMessage { command, editor });
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
                if view.filter_editing {
                    let reload = view.edit_filter(key);
                    self.modal = Some(Modal::Commit(view));
                    if reload {
                        self.load_commit_preview();
                    }
                    return;
                }
                if view.history.search.editing.is_some() {
                    view.edit_search(key, self.screen_width);
                    self.modal = Some(Modal::Commit(view));
                    return;
                }
                if key.code == KeyCode::Esc && !view.history.tree.filter.is_empty() {
                    view.history.tree.set_filter("");
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
                        view.focus = if !view.show_tree || view.focus == Focus::List {
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
                    KeyCode::Char('/') => {
                        if view.focus == Focus::List && view.show_tree {
                            view.filter_editing = true;
                        } else {
                            view.history.search.begin();
                        }
                    }
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
                        self.history_preferences = Some((view.preferences(), view.show_tree));
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
                        self.start(Task::Hunk(*hunks));
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
        self.filters[self.workspace.index()].editing = false;
        self.workspace = workspace;
        self.selected = 0;
        if workspace == Workspace::Files {
            self.explorer.select_file(0);
        }
        self.focus = if workspace == Workspace::Files && !self.files_tree_visible() {
            Focus::Preview
        } else {
            Focus::List
        };
        self.preview_search = Search::default();
        if workspace == Workspace::Remotes {
            self.selected = self.remote_tree.cursor;
        }
        self.apply_filter();
    }

    fn navigate(&mut self, delta: isize) {
        self.mouse_position = None;
        self.show_path = false;
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
            if self.workspace == Workspace::Remotes {
                self.remote_tree.navigate(delta);
                self.selected = self.remote_tree.cursor;
            } else {
                let indices = self
                    .visible_items()
                    .into_iter()
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                let position = indices
                    .iter()
                    .position(|index| *index == self.selected)
                    .unwrap_or(0);
                self.selected = indices
                    .get(
                        position
                            .saturating_add_signed(delta)
                            .min(indices.len().saturating_sub(1)),
                    )
                    .copied()
                    .unwrap_or_else(|| {
                        if self.items().is_empty() {
                            0
                        } else {
                            usize::MAX
                        }
                    });
            }
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
        if files.iter().any(|file| {
            self.pending_staging
                .get(&file.path)
                .map(|staged| !*staged)
                .unwrap_or(file.status.as_bytes()[1] != b' ')
        }) {
            self.invoke(CommandId::Stage);
        } else if files.iter().any(|file| {
            self.pending_staging
                .get(&file.path)
                .copied()
                .unwrap_or(!matches!(file.status.as_bytes()[0], b' ' | b'?'))
        }) {
            self.invoke(CommandId::Unstage);
        } else {
            self.notice = "No changes to stage or unstage in this selection.".into();
        }
    }

    fn preview_target(&self) -> Preview {
        if self.workspace == Workspace::Files && self.explorer.rows.is_empty() {
            return Preview::Empty("No files match the current filter. Esc clears it.".into());
        }
        if self.workspace == Workspace::Files
            && !self.file_selected()
            && let Some(row) = self.explorer.selected()
        {
            if self.snapshot.files.is_empty() {
                return Preview::Empty(
                    "Working tree is clean.\n\nPress 2 for history or : for commands.".into(),
                );
            }
            return Preview::Folder {
                path: row.path.clone(),
                heading: if row.path.as_os_str().is_empty() {
                    self.repo.root.display().to_string()
                } else {
                    row.path.display().to_string()
                },
                files: self
                    .snapshot
                    .files
                    .iter()
                    .filter(|file| file.path.starts_with(&row.path))
                    .cloned()
                    .collect(),
            };
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
            Workspace::Remotes => {
                if let Some(branch) = self.selected_remote_branch() {
                    Preview::RemoteBranch {
                        branch: branch.clone(),
                        head: self.snapshot.head.clone(),
                    }
                } else if let Some(remote) = self.selected_remote() {
                    let path = &self.remote_tree.selected().expect("remote selection").path;
                    Preview::Remote {
                        name: remote.clone(),
                        branch: self.snapshot.branch.clone(),
                        folder: path.display().to_string(),
                        branches: self
                            .snapshot
                            .remote_branches
                            .iter()
                            .filter(|branch| {
                                branch.remote == *remote
                                    && Path::new(&format!("{}/{}", branch.remote, branch.branch))
                                        .starts_with(path)
                            })
                            .map(|branch| format!("{}/{}", branch.remote, branch.branch))
                            .collect(),
                        tracking: self
                            .snapshot
                            .branches
                            .iter()
                            .filter(|branch| branch.upstream.starts_with(&format!("{remote}/")))
                            .map(|branch| {
                                format!("{} → {} {}", branch.name, branch.upstream, branch.tracking)
                            })
                            .collect(),
                    }
                } else {
                    Preview::Empty("No matching remotes. Configure a remote with git remote add, or Esc to clear the filter.".into())
                }
            }
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
                .remote_tree
                .rows
                .get(self.selected)
                .map(|row| row.path.as_os_str().to_owned()),
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
            self.preview_valid = false;
            self.preview_search
                .reindex(&Document::full(&self.preview), 0);
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
            self.selected_remote()
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
        if Self::index_command(command) {
            if let Err(error) = self.enqueue_index(command) {
                self.fail(error);
            }
            return;
        }
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
        if matches!(command, CommandId::Commit | CommandId::Amend) {
            match editor::ResultEditor::new(&input) {
                Ok(mut editor) => {
                    editor.set_theme(&self.theme);
                    editor.area.set_block(ratatui::widgets::Block::default());
                    self.modal = Some(Modal::CommitMessage {
                        command,
                        editor: Box::new(editor),
                    });
                }
                Err(error) => self.fail(error),
            }
            return;
        }
        self.modal = Some(Modal::Prompt {
            kind: PromptKind::Command(command),
            caption: caption.into(),
            input,
        });
    }

    fn prepare(&mut self, command: CommandId, input: Option<String>) -> Result<()> {
        use CommandId::*;
        if Self::index_command(command) {
            return self.enqueue_index(command);
        }
        let simple = matches!(command, Theme | ToggleTree | Log | Edit);
        let prompt = input.is_none()
            && matches!(
                command,
                Commit | Amend | BranchCreate | BranchRename | StashSave | TagCreate | WorktreeAdd
            );
        if self.guard_stale && !simple && !prompt {
            ensure!(!self.index_session, "Wait for staged changes to finish");
            self.cancel_reads();
            self.after_guard = Some((command, input));
            self.guard_read = Some(self.read(ReadKind::Snapshot {
                all_history: self.history_all,
            }));
            self.modal = Some(Modal::Loading(
                "Validating repository before reviewed action…".into(),
            ));
            return Ok(());
        }
        if command == ToggleTree {
            self.toggle_file_tree();
            return Ok(());
        }
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
            Theme | ToggleTree | Stage | Unstage | StageAll | UnstageAll => unreachable!(),
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
            Discard | TakeCurrent | TakeIncoming | DeleteConflict => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                match command {
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
                }.path(&file.path)
            }
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
                let selected_branch = (self.workspace == Workspace::Remotes)
                    .then(|| self.selected_remote_branch())
                    .flatten()
                    .map(|branch| branch.branch.clone());
                detail = format!(
                    "{}\nRemote: {remote}\nBranch: {}\nNo force-push will be used.",
                    command.label(),
                    self.snapshot.branch
                );
                let mut action = match command {
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
                        let destination = if let Some(branch) = &selected_branch {
                            branch.as_str()
                        } else if command == Push {
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
                };
                if matches!(command, PullFF | PullMerge | PullRebase) {
                    let upstream = self
                        .snapshot
                        .branches
                        .iter()
                        .find(|branch| branch.current)
                        .map(|branch| branch.upstream.as_str())
                        .unwrap_or("");
                    let branch = selected_branch
                        .as_deref()
                        .or_else(|| upstream.strip_prefix(&format!("{remote}/")))
                        .unwrap_or(&self.snapshot.branch);
                    ensure!(
                        !self.snapshot.branch.starts_with("detached @"),
                        "Switch to a local branch before pulling"
                    );
                    action.args.push(branch.into());
                    detail.push_str(&format!("\nSource remote branch: {remote}/{branch}"));
                }
                action
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
        self.optimistic_staging(&task);
        self.folder_status = tree::folder_status(&self.snapshot.files, &self.pending_staging);
        self.cancel_reads();
        let repo = self.repo.clone();
        let all_history = self.history_all;
        let index_snapshot = (matches!(&task, Task::Git(action) if action.updates_only_index())
            && self.snapshot.all_history == all_history)
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
        let ui = thread::current();
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
                None => repo.snapshot_scoped(all_history),
            }
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(WorkerResult {
                label,
                output,
                snapshot,
                preview,
                conflict: saved_conflict,
            });
            ui.unpark();
        });
    }

    pub fn poll_worker(&mut self) -> bool {
        let index_changed = self.poll_index();
        if self.worker.is_none() {
            return index_changed;
        }
        let result = match self.worker.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => {
                self.pending_staging.clear();
                self.folder_status =
                    tree::folder_status(&self.snapshot.files, &self.pending_staging);
                self.busy = None;
                self.worker = None;
                self.error = Some("Git worker stopped unexpectedly. Refresh and inspect repository state before retrying.".into());
                return true;
            }
            _ => return index_changed,
        };
        self.busy = None;
        self.worker = None;
        self.pending_staging.clear();
        self.folder_status = tree::folder_status(&self.snapshot.files, &self.pending_staging);
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
            self.force_preview_refresh = true;
            self.snapshot_read = Some(self.read(ReadKind::Snapshot {
                all_history: self.history_all,
            }));
        }
    }

    pub fn auto_refresh(&mut self, now: Instant) {
        if self.busy.is_none()
            && self.modal.is_none()
            && (self.loaded || self.error.is_none())
            && now.saturating_duration_since(self.last_refresh) >= Duration::from_secs(2)
            && now.saturating_duration_since(self.last_input) >= Duration::from_millis(350)
            && self.snapshot_read.is_none()
        {
            self.refresh_round = (self.refresh_round + 1) % 5;
            if self.guard_stale
                || self.refresh_round == 0
                || self.workspace == Workspace::Worktrees
                || !self.loaded
            {
                self.snapshot_read = Some(self.read(ReadKind::Snapshot {
                    all_history: self.history_all,
                }));
            } else {
                self.snapshot_read =
                    Some(self.read(ReadKind::Refresh(Box::new(self.snapshot.clone()))));
            }
        }
    }

    pub fn loading(&self) -> bool {
        self.index_status_read.is_some()
            || self.guard_read.is_some()
            || self.snapshot_read.is_some()
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
        self.index_status_read = None;
        self.guard_read = None;
        self.after_guard = None;
        self.force_preview_refresh = false;
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
        self.guard_stale = false;
        let force_preview = std::mem::take(&mut self.force_preview_refresh);
        let same_state = self.loaded
            && self.repo.root == repo.root
            && self.snapshot.guard == snapshot.guard
            && self.snapshot.all_history == snapshot.all_history;
        if !force_preview
            && (self.preview_valid || self.preview_loading)
            && same_state
            && self.workspace == Workspace::Files
        {
            self.snapshot = snapshot;
            self.last_refresh = Instant::now();
            return;
        }
        let previous_target = same_state.then(|| self.preview_target());
        self.mouse_position = None;
        self.show_path = false;
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
        let previous_explorer = self.explorer.clone();
        let previous_remotes = self.remote_tree.clone();
        self.repo = repo;
        self.snapshot = snapshot;
        self.folder_status = tree::folder_status(&self.snapshot.files, &self.pending_staging);
        self.remote_tree = tree::remotes(&self.snapshot);
        self.remote_tree.restore_view(&previous_remotes);
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
        if !previous_explorer.filter.is_empty() {
            self.explorer.restore_view(&previous_explorer);
        }
        if self.workspace == Workspace::Files
            && let Some(index) = self.explorer.selected().and_then(|row| row.file)
        {
            self.selected = index;
        }
        if self.workspace == Workspace::Files
            && !self.files_tree_visible()
            && self.snapshot.files.len() == 1
        {
            self.explorer.select_file(0);
            self.selected = 0;
            self.focus = Focus::Preview;
        }
        if self.workspace == Workspace::Remotes {
            self.selected = self.remote_tree.cursor;
        }
        if !matches!(self.workspace, Workspace::Files | Workspace::Remotes)
            && !self.filters[self.workspace.index()].query.is_empty()
        {
            let visible = self.visible_items();
            if !visible.iter().any(|(index, _)| *index == self.selected) {
                self.selected = visible
                    .first()
                    .map(|(index, _)| *index)
                    .unwrap_or(usize::MAX);
            }
        }
        self.last_refresh = Instant::now();
        let keep_preview = !force_preview
            && same_state
            && self.workspace != Workspace::Worktrees
            && (self.preview_valid || self.preview_loading)
            && previous_target.as_ref() == Some(&self.preview_target());
        if !keep_preview {
            self.request_preview(retained.is_none());
        }
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
            if self.guard_read == Some(response.id) {
                self.guard_read = None;
                let pending = self.after_guard.take();
                if !matches!(self.modal, Some(Modal::Loading(_))) {
                    continue;
                }
                self.modal = None;
                match response.result {
                    Ok(ReadData::Snapshot(data)) => {
                        let old = &self.snapshot.guard;
                        let new = &data.1.guard;
                        let metadata_changed = old.metadata_changed(new);
                        let selected = self.selected_key();
                        let (repo, snapshot) = *data;
                        self.apply_snapshot(repo, snapshot);
                        if metadata_changed
                            || (selected.is_some() && selected != self.selected_key())
                        {
                            self.error = Some("Repository metadata or selection changed. Review the refreshed view and retry the action.".into());
                        } else if let Some((command, input)) = pending
                            && let Err(error) = self.prepare(command, input)
                        {
                            self.fail(error);
                        }
                    }
                    Err(error) => {
                        self.error = Some(format!("Could not validate repository: {error}"))
                    }
                    _ => {
                        self.error =
                            Some("Unexpected validation response; refresh before retrying.".into())
                    }
                }
                changed = true;
            } else if self.index_status_read == Some(response.id) {
                self.index_status_read = None;
                self.index_session = false;
                self.index_scopes.clear();
                self.busy = None;
                self.pending_staging.clear();
                self.folder_status =
                    tree::folder_status(&self.snapshot.files, &self.pending_staging);
                match response.result {
                    Ok(ReadData::IndexStatus { scopes, files }) => {
                        self.reconcile_index(scopes, files)
                    }
                    Err(error) => {
                        self.pending_staging.clear();
                        self.folder_status =
                            tree::folder_status(&self.snapshot.files, &self.pending_staging);
                        self.error = Some(format!(
                            "Could not refresh index status: {error}. Press r to reconcile."
                        ));
                    }
                    _ => {
                        self.error =
                            Some("Unexpected index status response; press r to reconcile.".into())
                    }
                }
                changed = true;
            } else if self.snapshot_read == Some(response.id) {
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
                self.preview_valid = matches!(&response.result, Ok(ReadData::Preview(_)));
                self.preview = match response.result {
                    Ok(ReadData::Preview(text)) => text,
                    Err(error) => format!("Could not load preview:\n{error}"),
                    _ => String::new(),
                };
                self.preview_search
                    .reindex(&Document::full(&self.preview), usize::from(self.scroll));
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
                        let (preferences, tree) = self
                            .history_preferences
                            .take()
                            .map(|(preferences, tree)| (Some(preferences), Some(tree)))
                            .unwrap_or_default();
                        let mut view = CommitView::new(commit, preferences);
                        if let Some(tree) = tree {
                            view.show_tree = tree;
                            if !tree {
                                view.focus = Focus::Preview;
                            }
                        }
                        self.modal = Some(Modal::Commit(Box::new(view)));
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
        self.last_input = Instant::now();
        if self.modal.is_none() && self.filters[self.workspace.index()].editing {
            let filter = &mut self.filters[self.workspace.index()];
            if filter.query.len() + text.len() <= 1024 {
                filter
                    .query
                    .extend(text.chars().filter(|character| !character.is_control()));
                self.apply_filter();
            }
            return;
        }
        if self.modal.is_none() && self.preview_search.editing.is_some() {
            self.preview_search.paste(text);
            return;
        }
        if let Some(Modal::Commit(view)) = &mut self.modal
            && view.filter_editing
        {
            let mut query = view.history.tree.filter.clone();
            if query.len() + text.len() <= 1024 {
                query.extend(text.chars().filter(|character| !character.is_control()));
                view.history.tree.set_filter(&query);
                if let Some(index) = view.history.tree.selected().and_then(|row| row.file)
                    && view.select(index)
                {
                    self.load_commit_preview();
                }
            }
            return;
        }
        match &mut self.modal {
            Some(Modal::Commit(view)) => view.history.search.paste(text),
            Some(Modal::Inline { editor, .. } | Modal::CommitMessage { editor, .. }) => {
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
    fn rapid_index_inputs_coalesce_and_last_toggle_wins() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        for _ in 0..3 {
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }
        assert_eq!(app.index_queue.len(), 1);
        assert!(app.index_queue[0].staged);
        assert!(app.pending_staging[Path::new("a")]);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.index_queue.len(), 1);
        assert_eq!(
            app.index_queue[0].scopes.len(),
            2,
            "adjacent stage inputs use one Git batch"
        );
        app.index_queue.clear();
        app.enqueue_index(CommandId::StageAll).unwrap();
        assert_eq!(
            app.index_queue[0].scopes,
            [PathBuf::from(".")],
            "root staging must not enumerate every child"
        );
        assert!(app.guard_stale);
        assert!(
            app.guard_read.is_none(),
            "staging must not start a full validation scan"
        );
    }

    #[test]
    fn cancelling_lazy_validation_never_executes_the_waiting_destructive_action() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        std::fs::write(dir.path().join("a"), "keep me").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        app.guard_stale = true;
        app.prepare(CommandId::Discard, None).unwrap();
        assert!(app.guard_read.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.guard_read.is_none() && app.after_guard.is_none());
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            app.poll_loading();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(app.modal.is_none() && app.busy.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn mouse_motion_within_one_hover_target_does_not_request_redraws() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let moved = |x, y| MouseEvent {
            kind: MouseEventKind::Moved,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(!app.mouse_event(moved(5, 6)));
        let started = app.hover_started;
        assert!(!app.mouse_event(moved(6, 6)));
        assert_eq!(
            app.hover_started, started,
            "motion within a row must not restart the delay"
        );
        app.hover_started = Some(Instant::now() - Duration::from_millis(250));
        assert!(!app.tooltip_visible());
        app.hover_started = Some(Instant::now() - Duration::from_millis(350));
        assert!(app.tooltip_visible());
        assert!(!app.mouse_event(moved(7, 6)));
        assert!(app.mouse_event(moved(5, 7)));
        assert!(
            !app.tooltip_visible(),
            "a different row starts a fresh 300 ms delay"
        );
        assert!(!app.mouse_event(moved(139, 20)));
        assert!(!app.mouse_event(moved(138, 20)));
        app.show_path = true;
        assert!(
            app.tooltip_visible(),
            "keyboard path reveal stays immediate"
        );
    }

    #[test]
    fn unchanged_idle_refresh_reuses_preview_but_manual_refresh_reloads_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        let original = app.preview.clone();
        let previous_read = app.next_read;
        let snapshot = app.snapshot.clone();
        app.apply_snapshot(app.repo.clone(), snapshot);
        assert_eq!(app.next_read, previous_read);
        assert_eq!(app.preview, original);
        app.force_preview_refresh = true;
        app.apply_snapshot(app.repo.clone(), app.snapshot.clone());
        assert!(app.next_read > previous_read);
        app.last_refresh = Instant::now() - Duration::from_secs(3);
        app.last_input = Instant::now();
        app.auto_refresh(Instant::now());
        assert!(
            app.snapshot_read.is_none(),
            "background refresh must wait for idle input"
        );
    }

    #[test]
    fn unchanged_refresh_does_not_restart_an_inflight_non_file_preview() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        let mut app = App::ready(dir.path()).unwrap();
        app.workspace = Workspace::Remotes;
        app.preview_loading = true;
        app.preview_valid = false;
        let generation = app.next_read;
        app.apply_snapshot(app.repo.clone(), app.snapshot.clone());
        assert_eq!(
            app.next_read, generation,
            "idle snapshots must not repeatedly cancel a slow preview"
        );
        assert!(app.preview_loading);
    }

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
        ] {
            assert!(
                command.needs_confirmation(),
                "{:?} bypasses confirmation",
                command
            );
        }
        for command in [Stage, Unstage, StageAll, UnstageAll] {
            assert!(
                !command.needs_confirmation(),
                "{command:?} must be immediate"
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
