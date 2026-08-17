//! Workspace-pane rendering: the environment checklist (`Zephyr Base`,
//! then `Projects Base`) with a broken location's reason right under its
//! row --- informational, never navigable --- a horizontal separator,
//! then the embedded project file list filling the rest of the pane
//! (`crate::ui::files`' row/list grammar, reused so it looks like
//! MicroPython's own local pane). Selected rows highlight full-width.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::ui::{Palette, dashboard_focused, pane_block};
use crate::workspace::{WorkspaceAction, WorkspacePanel};

/// Draws the full second row for a workspace+build backend: the workspace
/// pane on the left, the project panel (already its own module) on the right.
pub fn draw_row(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    draw(frame, left, app, palette);
    super::build::draw(frame, right, app, palette);
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    if app.workspace.is_none() {
        return;
    }
    let focused = dashboard_focused(app, Focus::Workspace);
    let block = pane_block("Workspace", focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    draw_rows(frame, inner, app, palette);
}

fn draw_rows(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(panel) = &app.workspace else {
        return;
    };
    let caps = app.manager.capabilities();
    let actions = panel.actions(&caps);
    let mut y = area.y;
    for (position, action) in actions.iter().enumerate() {
        // Frozen at wherever the checklist cursor was left while the user is
        // down in the file list --- it must not also draw as selected then.
        let selected = !panel.in_files && panel.cursor == position;
        match action {
            WorkspaceAction::Choose => {
                y = render_row(
                    frame,
                    area,
                    y,
                    checklist_row(
                        panel.resolved.is_some(),
                        panel.invalid.is_some(),
                        "Zephyr Base",
                        answer_value(
                            panel
                                .resolved
                                .as_ref()
                                .map(|workspace| workspace.dir.display().to_string()),
                            panel.invalid.is_some(),
                            area.width,
                            palette,
                        ),
                        palette,
                    ),
                    selected,
                );
                // A broken location's reason sits directly under its row,
                // wrapped so the install guide link survives. Not part of
                // the navigation --- the cursor never lands here.
                if let Some(message) = &panel.invalid {
                    y = render_info(frame, area, y, message.clone().fg(palette.error), 4);
                }
            }
            WorkspaceAction::Projects => {
                y = render_row(
                    frame,
                    area,
                    y,
                    checklist_row(
                        panel.projects.is_some(),
                        panel.projects_invalid.is_some(),
                        "Projects Base",
                        answer_value(
                            panel.projects.as_ref().map(|dir| dir.display().to_string()),
                            panel.projects_invalid.is_some(),
                            area.width,
                            palette,
                        ),
                        palette,
                    ),
                    selected,
                );
            }
            WorkspaceAction::Project => {
                // The build panel owns the answer; the question is asked
                // here, beside the other prerequisites.
                let project_ok = app.project_gate_ok();
                y = render_row(
                    frame,
                    area,
                    y,
                    checklist_row(
                        project_ok,
                        false,
                        "Project path",
                        answer_value(
                            project_ok.then(|| {
                                format!(
                                    "{} · {}",
                                    app.build.as_ref().unwrap().root.display(),
                                    app.build.as_ref().unwrap().project_origin.label()
                                )
                            }),
                            false,
                            area.width,
                            palette,
                        ),
                        palette,
                    ),
                    selected,
                );
            }
            WorkspaceAction::Board => {
                // The board name is the row's identity: the origin suffix
                // rides along only when the whole thing fits, never at the
                // name's expense.
                let budget = value_budget(area.width);
                let value = app.build.as_ref().and_then(|panel| {
                    panel.board.as_ref().map(|choice| {
                        let origin = match choice.origin {
                            crate::build::BoardOrigin::Picked => "picked",
                            crate::build::BoardOrigin::Config => "saved",
                            crate::build::BoardOrigin::Cache => "from build/",
                        };
                        let name = choice.name.clone();
                        let suffix = format!(" · {origin}");
                        if name.chars().count() + suffix.chars().count() <= budget {
                            format!("{name}{suffix}")
                        } else {
                            name
                        }
                    })
                });
                y = render_row(
                    frame,
                    area,
                    y,
                    checklist_row(
                        value.is_some(),
                        false,
                        "Board",
                        answer_value(value, false, area.width, palette),
                        palette,
                    ),
                    selected,
                );
            }
            WorkspaceAction::Shield => {
                // The optional answer under the board it rides on: a picked
                // name shows like any other answer, while "none" is stated
                // as such rather than left as an open question --- the
                // shield is the one checklist row whose empty answer is
                // valid.
                let shield = app.build.as_ref().and_then(|panel| panel.shield.clone());
                let value = match shield {
                    Some(name) => answer_value(Some(name), false, area.width, palette),
                    None => Span::raw("none (optional)").dim(),
                };
                let done = app
                    .build
                    .as_ref()
                    .is_some_and(|panel| panel.shield.is_some());
                y = render_row(
                    frame,
                    area,
                    y,
                    checklist_row(done, false, "Shield", value, palette),
                    selected,
                );
            }
        }
    }
    y = separator(frame, area, y);
    draw_files_section(frame, area, y, app, panel, palette);
}

/// The embedded file list under the checklist: a title bar --- the project's
/// own name, concatenated with the path walked below it ("blinkety/",
/// "blinkety/src/"), highlighted with the theme's selection background so it
/// reads as a bar, distinct from the listing under it --- then entries
/// filling the rest of the pane, the same `row`/`render_list` grammar
/// `crate::ui::files` draws for MicroPython's local pane, reused with no
/// comparison status (`row`'s `None` arm draws no marker column at all ---
/// this pane has no other side to compare against), so an empty listing, an
/// overlong name and a directory marker read identically here. A below-root
/// listing leads with a `[..]` parent row (see
/// [`WorkspacePanel::parent_row`]).
fn draw_files_section(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    app: &App,
    panel: &WorkspacePanel,
    palette: Palette,
) {
    if y >= area.bottom() {
        return;
    }
    let project = panel
        .files_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| panel.files_root.display().to_string());
    let title = panel
        .files_path
        .strip_prefix(&panel.files_root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| format!("{project}/{}/", relative.display()))
        .unwrap_or(format!("{project}/"));
    // Padded to the full row so the background fills the bar edge to edge.
    let text = shorten_start(&title, area.width as usize);
    let padding = (area.width as usize).saturating_sub(text.chars().count());
    let header = Line::from(format!("{text}{}", " ".repeat(padding))).style(
        Style::new()
            .bg(palette.selection)
            .fg(palette.fg)
            .add_modifier(Modifier::BOLD),
    );
    let y = render_row(frame, area, y, header, false);
    if y >= area.bottom() {
        return;
    }

    let list_area = Rect {
        x: area.x,
        y,
        width: area.width,
        height: area.bottom() - y,
    };
    if let Some(error) = &panel.files_error {
        frame.render_widget(
            Paragraph::new(error.clone().fg(palette.error)).wrap(Wrap { trim: true }),
            list_area,
        );
        return;
    }

    let focused = dashboard_focused(app, Focus::Workspace);
    // The `[..]` row leads whenever the listing is below the project root,
    // so `files_cursor` (which addresses drawn rows, 0 first) can be passed
    // straight through as the list's selection.
    let mut items = Vec::with_capacity(panel.files_row_count());
    if panel.parent_row() {
        items.push(super::files::row(
            "[..]",
            true,
            0,
            None,
            list_area.width,
            palette,
        ));
    }
    items.extend(panel.visible_files().iter().map(|entry| {
        super::files::row(
            &entry.name,
            entry.is_dir,
            entry.size,
            None,
            list_area.width,
            palette,
        )
    }));
    let cursor = panel.in_files.then_some(panel.files_cursor);
    super::files::render_list(frame, list_area, items, cursor, focused);
}

/// One checklist row: `✓` when the answer exists, a dim `□` while the
/// question is still open (the mark that says *this needs defining*), a
/// red `✗` when a configured one failed validation --- then the label,
/// then the answer itself. Shared with the project panel --- one
/// checklist grammar across the row.
pub(super) fn checklist_row(
    done: bool,
    broken: bool,
    label: &str,
    value: Span<'static>,
    palette: Palette,
) -> Line<'static> {
    let mark = if broken {
        Span::styled("✗", Style::new().fg(palette.error).bold())
    } else if done {
        Span::styled("✓", Style::new().fg(palette.success).bold())
    } else {
        Span::styled("□", Style::new().dim())
    };
    Line::from(vec![
        mark,
        Span::raw(" "),
        Span::styled(format!("{label:<15}"), Style::new().bold()),
        value,
    ])
}

/// The right-hand side of a checklist row: the answer when there is one,
/// a red `!` when a configured one failed validation, a yellow `?` while
/// the question is open.
pub(super) fn answer_value(
    answer: Option<String>,
    broken: bool,
    width: u16,
    palette: Palette,
) -> Span<'static> {
    if let Some(answer) = answer {
        shorten_start(&answer, value_budget(width))
            .fg(palette.success)
            .bold()
    } else if broken {
        "!".fg(palette.error).bold()
    } else {
        "?".fg(palette.warning).bold()
    }
}

/// Characters a checklist row's value may occupy: the mark, the space and
/// the 15-column label take 17.
pub(super) fn value_budget(width: u16) -> usize {
    (width as usize).saturating_sub(17).max(8)
}

/// The dim key a pane's pinned last line labels itself with (`state`,
/// `last`, `env`), in the same 6 columns across both panes.
pub(super) fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<6}"), Style::new().dim())
}

/// Renders one navigable row, full-width reversed when selected, and
/// returns the next row's y. Rows past the pane's bottom are dropped.
pub(super) fn render_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    line: Line<'static>,
    selected: bool,
) -> u16 {
    if y >= area.bottom() {
        return y;
    }
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    if selected {
        frame.render_widget(
            line.style(Style::new().add_modifier(Modifier::REVERSED)),
            rect,
        );
    } else {
        frame.render_widget(line, rect);
    }
    y + 1
}

/// The horizontal rule between the checklist and the buttons: the
/// prerequisite questions and the operations they unlock are different
/// kinds of rows, and the line says so at a glance.
pub(super) fn separator(frame: &mut Frame, area: Rect, y: u16) -> u16 {
    if y >= area.bottom() {
        return y;
    }
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from("─".repeat(area.width as usize)).dim()),
        rect,
    );
    y + 1
}

/// Renders a non-navigable info block (up to `height` wrapped lines)
/// under a checklist row, returning the next row's y.
fn render_info(frame: &mut Frame, area: Rect, y: u16, span: Span<'static>, height: u16) -> u16 {
    let height = height.min(area.bottom().saturating_sub(y));
    if height == 0 {
        return y;
    }
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    };
    frame.render_widget(
        Paragraph::new(Line::from(span)).wrap(Wrap { trim: false }),
        rect,
    );
    y + height
}

/// Shortens from the left: a path's tail (its distinctive part) matters
/// more than its `/tmp` prefix.
fn shorten_start(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = text.chars().skip(length - (max - 1)).collect();
    format!("…{tail}")
}
