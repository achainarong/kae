use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::{
    conflict::{Choice, Conflict},
    editor,
    git::{Action, GitOutput, Hunks, Repo, Snapshot},
    rebase::{Plan, RebaseAction},
    ui,
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
}

impl CommandId {
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
            Self::Stage => "Stage selected file",
            Self::Unstage => "Unstage selected file",
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
            Stage | Unstage | Hunks | Discard | Edit => {
                app.workspace == Workspace::Files && !app.snapshot.files.is_empty()
            }
            Resolve | TakeCurrent | TakeIncoming | DeleteConflict => {
                app.workspace == Workspace::Files
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
}

pub enum PromptKind {
    Command(CommandId),
    Reword(Box<Plan>),
}

pub enum Modal {
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
    Hunks(Box<Hunks>),
    Rebase(Box<Plan>),
}

struct WorkerResult {
    label: String,
    output: std::result::Result<GitOutput, String>,
    snapshot: std::result::Result<Snapshot, String>,
    preview: bool,
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
    worker: Option<Receiver<WorkerResult>>,
    edit_path: Option<PathBuf>,
    quit: bool,
}

impl App {
    pub fn new(path: &Path) -> Result<Self> {
        let repo = Repo::discover(path)?;
        let snapshot = repo.snapshot()?;
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
            error: None,
            busy: None,
            quit_requested: false,
            last_output: "No operations yet.".into(),
            worker: None,
            edit_path: None,
            quit: false,
        };
        app.load_preview();
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
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
            if shutdown.load(Ordering::Relaxed) && !self.quit_requested {
                self.quit_requested = true;
                redraw = true;
            }
            if self.quit_requested && self.busy.is_none() {
                self.quit = true;
                continue;
            }
            if let Some(path) = self.edit_path.take() {
                ratatui::restore();
                let result = editor::open(&path);
                *terminal = ratatui::init();
                terminal.clear()?;
                if let Err(error) = result {
                    self.fail(error);
                }
                self.refresh();
                redraw = true;
            }
            if redraw {
                terminal.draw(|frame| ui::draw(frame, self))?;
                redraw = false;
            }
            if event::poll(Duration::from_millis(80))? {
                redraw = true;
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let size = terminal.size()?;
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
                    Event::Mouse(mouse) if self.modal.is_none() && self.busy.is_none() => {
                        match mouse.kind {
                            event::MouseEventKind::ScrollDown => self.navigate(3),
                            event::MouseEventKind::ScrollUp => self.navigate(-3),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
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
            } else if matches!(&self.modal, Some(Modal::Conflict(conflict)) if conflict.dirty()) {
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
            if key.code == KeyCode::Char('?') {
                self.modal = Some(Modal::Help { scroll: 0 });
            }
            if key.code == KeyCode::Char('q') {
                self.quit_requested = true;
            }
            return;
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
                    self.selected = 0;
                    self.load_preview();
                }
            }
            KeyCode::Esc => self.error = None,
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
                KeyCode::Char('y') => self.start(task),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    if let Task::Rebase(plan) = task {
                        self.modal = Some(Modal::Rebase(Box::new(plan)));
                    }
                }
                _ => self.modal = Some(Modal::Confirm { task, detail }),
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
                            conflict.selected = (conflict.selected + 1) % count;
                            conflict.scroll = 0;
                        }
                        KeyCode::Char('p') | KeyCode::Char('[') => {
                            conflict.selected = (conflict.selected + count - 1) % count;
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
                        KeyCode::Char('s') => match conflict.save(&self.repo) {
                            Ok(()) => {
                                self.notice = "Resolution saved to working tree — not staged. Press a to stage when complete.".into();
                            }
                            Err(error) => self.fail(error),
                        },
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
                            match Hunks::load(&self.repo, &self.snapshot, file, !hunks.cached) {
                                Ok(other) => hunks = Box::new(other),
                                Err(error) => self.fail(error),
                            }
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
            self.selected = self
                .selected
                .saturating_add_signed(delta)
                .min(self.items().len().saturating_sub(1));
            self.load_preview();
        }
    }

    fn load_preview(&mut self) {
        self.scroll = 0;
        self.horizontal = 0;
        self.preview = self
            .preview_text()
            .unwrap_or_else(|error| format!("Could not load preview:\n{error:#}"));
    }

    fn preview_text(&self) -> Result<String> {
        let empty = || "Nothing here yet. Use : to find available commands.".to_owned();
        Ok(match self.workspace {
            Workspace::Files => match self.snapshot.files.get(self.selected) { Some(file) => self.repo.file_preview(file)?, None => "Working tree is clean.\n\nPress 2 for history, b to create a branch, or : for commands.".into() },
            Workspace::History => match self.snapshot.commits.get(self.selected) { Some(commit) => format!("{} · {}\n\n{}", commit.author, commit.age, self.repo.show(&commit.oid)?), None => "No commits yet. Stage files and press c to create the first commit.".into() },
            Workspace::Branches => match self.snapshot.branches.get(self.selected) { Some(branch) => format!("Branch: {}\nUpstream: {} {}\n\n{}", branch.name, branch.upstream, branch.tracking, self.repo.show(&branch.oid)?), None => empty() },
            Workspace::Remotes => match self.snapshot.remotes.get(self.selected) { Some(remote) => format!("Remote: {remote}\nCurrent branch: {}\n\n{}\n\nf fetch · p pull fast-forward only · P push\n: for pull merge / rebase and upstream setup.\n\nNetwork authentication must already be configured.\nRemote URLs are omitted to avoid exposing credentials.", self.snapshot.branch, self.repo.text(&["status", "--short", "--branch"])?), None => "No remotes configured.\n\nAdd one using git remote add, then press r.\nRemote configuration editing is not part of this MVP.".into() },
            Workspace::Stashes => match self.snapshot.stashes.get(self.selected) { Some(stash) => self.repo.show(&stash.oid)?, None => empty() },
            Workspace::Tags => match self.snapshot.tags.get(self.selected) { Some(tag) => self.repo.show(&format!("refs/tags/{tag}"))?, None => empty() },
            Workspace::Worktrees => match self.snapshot.worktrees.get(self.selected) { Some(worktree) => format!("{}\nBranch: {}\nLocked: {}\n\nEnter to open.\n: to add a detached worktree or remove a clean one.\n\n{}", worktree.path.display(), worktree.branch, worktree.locked, Repo { root: worktree.path.clone() }.text(&["status", "--short", "--branch"] ).unwrap_or_else(|error| error.to_string())), None => empty() },
        })
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
        self.error = None;
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
        let new = |args: &[&str]| Action::new(&self.snapshot, command.label(), args);
        let mut confirm = false;
        let mut detail = command.label().to_owned();
        let action = match command {
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
                self.modal = Some(Modal::Conflict(Box::new(Conflict::load(
                    &self.repo,
                    file.path.clone(),
                    self.snapshot.guard.clone(),
                    &self.snapshot.branch,
                )?)));
                return Ok(());
            }
            Hunks => {
                let file = self
                    .snapshot
                    .files
                    .get(self.selected)
                    .context("Select a file")?;
                let cached = file.status.as_bytes()[1] == b' ';
                self.modal = Some(Modal::Hunks(Box::new(crate::git::Hunks::load(
                    &self.repo,
                    &self.snapshot,
                    file,
                    cached,
                )?)));
                return Ok(());
            }
            Rebase => {
                self.modal = Some(Modal::Rebase(Box::new(Plan::load(
                    &self.repo,
                    &self.snapshot,
                    &self.revision()?,
                )?)));
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
                self.output(
                    "Selected revision → HEAD",
                    self.repo.text(&[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        &revision,
                        "HEAD",
                        "--",
                    ])?,
                );
                return Ok(());
            }
            Review => {
                let text = if self.snapshot.head.is_some() {
                    self.repo
                        .text(&["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"])?
                } else {
                    self.repo
                        .text(&["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"])?
                };
                self.output(
                    "Integration result against HEAD · includes staged + unstaged changes",
                    if text.is_empty() {
                        "No changes against HEAD.".into()
                    } else {
                        text
                    },
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
                    Stage => new(&["add"]),
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
            StageAll => new(&["add", "--all"]),
            UnstageAll if self.snapshot.head.is_some() => new(&["restore", "--staged", "--", "."]),
            UnstageAll => new(&["rm", "--cached", "-r", "-f", "--", "."]),
            Commit | Amend => {
                let Some(message) = input else {
                    let initial = if command == Amend {
                        self.repo.text(&["log", "-1", "--format=%B"])?
                    } else {
                        String::new()
                    };
                    self.prompt(command, "Commit message", initial);
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
                        new(&["branch", "-m", "--", branch, &input]).idle()
                    }
                    StashSave => new(&["stash", "push", "--include-untracked", "-m", &input]),
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
                    Fetch => new(&["fetch", "--", &remote]),
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
                if command == Continue {
                    self.repo.ensure_resolved()?;
                }
                confirm = command != Continue;
                detail = if command == Abort {
                    format!(
                        "Abort {}?\nResolution progress and operation changes may be discarded as Git restores the pre-operation state. Copy any manual edits you want to retain first.",
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
        if confirm {
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
        let repo = self.repo.clone();
        let label = match &task {
            Task::Git(action) | Task::Preview(action) => action.label.clone(),
            Task::Hunk(_) => "Apply selected hunk".into(),
            Task::Rebase(_) => "Interactive rebase".into(),
        };
        self.busy = Some(label.clone());
        self.error = None;
        let (sender, receiver) = mpsc::channel();
        self.worker = Some(receiver);
        thread::spawn(move || {
            let preview = matches!(task, Task::Preview(_));
            let output = match task {
                Task::Git(action) => repo.execute(&action),
                Task::Preview(action) => repo.merge_preview(&action),
                Task::Hunk(hunks) => hunks.apply(&repo),
                Task::Rebase(plan) => plan.execute(&repo),
            }
            .map_err(|error| format!("{error:#}"));
            let snapshot = repo.snapshot().map_err(|error| format!("{error:#}"));
            let _ = sender.send(WorkerResult {
                label,
                output,
                snapshot,
                preview,
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
            Ok(snapshot) => self.snapshot = snapshot,
            Err(error) => self.error = Some(format!("Could not refresh repository: {error}")),
        }
        self.selected = self.selected.min(self.items().len().saturating_sub(1));
        self.load_preview();
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
        true
    }

    fn refresh(&mut self) {
        match self.repo.snapshot() {
            Ok(snapshot) => {
                self.snapshot = snapshot;
                self.selected = self.selected.min(self.items().len().saturating_sub(1));
                self.load_preview();
                self.notice = "Repository refreshed".into();
            }
            Err(error) => self.fail(error),
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
    fn empty_repository_navigation_and_palette_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        let mut app = App::new(dir.path()).unwrap();
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
