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
    history::{CellKind, DiffCell, DiffLayout, Search},
    rebase::{Plan, RebaseAction},
};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub fn minimum_size(app: &App) -> (u16, u16) {
    if matches!(
        app.modal,
        Some(
            Modal::Conflict(_)
                | Modal::Inline { .. }
                | Modal::Commit(_)
                | Modal::Hunks(_)
                | Modal::Rebase(_)
                | Modal::Confirm { .. }
        )
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

    if app.modal.is_none() {
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
                let color = if app.workspace == Workspace::Files
                    && app.snapshot.files[index].is_conflict()
                {
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
            Paragraph::new(diff_window(
                &app.preview,
                usize::from(app.scroll),
                panes[1].height.saturating_sub(2),
            ))
            .scroll((0, app.horizontal))
            .block(panel(
                " Diff / details · Enter browse commit · Tab focus ".into(),
                app.focus == Focus::Preview,
            )),
            panes[1],
        );
    }

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
    } else if app.loading() {
        ("Loading repository / preview…", MUTED)
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
            } else if matches!(app.modal, Some(Modal::Commit(_))) {
                " History browser · read only · Esc returns to history · [/] file · / find · d layout"
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
        Modal::Loading(label) => frame.render_widget(
            Paragraph::new(format!("\n{}\n\nEsc / q cancel loading", safe_text(label)))
                .block(panel(" Loading · read only ".into(), true)),
            area,
        ),
        Modal::Help { scroll } => {
            frame.render_widget(Paragraph::new(
                concat!(
                    "Navigate: 1–7 workspace · [/] workspace · Tab list/preview\n",
                    "j/k or arrows move · h/l horizontal · PgUp/PgDn scroll\n\n",
                    "Files: s/u stage/unstage · S/U all · v hunks · e external editor\n",
                    "Enter on conflict: resolve · c commit · a amend\n",
                    "Branches: Enter switch · b new branch\n",
                    "History: Enter file tree · Enter folder/file · h/l collapse/expand\n",
                    "Inside a commit: [/] file · {/} hunk · / text search · n/N match\n",
                    "d unified/split · t full file · Tab focus · P merge parent\n",
                    "History / branches / tags: m preview merge · M merge\n",
                    "History: i interactive rebase (selected commit is the base)\n",
                    "Remotes: f review fetch · p pull fast-forward only · P push\n",
                    "Operation: C review continue · A review abort · : skip / review\n\n",
                    ": or / opens command search in the main view.\n",
                    "Prompts: type · Backspace · Ctrl-U clear · Enter accept · Esc cancel\n",
                    "Confirmations: only typed y proceeds; n / Esc cancels.\n",
                    "Conflicts: 1 current · 2 incoming · 3 both · 4 reverse\n",
                    "n/p region · u undo · b base · s review save · a stage · e inline · E external\n",
                    "Inline: Ctrl-Z undo · Ctrl-Y redo · Ctrl-S review save\n",
                    "Esc keeps the inline draft without writing it.\n",
                    "Rebase: j/k select · J/K reorder · p/r/s/f/d/e actions · Enter review\n\n",
                    "r refresh (also automatic every 2s outside dialogs) · ~ output · q close/quit\n",
                    "Network auth must be configured externally; no automatic force pushes."
                )
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
            let block = Block::bordered()
                .title(" Review action · j/k scroll · confirmation required ")
                .border_style(Style::default().fg(Color::Yellow));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
            frame.render_widget(
                Paragraph::new(format!(
                    "\n{}",
                    detail.lines().map(safe_text).collect::<Vec<_>>().join("\n")
                ))
                .wrap(Wrap { trim: false })
                .scroll((app.confirm_scroll, 0)),
                rows[0],
            );
            frame.render_widget(Paragraph::new("[y] Proceed    [n / Esc] Cancel\nOnly typed y confirms; pasted text never executes."), rows[1]);
        }
        Modal::Output {
            title,
            text,
            scroll,
            horizontal,
        } => {
            frame.render_widget(
                Paragraph::new(diff_window(
                    text,
                    usize::from(*scroll),
                    area.height.saturating_sub(2),
                ))
                .scroll((0, *horizontal))
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
        Modal::Inline { editor, .. } => {
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(area);
            frame.render_widget(&editor.area, rows[0]);
            frame.render_widget(Paragraph::new("Type to edit · arrows / Home / End move · Ctrl-Z undo · Ctrl-Y redo\nCtrl-S review save (not stage) · Esc keep draft and return · q inserts text"), rows[1]);
        }
        Modal::Commit(view) => draw_commit(frame, view, area),
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
    let region = regions.get(conflict.selected).copied();
    let current = region
        .map(|region| region.current.as_str())
        .unwrap_or(&conflict.current_file);
    let incoming = region
        .map(|region| region.incoming.as_str())
        .unwrap_or(&conflict.incoming_file);
    let choice = match region.and_then(|region| region.choice) {
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
            if regions.is_empty() {
                0
            } else {
                conflict.selected + 1
            },
            regions.len(),
            conflict.unresolved(),
            if conflict.parse_error.is_some() {
                "repair malformed markers in editor"
            } else if regions.is_empty() {
                "manual result"
            } else {
                choice
            },
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
        Paragraph::new(word_diff(current, incoming, true))
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
                    .and_then(|region| region.base.as_deref())
                    .or(conflict.base_file.as_deref())
                    .unwrap_or("No common-ancestor blob (for example, an add/add conflict)."),
            ))
            .scroll((conflict.scroll, conflict.horizontal))
            .block(panel(
                if region.is_some_and(|region| region.base.is_some()) {
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
        Paragraph::new(word_diff(current, incoming, false))
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
                " Result · e inline edit · E external · s review save ".into(),
                true,
            )),
        body[1],
    );
    let footer = if conflict.confirm_close {
        "Discard UNSAVED in-memory choices? y discard / n keep editing. Saved file changes are retained."
    } else {
        "1 current · 2 incoming · 3 current+incoming · 4 reverse · u undo · n/p region\nb base · j/k scroll · s review save · a stage · e inline · E external · q close"
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

fn draw_commit(frame: &mut Frame, view: &crate::app::CommitView, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    let parent = if view.commit.parents.is_empty() {
        "root commit vs empty tree".into()
    } else {
        format!(
            "parent {}/{} · {}",
            view.commit.parent + 1,
            view.commit.parents.len(),
            &view.commit.parents[view.commit.parent][..10]
        )
    };
    let side_by_side = view.history.split(area.width, view.full);
    let layout = if view.history.loading {
        "loading"
    } else if view.full {
        "full file"
    } else if side_by_side {
        "side by side"
    } else if view.history.layout == DiffLayout::SideBySide {
        view.history
            .document
            .split_note
            .unwrap_or("unified · widen to 120 columns for split")
    } else {
        "unified"
    };
    let path = view
        .commit
        .files
        .get(view.selected)
        .map(|file| safe_text(&file.path.to_string_lossy()))
        .unwrap_or_default();
    let search = &view.history.search;
    let matches = if search.query.is_empty() {
        String::new()
    } else {
        format!(
            " · /{} {}/{}{}",
            safe_text(&search.query),
            search.active.map(|index| index + 1).unwrap_or(0),
            search.matches.len(),
            if search.matches.is_empty() && !view.history.loading {
                " (no matches)"
            } else {
                ""
            }
        )
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {} {}\n {} · {} · {}{}",
            &view.commit.oid[..10],
            safe_text(&view.commit.subject),
            parent,
            layout,
            path,
            matches
        ))
        .style(Style::default().fg(ACCENT)),
        rows[0],
    );
    let panes = split(rows[1], 32);
    let items = view
        .history
        .tree
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth.min(32));
            match row.file.and_then(|index| view.commit.files.get(index)) {
                Some(file) => ListItem::new(format!(
                    "{indent}{} {} {}{}",
                    if row.file == Some(view.selected) {
                        "●"
                    } else {
                        " "
                    },
                    file.status,
                    safe_text(&row.name.to_string_lossy()),
                    file.original
                        .as_ref()
                        .map(|path| format!(" ← {}", safe_text(&path.to_string_lossy())))
                        .unwrap_or_default()
                )),
                None => ListItem::new(format!(
                    "{indent}{} {}/ ({})",
                    if view.history.tree.collapsed.contains(&row.path) {
                        "▸"
                    } else {
                        "▾"
                    },
                    safe_text(&row.name.to_string_lossy()),
                    row.count
                ))
                .style(Style::default().fg(ACCENT)),
            }
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(if items.is_empty() {
        None
    } else {
        Some(view.history.tree.cursor)
    });
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(
                " Changed files · folders ".into(),
                view.focus == Focus::List,
            ))
            .highlight_symbol("› ")
            .highlight_style(Style::default().bg(Color::DarkGray).bold()),
        panes[0],
        &mut state,
    );
    if view.history.loading {
        frame.render_widget(
            Paragraph::new("Loading file…").block(panel(" File preview ".into(), true)),
            panes[1],
        );
    } else if side_by_side {
        let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(panes[1]);
        let start = view.history.document.row_for(view.scroll);
        for (left, column) in [(true, columns[0]), (false, columns[1])] {
            let lines = view
                .history
                .document
                .rows
                .iter()
                .skip(start)
                .take(usize::from(column.height.saturating_sub(2)))
                .map(|row| {
                    split_line(
                        if left {
                            row.left.as_ref()
                        } else {
                            row.right.as_ref()
                        },
                        search,
                        view.horizontal,
                        column.width.saturating_sub(2),
                    )
                })
                .collect::<Vec<_>>();
            let title = if left && view.commit.parents.is_empty() {
                " Old · empty tree "
            } else if left {
                " Old · parent "
            } else {
                " New · commit "
            };
            frame.render_widget(
                Paragraph::new(lines).block(panel(title.into(), view.focus == Focus::Preview)),
                column,
            );
        }
    } else {
        let lines = view
            .history
            .document
            .lines
            .iter()
            .enumerate()
            .skip(view.scroll)
            .take(usize::from(panes[1].height.saturating_sub(2)))
            .map(|(source, line)| {
                let prefix = if view.full {
                    Span::styled(format!("{:>5} ", source + 1), Style::default().fg(MUTED))
                } else {
                    Span::raw("")
                };
                let style = if view.full {
                    Style::default()
                } else {
                    patch_style(line)
                };
                presentation_line(
                    prefix,
                    highlighted(line, source, 0, search, style),
                    view.horizontal,
                    panes[1].width.saturating_sub(2),
                )
            })
            .collect::<Vec<_>>();
        let title = if !view.full {
            " File diff · unified "
        } else if view
            .commit
            .files
            .get(view.selected)
            .is_some_and(|file| file.status == "D")
        {
            " Full file · parent version (deleted) "
        } else {
            " Full file · committed version "
        };
        frame.render_widget(
            Paragraph::new(lines).block(panel(title.into(), view.focus == Focus::Preview)),
            panes[1],
        );
    }
    let footer = if let Some(input) = &search.editing {
        let input = format!("{}▏", safe_text(input));
        let offset = Span::raw(&input)
            .width()
            .saturating_sub(usize::from(area.width.saturating_sub(1)));
        vec![
            presentation_line(Span::raw("/"), vec![Span::raw(input)], offset, area.width),
            Line::raw("Enter find · Ctrl-U clear · Esc cancel · literal, case-insensitive"),
        ]
    } else if !search.query.is_empty() {
        vec![
            Line::raw(format!(
                "{} · /{} · n/N match · / edit",
                if search.matches.is_empty() && !view.history.loading {
                    "No matches".into()
                } else {
                    format!(
                        "Match {}/{}",
                        search.active.map(|index| index + 1).unwrap_or(0),
                        search.matches.len()
                    )
                },
                safe_text(&search.query)
            )),
            Line::raw("[/] file · d unified/split · t full · Tab focus · Esc back"),
        ]
    } else {
        vec![
            Line::raw("j/k move · h/l tree/scroll · Enter open · [/] file · Tab focus"),
            Line::raw("/ find · n/N match · {/} hunk · d split · t full · P parent · Esc back"),
        ]
    };
    frame.render_widget(Paragraph::new(footer), rows[2]);
}

fn patch_style(line: &str) -> Style {
    Style::default().fg(if line.starts_with("+++") || line.starts_with("---") {
        ACCENT
    } else if line.starts_with('+') {
        Color::Green
    } else if line.starts_with('-') {
        Color::Red
    } else if line.starts_with("@@") {
        Color::Magenta
    } else {
        Color::Reset
    })
}

fn match_style(active: bool) -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(if active {
            Color::LightMagenta
        } else {
            Color::Yellow
        })
        .bold()
}

fn highlighted(
    text: &str,
    source: usize,
    prefix: usize,
    search: &Search,
    base: Style,
) -> Vec<Span<'static>> {
    let matches = search.on_line(source);
    if matches.is_empty() {
        return vec![Span::styled(safe_text(text), base)];
    }
    let mut spans = Vec::new();
    let mut offset = prefix;
    let mut index = 0;
    let mut run = String::new();
    let mut run_style = base;
    let span = Span::raw(text);
    for grapheme in span.styled_graphemes(base) {
        let end = offset + grapheme.symbol.len();
        while index < matches.len() && matches[index].bytes.end <= offset {
            index += 1;
        }
        let matching = matches
            .get(index)
            .is_some_and(|matched| matched.bytes.start < end);
        let active = search.current().is_some_and(|matched| {
            matched.line == source && matched.bytes.start < end && matched.bytes.end > offset
        });
        let style = if matching {
            base.patch(match_style(active))
        } else {
            base
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push_str(&safe_text(grapheme.symbol));
        offset = end;
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

fn split_line(
    cell: Option<&DiffCell>,
    search: &Search,
    horizontal: usize,
    width: u16,
) -> Line<'static> {
    let Some(cell) = cell else {
        return Line::raw("");
    };
    let color = match cell.kind {
        CellKind::Delete => Color::Red,
        CellKind::Insert => Color::Green,
        CellKind::Hunk => Color::Magenta,
        CellKind::Meta => MUTED,
        CellKind::Context => Color::Reset,
    };
    let base = Style::default().fg(color);
    let sign = match cell.kind {
        CellKind::Delete => "-",
        CellKind::Insert => "+",
        _ => " ",
    };
    let prefix_style = search
        .on_line(cell.source)
        .iter()
        .find(|matched| cell.prefix > 0 && matched.bytes.start < cell.prefix)
        .map(|matched| match_style(search.current() == Some(matched)))
        .unwrap_or(Style::default().fg(MUTED));
    let prefix = Span::styled(
        format!(
            "{:>5} {sign} ",
            cell.number
                .map(|number| number.to_string())
                .unwrap_or_default()
        ),
        prefix_style,
    );
    let mut spans = highlighted(&cell.text, cell.source, cell.prefix, search, base);
    if let Some((source, annotation)) = &cell.annotation {
        spans.push(Span::raw("  "));
        spans.extend(highlighted(
            annotation,
            *source,
            0,
            search,
            Style::default().fg(MUTED),
        ));
    }
    presentation_line(prefix, spans, horizontal, width)
}

/// Crop by displayed graphemes, not bytes or u16 offsets. Gutters remain fixed
/// even when a search result is far into a long UTF-8 line.
fn presentation_line(
    prefix: Span<'static>,
    spans: Vec<Span<'static>>,
    offset: usize,
    width: u16,
) -> Line<'static> {
    let available = usize::from(width).saturating_sub(prefix.width());
    let mut visible = vec![prefix];
    let mut column = 0usize;
    let end = offset.saturating_add(available);
    'spans: for span in spans {
        for grapheme in span.styled_graphemes(Style::default()) {
            let size = Span::raw(grapheme.symbol).width();
            if column >= end {
                break 'spans;
            }
            let next = column.saturating_add(size);
            if next > offset {
                if column < offset || next > end {
                    visible.push(Span::styled(
                        " ".repeat(next.min(end).saturating_sub(column.max(offset))),
                        grapheme.style,
                    ));
                } else {
                    visible.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
                }
            }
            column = next;
        }
    }
    Line::from(visible)
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

fn diff_window(text: &str, start: usize, height: u16) -> Text<'static> {
    diff_text(
        &text
            .lines()
            .skip(start)
            .take(usize::from(height))
            .collect::<Vec<_>>()
            .join("\n"),
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

    #[test]
    fn search_highlights_whole_graphemes_and_long_lines_crop_without_u16_limits() {
        use crate::history::Document;
        let text = "e\u{301} 👩\u{200d}💻 雪";
        let document = Document::full(text);
        let mut search = Search {
            query: "👩".into(),
            ..Search::default()
        };
        search.reindex(&document, 0);
        let spans = highlighted(text, 0, 0, &search, Style::default());
        assert!(
            spans
                .iter()
                .any(|span| span.content.contains("👩\u{200d}💻")
                    && span.style.bg == Some(Color::LightMagenta))
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            text
        );
        let text = format!("{}NEEDLE 雪", "x".repeat(100_000));
        search.query = "needle".into();
        search.reindex(&Document::full(&text), 0);
        let line = presentation_line(
            Span::raw(" 1234 "),
            highlighted(&text, 0, 0, &search, Style::default()),
            100_000,
            32,
        );
        let shown = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(shown.starts_with(" 1234 NEEDLE"));
        assert!(line.width() <= 32);
    }
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
        let mut app = App::ready(dir.path()).unwrap();
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
