use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use similar::{ChangeTag, TextDiff};

use crate::{
    app::{App, Focus, Modal, Workspace},
    conflict::{Choice, Conflict},
    git::Hunks,
    rebase::{Plan, RebaseAction},
};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub fn minimum_size(app: &App) -> (u16, u16) {
    if matches!(
        app.modal,
        Some(Modal::Conflict(_) | Modal::Hunks(_) | Modal::Rebase(_) | Modal::Confirm { .. })
    ) {
        (60, 24)
    } else {
        (44, 12)
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let (minimum_width, minimum_height) = minimum_size(app);
    if area.width < minimum_width || area.height < minimum_height {
        frame.render_widget(
            Paragraph::new(format!("kae\n\nResize to at least {minimum_width} × {minimum_height}.\nEditing and confirmations are paused.\nEsc / q close; unsaved drafts are retained."))
                .block(Block::bordered().title(" Terminal too small ")),
            area,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    let repo = app
        .repo
        .root
        .file_name()
        .unwrap_or(app.repo.root.as_os_str())
        .to_string_lossy();
    let tracking = app
        .snapshot
        .branches
        .iter()
        .find(|branch| branch.current)
        .map(|branch| format!("{} {}", branch.upstream, branch.tracking))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" kae ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(format!("  {} / ", safe_text(&repo))),
            Span::styled(
                safe_text(&app.snapshot.branch),
                Style::default().fg(ACCENT).bold(),
            ),
            Span::styled(
                format!("   {}", safe_text(&tracking)),
                Style::default().fg(MUTED),
            ),
        ])),
        rows[0],
    );
    let tabs = Workspace::ALL
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            Line::from(if area.width < 100 {
                format!("{}{}", index + 1, &workspace.title()[..1])
            } else {
                format!("{} {}", index + 1, workspace.title())
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(tabs)
            .select(app.workspace.index())
            .divider(" ")
            .highlight_style(Style::default().fg(ACCENT).bold()),
        rows[1],
    );

    let panes = split(
        rows[2],
        if app.workspace == Workspace::History {
            50
        } else {
            32
        },
    );
    let items: Vec<ListItem> = app
        .items()
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let text = if app.workspace == Workspace::History {
                Text::from(
                    item.lines()
                        .map(|line| Line::raw(safe_text(line)))
                        .collect::<Vec<_>>(),
                )
            } else {
                Text::raw(safe_text(&item))
            };
            let color =
                if app.workspace == Workspace::Files && app.snapshot.files[index].is_conflict() {
                    Color::Red
                } else if app.workspace == Workspace::History {
                    Color::LightBlue
                } else {
                    Color::Reset
                };
            ListItem::new(text).style(Style::default().fg(color))
        })
        .collect();
    let title = format!(
        " {} · {} {} ",
        app.workspace.title(),
        items.len(),
        if app.workspace == Workspace::History {
            "commits (latest 300)"
        } else {
            "items"
        }
    );
    let list = List::new(items)
        .block(panel(title, app.focus == Focus::List))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).bold())
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(if app.items().is_empty() {
        None
    } else {
        Some(app.selected)
    });
    frame.render_stateful_widget(list, panes[0], &mut state);
    frame.render_widget(
        Paragraph::new(diff_text(&app.preview))
            .scroll((app.scroll, app.horizontal))
            .block(panel(
                " Diff / details · Tab focus · h/l horizontal ".into(),
                app.focus == Focus::Preview,
            )),
        panes[1],
    );

    if let Some(modal) = &app.modal {
        draw_modal(frame, app, modal, rows[2]);
    }

    let status = app
        .snapshot
        .operation
        .map(|operation| operation.label())
        .unwrap_or("Ready");
    let (message, color) = if let Some(error) = &app.error {
        (error.as_str(), Color::Red)
    } else if let Some(busy) = &app.busy {
        (busy.as_str(), Color::Yellow)
    } else {
        (app.notice.as_str(), MUTED)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    " {status} · {} conflicted files",
                    app.snapshot
                        .files
                        .iter()
                        .filter(|file| file.is_conflict())
                        .count()
                ),
                Style::default().fg(if app.snapshot.operation.is_some() {
                    Color::Yellow
                } else {
                    MUTED
                }),
            ),
            Line::styled(
                format!(
                    " {}{}",
                    if app.busy.is_some() {
                        "Running · "
                    } else {
                        ""
                    },
                    safe_text(message)
                ),
                Style::default().fg(color),
            ),
            Line::raw(if app.quit_requested {
                " Quit requested — waiting for Git to finish safely."
            } else if app.busy.is_some() {
                " Git is running · ? help · q / Ctrl-C quit after completion"
            } else {
                " : commands · ? help · Tab focus · r refresh · !/~ details · q quit"
            }),
        ]),
        rows[3],
    );
}

fn draw_modal(frame: &mut Frame, app: &App, modal: &Modal, area: Rect) {
    frame.render_widget(Clear, area);
    match modal {
        Modal::Help { scroll } => {
            frame.render_widget(Paragraph::new(
                "Navigate: 1–7 workspace · [/] previous/next · Tab list/preview\nj/k or arrows move · h/l scroll horizontally · PgUp/PgDn scroll\n\nFiles: s stage · u unstage · S/U all · v hunks · e external editor\nEnter on conflict: resolve · c commit · a amend\nBranches: Enter switch · b new branch\nHistory / branches / tags: m preview merge · M merge\nHistory: i interactive rebase (selected commit is the base)\nRemotes: f fetch · p pull fast-forward only · P push\nOperation: C continue · A abort · : for skip and integration review\n\n: or / opens searchable commands, including stash/tag/worktree actions.\nPrompts: type, Backspace, Ctrl-U clear, Enter accept, Esc cancel.\nConfirmations: y proceeds; n / Esc cancels.\nConflicts: 1 current · 2 incoming · 3 current+incoming · 4 reverse\nn/p region · u undo · b base · s save · a stage · e editor\nRebase: j/k select · J/K reorder · p/r/s/f/d/e actions · Enter review\n\nr refresh · ~ last output · q quit / close · Ctrl-C quit when idle\nNetwork auth must be configured externally; no automatic force pushes."
            ).wrap(Wrap { trim: false }).scroll((*scroll, 0)).block(panel(" Help · j/k scroll · Esc close ".into(), true)), area);
        }
        Modal::Palette { filter, selected } => {
            let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(area);
            frame.render_widget(
                Paragraph::new(safe_text(filter)).block(panel(
                    " Command search · type to filter · Esc cancel ".into(),
                    true,
                )),
                rows[0],
            );
            let commands = app.palette(filter);
            let list = List::new(
                commands
                    .iter()
                    .map(|command| ListItem::new(command.label()))
                    .collect::<Vec<_>>(),
            )
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(ACCENT).bold())
            .block(Block::bordered().title(" ↑/↓ select · Enter execute "));
            let mut state = ListState::default().with_selected(if commands.is_empty() {
                None
            } else {
                Some(*selected)
            });
            frame.render_stateful_widget(list, rows[1], &mut state);
        }
        Modal::Prompt { caption, input, .. } => {
            let text = format!(
                "\n{}\n\n{}▏\n\nEnter accept · Ctrl-U clear · Esc cancel",
                safe_text(caption),
                input.lines().map(safe_text).collect::<Vec<_>>().join("\n")
            );
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Input ".into(), true)),
                area,
            );
        }
        Modal::Confirm { detail, .. } => {
            frame.render_widget(
                Paragraph::new(format!(
                    "\n{}\n\n[y] Proceed    [n / Esc] Cancel",
                    detail.lines().map(safe_text).collect::<Vec<_>>().join("\n")
                ))
                .wrap(Wrap { trim: false })
                .block(
                    Block::bordered()
                        .title(" Review action · explicit confirmation required ")
                        .border_style(Style::default().fg(Color::Yellow)),
                ),
                area,
            );
        }
        Modal::Output {
            title,
            text,
            scroll,
            horizontal,
        } => {
            frame.render_widget(
                Paragraph::new(diff_text(text))
                    .scroll((*scroll, *horizontal))
                    .block(panel(
                        format!(
                            " {} · j/k scroll · h/l horizontal · Esc close ",
                            safe_text(title)
                        ),
                        true,
                    )),
                area,
            );
        }
        Modal::Conflict(conflict) => draw_conflict(frame, conflict, area),
        Modal::Hunks(hunks) => draw_hunks(frame, hunks, area),
        Modal::Rebase(plan) => draw_rebase(frame, plan, area),
    }
}

fn draw_conflict(frame: &mut Frame, conflict: &Conflict, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    let regions = conflict.regions();
    let region = regions[conflict.selected];
    let choice = match region.choice {
        None => "unresolved",
        Some(Choice::Current) => "current",
        Some(Choice::Incoming) => "incoming",
        Some(Choice::CurrentThenIncoming) => "current → incoming",
        Some(Choice::IncomingThenCurrent) => "incoming → current",
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {} · region {}/{} · {} unresolved · {} · {}",
            safe_text(&conflict.path.to_string_lossy()),
            conflict.selected + 1,
            regions.len(),
            conflict.unresolved(),
            choice,
            if conflict.dirty() {
                "UNSAVED"
            } else {
                "saved / unchanged"
            }
        ))
        .style(Style::default().fg(ACCENT).bold()),
        rows[0],
    );
    let wide = area.width >= 100;
    let source_height = if !wide && conflict.show_base { 70 } else { 50 };
    let body = Layout::vertical([
        Constraint::Percentage(source_height),
        Constraint::Percentage(100 - source_height),
    ])
    .split(rows[1]);
    let top = Layout::default()
        .direction(if wide {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints(if conflict.show_base {
            vec![Constraint::Ratio(1, 3); 3]
        } else {
            vec![Constraint::Ratio(1, 2); 2]
        })
        .split(body[0]);
    frame.render_widget(
        Paragraph::new(word_diff(&region.current, &region.incoming, true))
            .scroll((conflict.scroll, conflict.horizontal))
            .block(panel(
                format!(" {} ", safe_text(&conflict.current_label)),
                false,
            )),
        top[0],
    );
    let right = if conflict.show_base { 2 } else { 1 };
    if conflict.show_base {
        frame.render_widget(
            Paragraph::new(diff_text(
                region
                    .base
                    .as_deref()
                    .or(conflict.base_file.as_deref())
                    .unwrap_or("No common-ancestor blob (for example, an add/add conflict)."),
            ))
            .scroll((conflict.scroll, conflict.horizontal))
            .block(panel(
                if region.base.is_some() {
                    " Common ancestor · region "
                } else {
                    " Common ancestor · full file "
                }
                .into(),
                false,
            )),
            top[1],
        );
    }
    frame.render_widget(
        Paragraph::new(word_diff(&region.current, &region.incoming, false))
            .scroll((conflict.scroll, conflict.horizontal))
            .block(panel(
                format!(" {} ", safe_text(&conflict.incoming_label)),
                false,
            )),
        top[right],
    );
    frame.render_widget(
        Paragraph::new(diff_text(&conflict.result()))
            .scroll((
                conflict.result_offset().saturating_add(conflict.scroll),
                conflict.horizontal,
            ))
            .block(panel(
                " Result · complete file · e manual edit · s save (not stage) ".into(),
                true,
            )),
        body[1],
    );
    let footer = if conflict.confirm_close {
        "Discard UNSAVED in-memory choices? y discard / n keep editing. Saved file changes are retained."
    } else {
        "1 current · 2 incoming · 3 current+incoming · 4 incoming+current · u undo · n/p region\nb base · j/k scroll · h/l horizontal · s save · a stage · e editor · q close"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(if conflict.confirm_close {
            Color::Yellow
        } else {
            ACCENT
        })),
        rows[2],
    );
}

fn draw_hunks(frame: &mut Frame, hunks: &Hunks, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            " {} · {} · hunk {}/{}",
            safe_text(&hunks.path.to_string_lossy()),
            if hunks.cached {
                "STAGED → unstage"
            } else {
                "WORKING TREE → stage"
            },
            hunks.selected + 1,
            hunks.bodies.len()
        ))
        .style(Style::default().fg(ACCENT)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(diff_text(&String::from_utf8_lossy(
            &hunks.bodies[hunks.selected],
        )))
        .scroll((hunks.scroll, hunks.horizontal))
        .block(panel(" Selected hunk ".into(), true)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(
            "n/p hunk · j/k scroll · h/l horizontal · Tab staged/unstaged · s apply · Esc cancel",
        ),
        rows[2],
    );
}

fn draw_rebase(frame: &mut Frame, plan: &Plan, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            " Replay {} commits after {} · oldest first\n {}",
            plan.steps.len(),
            &plan.base[..plan.base.len().min(10)],
            if plan.published {
                "WARNING: includes commits reachable from remote-tracking refs"
            } else {
                "Local history rewrite · original HEAD will be backed up"
            }
        ))
        .style(Style::default().fg(if plan.published {
            Color::Yellow
        } else {
            ACCENT
        })),
        rows[0],
    );
    let items = plan
        .steps
        .iter()
        .map(|step| {
            let color = match step.action {
                RebaseAction::Drop => Color::Red,
                RebaseAction::Pick => Color::Reset,
                _ => Color::Yellow,
            };
            ListItem::new(format!(
                "{:<7} {} {}",
                step.action.name(),
                &step.oid[..step.oid.len().min(10)],
                safe_text(if step.action == RebaseAction::Reword {
                    &step.message
                } else {
                    &step.subject
                })
            ))
            .style(Style::default().fg(color))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(plan.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().bg(Color::DarkGray).bold())
            .block(panel(
                " Interactive rebase sequence · no changes until confirmed ".into(),
                true,
            )),
        rows[1],
        &mut state,
    );
    frame.render_widget(Paragraph::new("j/k select · J/K reorder · p pick · r reword · s squash · f fixup · d drop · e edit\nEnter review / start · Esc cancel · edit pauses for amend, then C continue"), rows[2]);
}

fn word_diff(left: &str, right: &str, current: bool) -> Text<'static> {
    let diff = TextDiff::configure()
        .timeout(std::time::Duration::from_millis(30))
        .diff_words(left, right);
    let mut lines = vec![Line::default()];
    for change in diff.iter_all_changes() {
        if (current && change.tag() == ChangeTag::Insert)
            || (!current && change.tag() == ChangeTag::Delete)
        {
            continue;
        }
        let style = if change.tag() == ChangeTag::Equal {
            Style::default()
        } else {
            Style::default()
                .fg(if current {
                    Color::LightRed
                } else {
                    Color::LightGreen
                })
                .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
        };
        for part in change.value().split_inclusive('\n') {
            let text = part
                .strip_suffix('\n')
                .unwrap_or(part)
                .trim_end_matches('\r');
            lines
                .last_mut()
                .expect("at least one line")
                .spans
                .push(Span::styled(safe_text(text), style));
            if part.ends_with('\n') {
                lines.push(Line::default());
            }
        }
    }
    Text::from(lines)
}

fn diff_text(text: &str) -> Text<'static> {
    Text::from(
        text.lines()
            .map(|line| {
                let color = if line.starts_with("+++") || line.starts_with("---") {
                    ACCENT
                } else if line.starts_with('+') {
                    Color::Green
                } else if line.starts_with('-')
                    || line.starts_with("<<<<<<<")
                    || line.starts_with(">>>>>>>")
                {
                    Color::Red
                } else if line.starts_with("@@") {
                    Color::Magenta
                } else {
                    Color::Reset
                };
                Line::styled(safe_text(line), Style::default().fg(color))
            })
            .collect::<Vec<_>>(),
    )
}

fn split(area: Rect, left: u16) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(if area.width >= 100 {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints([
            Constraint::Percentage(left),
            Constraint::Percentage(100 - left),
        ])
        .split(area)
}

fn panel(title: String, focused: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

pub fn safe_text(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character == '\t' {
                "    ".chars().collect::<Vec<_>>()
            } else if character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                character.escape_default().collect()
            } else {
                vec![character]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Repo;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders_all_workspaces_and_overlays_at_multiple_sizes() {
        let dir = tempfile::tempdir().unwrap();
        Repo {
            root: dir.path().into(),
        }
        .bytes(&["init", "-b", "main"])
        .unwrap();
        let mut app = App::new(dir.path()).unwrap();
        for (width, height) in [(140, 40), (60, 24), (20, 6), (44, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for workspace in Workspace::ALL {
                app.workspace = workspace;
                for modal in [
                    None,
                    Some(Modal::Help { scroll: 0 }),
                    Some(Modal::Palette {
                        filter: "".into(),
                        selected: 0,
                    }),
                    Some(Modal::Output {
                        title: "test".into(),
                        text: "diff\n+added".into(),
                        scroll: 0,
                        horizontal: 0,
                    }),
                ] {
                    app.modal = modal;
                    terminal.draw(|frame| draw(frame, &app)).unwrap();
                }
            }
        }
    }

    #[test]
    fn neutralizes_escape_and_directional_control_sequences() {
        assert_eq!(
            safe_text("hello\u{1b}[31m\nfile"),
            "hello\\u{1b}[31m\\nfile"
        );
        assert!(safe_text("\u{202e}").starts_with('\\'));
    }
}
