//! Workspace-pane rendering: the environment's status (workspace, west,
//! Zephyr and SDK versions) over a navigable action list whose rows show
//! the literal command each entry would run --- the same
//! never-paraphrase rule as the build panel (`SPEC.md` §15).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::ui::{content_style, dashboard_focused, pane_block};
use crate::workspace::WorkspaceAction;

/// Draws the full second row for a workspace+build backend: the workspace
/// pane on the left, the build panel (already its own module) on the right.
pub fn draw_row(frame: &mut Frame, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    draw(frame, left, app);
    super::build::draw(frame, right, app);
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if app.workspace.is_none() {
        return;
    }
    let focused = dashboard_focused(app, Focus::Workspace);
    let block = pane_block("Workspace", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [status, list] = Layout::vertical([Constraint::Length(7), Constraint::Min(1)]).areas(inner);

    draw_status(frame, status, app);
    draw_actions(frame, list, app, focused);
}
/// The environment's status: workspace + origin, west executable, Zephyr
/// version, SDK. Unresolved states say what to do next --- the actionable
/// error `SPEC.md` §14 asks for, not a dead pane.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let Some(panel) = &app.workspace else {
        return;
    };
    let lines = match &panel.resolved {
        Some(workspace) => {
            let version = workspace
                .zephyr_version()
                .map_or_else(|| "unknown".dim(), |v| v.fg(Color::Green));
            let west_line = if workspace.west == "west" {
                Line::from(vec![
                    label("west"),
                    "west".fg(Color::Green),
                    Span::raw(" "),
                    "from PATH".dim(),
                ])
            } else {
                Line::from(vec![
                    label("west"),
                    shorten_start(&workspace.west, (area.width as usize).saturating_sub(9))
                        .fg(Color::Green),
                ])
            };
            let sdk_line = match &workspace.sdk {
                Some(sdk) => {
                    let version = workspace
                        .sdk_version()
                        .map(|v| format!(" ({v})"))
                        .unwrap_or_default();
                    Line::from(vec![
                        label("sdk"),
                        shorten_start(
                            &format!("{}{version}", sdk.display()),
                            (area.width as usize).saturating_sub(11),
                        )
                        .fg(Color::Green),
                    ])
                }
                None => Line::from(vec![label("sdk"), "auto (set [zephyr] sdk to pin)".dim()]),
            };
            vec![
                Line::from(vec![
                    label("path"),
                    shorten_start(
                        &workspace.dir.display().to_string(),
                        (area.width as usize).saturating_sub(9),
                    )
                    .fg(Color::Green)
                    .bold(),
                    Span::raw(" "),
                    format!("({})", workspace.origin.label()).dim(),
                ]),
                Line::from(vec![label("zephyr"), version]),
                west_line,
                sdk_line,
            ]
        }
        None => {
            let config_path = crate::settings::user_config_path(app.home_dir());
            let short_config = shorten_start(
                &config_path.display().to_string(),
                (area.width as usize).saturating_sub(13),
            );
            if let Some(message) = &panel.invalid {
                // A configured location that is not an installation: the
                // reason (which carries the install guide link), then the
                // two ways out --- pick a real directory, or fix the file.
                vec![
                    Line::from(message.clone().fg(Color::Red)),
                    Line::from("Enter: choose the right directory".fg(Color::Yellow)),
                    Line::from(vec!["or fix ".dim(), short_config.dim().bold()]),
                ]
            } else {
                // Nothing configured: the picker answers this in three
                // keys, and the template below documents the config file
                // for whoever prefers setting it by hand --- every
                // `[zephyr]` key with its meaning.
                vec![
                    Line::from("no location configured".fg(Color::Yellow)),
                    Line::from(vec![
                        "Enter: ".dim(),
                        "choose the Zephyr installation directory".fg(Color::Yellow),
                    ]),
                    Line::from(vec![
                        "or set it in ".dim(),
                        short_config.dim().bold(),
                        ":".dim(),
                    ]),
                    Line::from("[zephyr]".dim().bold()),
                    Line::from("workspace = \"…\"".dim()),
                    Line::from("# sdk = \"…\"  (toolchain)   # west = \"…\"".dim()),
                ]
            }
        }
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_actions(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let Some(panel) = &app.workspace else {
        return;
    };
    let backend = app.manager.backend();
    let caps = app.manager.capabilities();
    let mut items = Vec::new();
    for action in panel.actions(&caps) {
        let item = match action {
            WorkspaceAction::Update => {
                let command = backend
                    .and_then(|backend| panel.update_command(backend))
                    .map(|command| command.to_string())
                    .unwrap_or_else(|| "resolve a workspace first".to_string());
                Line::from(vec![
                    Span::raw("  "),
                    "Update".bold(),
                    Span::raw("  "),
                    Span::styled(command, Style::new().dim()),
                ])
            }
            WorkspaceAction::SdkList => {
                let command = backend
                    .and_then(|backend| panel.sdk_list_command(backend))
                    .map(|command| command.to_string())
                    .unwrap_or_else(|| "resolve a workspace first".to_string());
                Line::from(vec![
                    Span::raw("  "),
                    "SdkList".bold(),
                    Span::raw("  "),
                    Span::styled(command, Style::new().dim()),
                ])
            }
            WorkspaceAction::Choose => Line::from(vec![
                Span::raw("  "),
                "Choose".bold(),
                Span::raw("  "),
                Span::styled(
                    "where is the Zephyr installation? (saved to the config)",
                    Style::new().dim(),
                ),
            ]),
        };
        items.push(ListItem::new(item));
    }

    let mut state = ListState::default().with_selected(Some(panel.cursor));
    frame.render_stateful_widget(
        List::new(items)
            .style(content_style(focused))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<7}"), Style::new().dim())
}

/// Shortens from the left, like the build panel's command rows: a path's
/// tail (its distinctive part) matters more than its `/tmp` prefix.
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
