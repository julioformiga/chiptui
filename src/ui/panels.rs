//! Dashboard panes: project, device, the no-filesystem placeholder, and log.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs, Wrap};

use crate::app::{App, Focus, LogTab, MonitorSource};
use crate::backend::Capability;
use crate::flash::RunState;
use crate::logs::{Level, PREFIX_WIDTH};
use crate::project::DetectionOutcome;
use crate::ui::{SPINNER, content_style, dashboard_focused, pane_block, pane_border};

/// Project identity: where it is, what it is, and how sure we are.
///
/// This pane is informational only --- it never holds focus, so it always
/// renders with a neutral (non-dimmed) style regardless of which pane is
/// active.
pub fn draw_project(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane_block("Project", false);
    let lines = project_content(app, area.width as usize);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Builds the Project pane's content lines. Extracted so the dashboard layout
/// can size the info row to fit the taller of the two panes (`draw_dashboard`)
/// without rendering twice.
pub(super) fn project_content(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let detection = app.manager.detection();

    // Deep embedded trees produce long paths; the tail identifies the project,
    // so they are shortened from the left instead of wrapping over three lines.
    let path_budget = width.saturating_sub(2 + LABEL_WIDTH);
    // A backend that makes the project a question (`ProjectSelect`) answers
    // it with the build panel's root, so a picked project re-roots this
    // field too; every other backend keeps the detection root.
    let root = if app
        .manager
        .capabilities()
        .contains(Capability::ProjectSelect)
        && let Some(panel) = &app.build
    {
        panel.root.display().to_string()
    } else {
        app.manager.root().map_or_else(
            || app.manager.start_dir().display().to_string(),
            |root| root.display().to_string(),
        )
    };
    lines.push(field("root", truncate_start(&root, path_budget)));

    // Only worth showing when the search climbed out of the working directory.
    if app
        .manager
        .root()
        .is_some_and(|root| root != app.manager.start_dir())
    {
        let cwd = app.manager.start_dir().display().to_string();
        lines.push(field("cwd", truncate_start(&cwd, path_budget)));
    }

    match detection.map(|d| &d.outcome) {
        Some(DetectionOutcome::Detected(kind)) => {
            lines.push(field_styled(
                "type",
                kind.display_name().to_string(),
                Style::new().fg(Color::Green).bold(),
            ));
        }
        Some(DetectionOutcome::Ambiguous(kinds)) => {
            let names = kinds
                .iter()
                .map(|k| k.display_name())
                .collect::<Vec<_>>()
                .join(" / ");
            lines.push(field_styled(
                "type",
                format!("ambiguous: {names} --- press 'o' to choose"),
                Style::new().fg(Color::Yellow),
            ));
        }
        Some(DetectionOutcome::Unknown) => {
            lines.push(field_styled(
                "type",
                "unknown".to_string(),
                Style::new().fg(Color::Red),
            ));
        }
        None => lines.push(field("type", "not detected yet".to_string())),
    }

    // The environment's versions, once a workspace resolves: read from
    // files (`zephyr/VERSION`, the venv's `pyvenv.cfg`), never from a
    // subprocess. Where the detection answer came from stays in the log
    // and the picker; this pane reports the state it builds against.
    if let Some(workspace) = app
        .workspace
        .as_ref()
        .and_then(|panel| panel.resolved.as_ref())
    {
        let mut versions = format!(
            "zephyr {}",
            workspace
                .zephyr_version()
                .unwrap_or_else(|| "unknown".to_string())
        );
        if let Some(python) = workspace.python_version() {
            versions.push_str(&format!(" · python {python}"));
        }
        lines.push(field("versions", versions));
    }

    // Tool availability, since a capability whose tool is missing is not usable.
    // A backend whose west comes from a workspace venv is checked against that
    // absolute path instead of `PATH` --- a venv-only west is not "missing".
    if let Some(kind) = app.manager.selected_kind() {
        let mut spans = vec![label_span("tools")];
        for (tool, available) in app.manager.registry().tool_status(kind) {
            let available = if tool == "west" {
                app.workspace
                    .as_ref()
                    .and_then(|panel| panel.resolved.as_ref())
                    .map_or(available, |workspace| {
                        std::path::Path::new(&workspace.west).is_file()
                    })
            } else {
                available
            };
            let style = if available {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            };
            spans.push(Span::styled(
                format!("{} {tool}  ", if available { "✓" } else { "✗" }),
                style,
            ));
        }
        lines.push(Line::from(spans));
    }

    lines
}

/// What esptool has reported about the connected board so far: identity
/// (chip/revision/features/crystal/MAC) and flash geometry, accumulated
/// across whatever `chip-id`/`flash-id`/flash/erase/verify runs have
/// happened in the Flash view (`crate::flash::FlashPanel::details`). The
/// backend's own name already lives in the Project pane above, so this space
/// is spent on the board itself instead of repeating it. Like the Project
/// pane it is informational only and never holds focus.
pub fn draw_detection(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane_block("Device info", false);
    let lines = device_content(app);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Builds the Device info pane's content lines (placeholder or details). See
/// [`project_content`] for why this is split out.
pub(super) fn device_content(app: &App) -> Vec<Line<'static>> {
    let caps = app.manager.capabilities();
    let dim = Style::new().add_modifier(Modifier::DIM);

    // This pane is esptool's report (chip/flash geometry). A backend that
    // flashes another way (Zephyr: `west flash` through the build panel)
    // declares no `DeviceInfo`/`EraseFlash` and gets the honest placeholder
    // instead of a hint pointing at a dialog that cannot talk to its board.
    if !caps.contains(Capability::DeviceInfo) && !caps.contains(Capability::EraseFlash) {
        return vec![Line::from("no device information for this project").style(dim)];
    }
    let Some(flash) = app.flash.as_ref() else {
        return vec![Line::from("press 'x' to open Flash and query the device").style(dim)];
    };
    let details = &flash.details;
    if details.is_empty() {
        return vec![
            Line::from("no device data yet --- run chip or flash information from the Flash menu")
                .style(dim),
        ];
    }

    let mut lines = Vec::new();
    if let Some(family) = details.family {
        let mut spans = vec![
            label_span("chip"),
            Span::styled(
                family.label().to_string(),
                Style::new().fg(Color::Green).bold(),
            ),
        ];
        if let Some(revision) = &details.revision {
            spans.push(Span::raw(format!(" (revision {revision})")));
        }
        lines.push(Line::from(spans));
    }
    if let Some(features) = &details.features {
        lines.push(field("features", features.clone()));
    }
    if let Some(crystal) = &details.crystal_mhz {
        lines.push(field("crystal", crystal.clone()));
    }
    if let Some(mac) = &details.mac {
        lines.push(field("MAC", mac.clone()));
    }

    // `memory:` shares the `flash id:` line (5 spaces after the flash id
    // value) so the info row stays compact.
    let has_flash_id = details.flash_manufacturer.is_some() || details.flash_device.is_some();
    let has_memory = details.flash_size.is_some();
    if has_flash_id || has_memory {
        let mut spans = vec![label_span("flash id")];
        if has_flash_id {
            spans.push(Span::raw(format!(
                "{} / {}",
                details.flash_manufacturer.as_deref().unwrap_or("?"),
                details.flash_device.as_deref().unwrap_or("?"),
            )));
        }
        if let Some(size) = &details.flash_size {
            spans.push(Span::raw("          "));
            spans.push(Span::styled("memory:", Style::new().dim()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(size.clone(), Style::new().fg(Color::Cyan)));
        }
        lines.push(Line::from(spans));
    }

    lines
}

/// Row 2 placeholder for the window before a browser exists at all (a
/// fresh `bootstrap()` that has not yet run `App::maybe_scan_devices`).
/// Once the browser exists the local pane always renders, whatever the
/// backend; the right half is the device pane under
/// [`Capability::Filesystem`], else [`crate::ui::files`]'s placeholder.
pub fn draw_no_filesystem(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane_block("Files", false);
    let backend = app
        .manager
        .selected_kind()
        .map_or("this backend".to_string(), |kind| kind.to_string());
    frame.render_widget(
        Paragraph::new(format!("{backend}: file browsing not implemented yet").dim()).block(block),
        area,
    );
}

/// Rolling status/log output.
///
/// Long entries wrap at the pane's width with a hanging indent past the
/// stamp, so a wrapped paragraph stays visually tied to its timestamp
/// instead of overflowing or being cut off at the terminal edge.
pub fn draw_logs(frame: &mut Frame, area: Rect, app: &mut App) {
    // The tab strip owns the border row (see `draw_log_tabs`), so the pane
    // itself carries no title.
    let focused = dashboard_focused(app, Focus::Logs);
    let block = pane_border(focused);

    // Publish the usable height so page-scrolling matches the rendered view,
    // and the wrapping width so clamping matches too. One column is reserved
    // for the scrollbar whether or not it is showing, so text never reflows
    // the moment the log outgrows the pane.
    let inner = block.inner(area);
    let gutter = if inner.width > 0 { 1 } else { 0 };
    app.log_viewport = (inner.height as usize).max(1);
    app.logs
        .set_view_width(inner.width.saturating_sub(gutter) as usize);

    let lines: Vec<Line> = app
        .logs
        .visible_rows(app.log_viewport)
        .into_iter()
        .map(|row| {
            let style = match row.entry.level {
                Level::Info => Style::new(),
                Level::Success => Style::new().fg(Color::Green),
                Level::Warn => Style::new().fg(Color::Yellow),
                Level::Error => Style::new().fg(Color::Red),
            };
            if row.first {
                let centis = row.entry.at.millisecond() / 10;
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{:02}:{:02}:{:02}.{centis:02} ",
                            row.entry.at.hour(),
                            row.entry.at.minute(),
                            row.entry.at.second()
                        ),
                        Style::new().dim(),
                    ),
                    Span::styled(format!("{} ", row.entry.level.marker()), style),
                    Span::styled(row.text, style),
                ])
            } else {
                // Continuation of a wrapped entry: indented past the stamp so
                // the whole paragraph reads as one timestamped line.
                Line::from(vec![
                    Span::raw(" ".repeat(PREFIX_WIDTH)),
                    Span::styled(row.text, style),
                ])
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(content_style(focused)),
        area,
    );

    draw_log_scrollbar(frame, inner, app);
}

/// The log pane's scrollbar: the shared bar (see [`crate::ui::draw_scrollbar`])
/// over visual (post-wrap) lines, with the thumb reflecting the scroll
/// position (bottom while following the tail).
fn draw_log_scrollbar(frame: &mut Frame, inner: Rect, app: &App) {
    let total = app.logs.total_lines();
    let viewport = app.log_viewport;
    if total <= viewport {
        return;
    }
    let max_scroll = total - viewport;
    let top = max_scroll - app.logs.scroll().min(max_scroll);
    crate::ui::draw_scrollbar(frame, inner, total, viewport, top);
}

/// Row 3's tab strip, drawn over the pane's own top border like the Ratatui
/// `Tabs` example: ` Log • Monitor `. `Monitor` is omitted entirely when the
/// backend has no `Capability::Monitor` --- capability-gated, never
/// backend-kind-gated (`AGENTS.md` §3). At the strip's right edge rides the
/// active tab's status: for Monitor, the source's title, a live icon (an
/// animated spinner while a command runs, a green check --- red cross on
/// failure --- for the last finished one) and the output's row count; for
/// Log, the entry count and, while scrolled, how far up the view sits.
pub fn draw_log_tabs(frame: &mut Frame, pane: Rect, app: &App) {
    // The strip spans the border row between the corners; drawn after the
    // pane's own widgets so it sits on top of the border.
    let strip = Rect {
        x: pane.x.saturating_add(1),
        y: pane.y,
        width: pane.width.saturating_sub(2),
        height: 1,
    };
    if strip.width == 0 {
        return;
    }

    let focused = dashboard_focused(app, Focus::Logs);
    let has_monitor = app.manager.capabilities().contains(Capability::Monitor);

    // The active tab reads brighter than the dim inactive one even without
    // focus, and like every focused pane goes cyan while Logs holds focus;
    // the underline makes the selection unmistakable either way (a
    // background highlight would vanish against some color schemes when the
    // pane is unfocused).
    let active_style = if focused {
        Style::new()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let inactive_style = Style::new().dim();

    let monitor = vec![Span::styled(
        "Monitor",
        if app.log_tab == LogTab::Monitor {
            active_style
        } else {
            inactive_style
        },
    )];

    let titles = vec![
        Line::from(Span::styled(
            "Log",
            if app.log_tab == LogTab::Log {
                active_style
            } else {
                inactive_style
            },
        )),
        Line::from(monitor),
    ];

    let selected_index = match app.log_tab {
        LogTab::Log => Some(0),
        LogTab::Monitor => has_monitor.then_some(1),
    };

    // The selection style lives on the titles themselves; the highlight
    // style adds nothing.
    let tabs = Tabs::new(titles)
        .select(selected_index)
        .style(inactive_style)
        .highlight_style(Style::new())
        .padding(" ", " ")
        .divider(symbols::DOT);

    frame.render_widget(tabs, strip);
    frame.render_widget(
        Paragraph::new(tab_status(app)).alignment(Alignment::Right),
        strip,
    );
}

/// The active tab's status, drawn at the strip's right edge. A leading
/// space keeps the pane's border dashes from touching it.
fn tab_status(app: &App) -> Line<'static> {
    match app.log_tab {
        LogTab::Log => {
            let mut text = format!(" Log ({})", app.logs.total_lines());
            if !app.logs.is_following() {
                text.push_str(&format!(" \u{2191}{}", app.logs.scroll()));
            }
            Line::from(text).dim()
        }
        LogTab::Monitor => monitor_status(app),
    }
}

/// The Monitor tab's status: the source's title with its live icon and the
/// output's row count (`App::monitor_view.rows`, published by the console's
/// renderer --- the strip draws after it, so the count is fresh).
fn monitor_status(app: &App) -> Line<'static> {
    let spinner = || {
        Some((
            SPINNER[(app.ticks as usize) % SPINNER.len()],
            Style::new().fg(Color::Yellow),
        ))
    };
    let check = || ("\u{2713}", Style::new().fg(Color::Green));
    let cross = || ("\u{2717}", Style::new().fg(Color::Red));

    let (icon, title): (Option<(&str, Style)>, String) = match app.monitor_source {
        MonitorSource::Build => match app.build.as_ref() {
            Some(panel) => {
                if let Some(label) = panel.running_label() {
                    (spinner(), label.to_string())
                } else if let Some(report) = panel.last.as_ref() {
                    let icon = if report.ok { check() } else { cross() };
                    (Some(icon), report.what.to_string())
                } else {
                    (None, "Build".to_string())
                }
            }
            None => (None, "Monitor".to_string()),
        },
        MonitorSource::Flash => match app.flash.as_ref() {
            Some(flash) => {
                let icon = match &flash.state {
                    RunState::Running => spinner(),
                    RunState::Succeeded => Some(check()),
                    RunState::Failed(_) => Some(cross()),
                    RunState::Idle => None,
                };
                let action = flash.selected_action();
                let title = if action.needs_firmware() {
                    action.label().to_string()
                } else {
                    "Monitor".to_string()
                };
                (icon, title)
            }
            None => (None, "Monitor".to_string()),
        },
        MonitorSource::Run => {
            let icon = match app.run_state {
                crate::app::RunState::Running => spinner(),
                crate::app::RunState::Finished => Some(check()),
                crate::app::RunState::Idle => None,
            };
            let title = app.run_script.as_ref().map_or_else(
                || "Run".to_string(),
                |path| format!("Run: {}", path.display()),
            );
            (icon, title)
        }
        MonitorSource::Device => {
            let icon = if app.device_monitor_process.is_some() {
                spinner()
            } else {
                None
            };
            (icon, "Monitor".to_string())
        }
    };

    let mut spans = vec![Span::raw(" ")];
    if let Some((icon, style)) = icon {
        spans.push(Span::styled(icon.to_string(), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(title, Style::new().dim()));
    spans.push(Span::styled(
        format!(" ({})", app.monitor_view.rows),
        Style::new().dim(),
    ));
    // The scrolled indicator Log shows: `↑N` with the rows *below* the view
    // (the same meaning as `LogStore::scroll`), shown only once the user
    // leaves the tail.
    if !app.monitor_scroll.following {
        let below = app
            .monitor_view
            .rows
            .saturating_sub(app.monitor_view.viewport + app.monitor_scroll.offset);
        spans.push(Span::styled(
            format!(" \u{2191}{below}"),
            Style::new().dim(),
        ));
    }
    Line::from(spans)
}

/// Width of the field-label column, including its colon and trailing space.
const LABEL_WIDTH: usize = 11;

fn label_span(label: &str) -> Span<'static> {
    Span::styled(
        format!("{:<LABEL_WIDTH$}", format!("{label}:")),
        Style::new().dim(),
    )
}

/// Shortens `text` from the left, keeping the tail.
///
/// Paths are truncated at the front because the last components --- the project
/// directory --- are what identify it.
fn truncate_start(text: &str, max: usize) -> String {
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

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![label_span(label), Span::raw(value)])
}

fn field_styled(label: &str, value: String, style: Style) -> Line<'static> {
    Line::from(vec![label_span(label), Span::styled(value, style)])
}

#[cfg(test)]
mod tests {
    use super::truncate_start;

    #[test]
    fn short_text_is_left_alone() {
        assert_eq!(truncate_start("/home/dev/blinky", 40), "/home/dev/blinky");
        assert_eq!(truncate_start("abc", 3), "abc");
    }

    #[test]
    fn long_paths_keep_their_tail() {
        assert_eq!(truncate_start("/home/dev/zephyr/app", 10), "…ephyr/app");
        assert!(truncate_start("/home/dev/zephyr/app", 10).chars().count() <= 10);
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        assert_eq!(truncate_start("/very/long/path", 0), "…");
        assert_eq!(truncate_start("/very/long/path", 1), "…");
        assert_eq!(truncate_start("", 0), "");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Paths can contain multi-byte characters; slicing by byte would panic.
        let path = "/home/dev/µcontroller/blinky";
        let truncated = truncate_start(path, 12);
        assert!(truncated.chars().count() <= 12);
        assert!(truncated.ends_with("blinky"));
    }
}
