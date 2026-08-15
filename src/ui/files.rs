//! Dual-pane file browser rendering.
//!
//! Two independently navigable panes, as in `mc`. The status column between
//! name and size is the comparison: it is computed against whatever the *other*
//! pane currently shows, so the panes can sit at unrelated paths and the
//! answer is still meaningful.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::backend::Capability;
use crate::browser::{Browser, PaneState};
use crate::device::{DiscoveryState, ScriptState};
use crate::files::SyncStatus;
use crate::ui::{SPINNER, content_style, dashboard_focused, pane_block};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(browser) = &app.browser else {
        let block = pane_block("Files", false);
        frame.render_widget(
            Paragraph::new("the file listing has not started yet".dim()).block(block),
            area,
        );
        return;
    };

    // The legend explains the comparison markers, which only exist when
    // there is a device pane to compare against; without it the row's last
    // line is dead weight for the local pane.
    let has_filesystem = app.manager.capabilities().contains(Capability::Filesystem);
    let (body, legend) = if has_filesystem {
        let [body, legend] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
        (body, Some(legend))
    } else {
        (area, None)
    };
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);

    let statuses = browser.statuses();
    draw_local(frame, left, app, browser, &statuses);
    if has_filesystem {
        draw_device(frame, right, app, browser, &statuses);
    } else if app.build_pane_visible() {
        super::build::draw(frame, right, app);
    } else {
        draw_no_device(frame, right, app);
    }
    if let Some(legend) = legend {
        draw_legend(frame, legend);
    }
}

/// The right half of row 2 for a backend with no [`Capability::Filesystem`]
/// (today: Zephyr): there is no device filesystem to browse, and this is the
/// space its build panel will occupy. Kept capability-gated, never
/// backend-kind-gated (`AGENTS.md` §3).
fn draw_no_device(frame: &mut Frame, area: Rect, app: &App) {
    let backend = app
        .manager
        .selected_kind()
        .map_or("this backend".to_string(), |kind| kind.to_string());
    let block = pane_block("Device", false);
    frame.render_widget(
        Paragraph::new(format!("{backend}: no device filesystem").dim()).block(block),
        area,
    );
}

fn draw_local(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    browser: &Browser,
    statuses: &BTreeMap<String, SyncStatus>,
) {
    let focused = dashboard_focused(app, Focus::FilesLocal);
    let title = format!(
        "Local files: {}",
        shorten(&browser.local_path.display().to_string(), area.width)
    );
    let block = pane_block(&title, focused);

    if let Some(error) = &browser.local_error {
        frame.render_widget(
            Paragraph::new(error.clone().fg(Color::Red))
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = browser
        .visible_local()
        .into_iter()
        .map(|entry| {
            row(
                &entry.name,
                entry.is_dir,
                entry.size,
                statuses.get(&entry.name).copied(),
                inner.width,
            )
        })
        .collect();

    render_list(frame, inner, items, browser.local_cursor, focused);
}

fn draw_device(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    browser: &Browser,
    statuses: &BTreeMap<String, SyncStatus>,
) {
    let focused = dashboard_focused(app, Focus::FilesDevice);
    // The running-script flag rides along in the title rather than the body:
    // it explains why an overlay may appear, without claiming list space.
    let mut title = format!("Device files: {}", browser.device_path);
    if app.devices.script_state() == ScriptState::Running {
        title.push_str(" · script running");
    }
    let block = pane_block(&title, focused);

    match &browser.device_state {
        PaneState::Idle => {
            frame.render_widget(
                Paragraph::new("press 'd' to look for a device".dim()).block(block),
                area,
            );
            return;
        }
        PaneState::Loading => {
            let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];
            // Finding the board, waiting on the user and listing it are all
            // waits, but they fail or stall for different reasons, so they
            // are named differently.
            let what = if browser.held_for_interrupt() {
                "waiting to interrupt a running script".to_string()
            } else if app.devices.discovery == DiscoveryState::Scanning {
                "searching for a device".to_string()
            } else {
                format!("listing {}", browser.device_path)
            };
            frame.render_widget(
                Paragraph::new(format!("{spinner} {what}…").dim()).block(block),
                area,
            );
            return;
        }
        PaneState::Failed(error) => {
            frame.render_widget(
                Paragraph::new(error.clone().fg(Color::Red))
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        PaneState::Ready => {}
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let items: Vec<ListItem> = browser
        .visible_device()
        .into_iter()
        .map(|entry| {
            row(
                &entry.name,
                entry.is_dir,
                entry.size,
                statuses.get(&entry.name).copied(),
                list_area.width,
            )
        })
        .collect();

    render_list(frame, list_area, items, browser.device_cursor, focused);
    draw_device_footer(frame, footer_area, browser);
}

/// Free space on the connected board, as a progress bar --- filled by the
/// used fraction, colored green/yellow/red at 70%/90% used so it doubles as
/// an early warning before an upload runs out of room.
fn draw_device_footer(frame: &mut Frame, area: Rect, browser: &Browser) {
    match &browser.device_space {
        Some(Ok(usage)) if usage.total > 0 => {
            // `Gauge::ratio` panics outside 0.0..=1.0; `total > 0` above and the
            // clamp here keep a malformed or stale reading from crashing the UI.
            let used_ratio = (usage.used as f64 / usage.total as f64).clamp(0.0, 1.0);
            let color = if used_ratio >= 0.9 {
                Color::Red
            } else if used_ratio >= 0.7 {
                Color::Yellow
            } else {
                Color::Green
            };
            // `Gauge::label` is always centered, with no alignment option ---
            // left-padding the text to the full width forces it flush right
            // instead, matching the local pane's footer.
            let label = format!(
                "total: {}/{}",
                human_size(usage.used),
                human_size(usage.total)
            );
            let label = format!("{label:>width$}", width = area.width as usize);
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::new().fg(color))
                    .ratio(used_ratio)
                    .label(label),
                area,
            );
        }
        Some(Ok(_)) => {
            frame.render_widget(Paragraph::new("free space: 0".dim()), area);
        }
        Some(Err(_)) => {
            frame.render_widget(Paragraph::new("free space unavailable".dim()), area);
        }
        None => {
            frame.render_widget(Paragraph::new("checking free space…".dim()), area);
        }
    }
}

fn render_list(
    frame: &mut Frame,
    area: Rect,
    items: Vec<ListItem<'static>>,
    cursor: usize,
    focused: bool,
) {
    if items.is_empty() {
        frame.render_widget(Paragraph::new("empty".dim()), area);
        return;
    }

    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(
        List::new(items)
            .style(content_style(focused))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

/// One entry: `<status> <name> <size>`, with the size flush right.
fn row(
    name: &str,
    is_dir: bool,
    size: u64,
    status: Option<SyncStatus>,
    width: u16,
) -> ListItem<'static> {
    let status = status.unwrap_or(SyncStatus::Directory);
    let size_text = if is_dir {
        "DIR".to_string()
    } else {
        human_size(size)
    };

    // 2 for the marker, 1 space, then the size column and a gap.
    let name_width = (width as usize).saturating_sub(3 + size_text.len() + 1);
    let display = truncate(name, name_width.max(1));
    let padding = name_width.saturating_sub(display.chars().count());

    let name_style = if is_dir {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };

    ListItem::new(Line::from(vec![
        Span::styled(format!("{} ", status.marker()), status_style(status)),
        Span::styled(display, name_style),
        Span::raw(" ".repeat(padding + 1)),
        Span::styled(size_text, Style::new().dim()),
    ]))
}

fn status_style(status: SyncStatus) -> Style {
    match status {
        SyncStatus::Identical => Style::new().fg(Color::Green),
        SyncStatus::SameSize => Style::new().fg(Color::Green).dim(),
        SyncStatus::Differs => Style::new().fg(Color::Yellow),
        SyncStatus::LocalOnly => Style::new().fg(Color::Cyan),
        SyncStatus::DeviceOnly => Style::new().fg(Color::Magenta),
        SyncStatus::Directory => Style::new().dim(),
        SyncStatus::TypeMismatch => Style::new().fg(Color::Red),
    }
}

fn draw_legend(frame: &mut Frame, area: Rect) {
    let entries = [
        (SyncStatus::Identical, "identical"),
        (SyncStatus::SameSize, "same size"),
        (SyncStatus::Differs, "differs"),
        (SyncStatus::LocalOnly, "local only"),
        (SyncStatus::DeviceOnly, "device only"),
    ];

    let mut spans = Vec::new();
    for (status, label) in entries {
        spans.push(Span::styled(
            format!(" {} ", status.marker()),
            status_style(status),
        ));
        spans.push(Span::styled(format!("{label}  "), Style::new().dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
        area,
    );
}

/// Sizes in the width a file pane can spare.
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;

    if bytes >= MIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1}k", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes}")
    }
}

/// Truncates a name, keeping the extension visible.
fn truncate(name: &str, max: usize) -> String {
    let length = name.chars().count();
    if length <= max {
        return name.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let kept: String = name.chars().take(max - 1).collect();
    format!("{kept}…")
}

/// Shortens a path from the left for a pane title.
fn shorten(path: &str, width: u16) -> String {
    let budget = (width as usize).saturating_sub(12);
    let length = path.chars().count();
    if length <= budget {
        return path.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    let tail: String = path.chars().skip(length - (budget - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_scaled_for_narrow_columns() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(139), "139");
        assert_eq!(human_size(2048), "2.0k");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    #[test]
    fn names_are_truncated_from_the_right() {
        assert_eq!(truncate("main.py", 10), "main.py");
        assert_eq!(truncate("a_very_long_module.py", 10), "a_very_lo…");
        assert_eq!(truncate("main.py", 1), "…");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let name = "configuração_do_dispositivo.py";
        let truncated = truncate(name, 12);
        assert_eq!(truncated.chars().count(), 12);
    }

    #[test]
    fn each_status_gets_its_own_colour() {
        // The marker alone must not be the only cue, for narrow terminals and
        // for anyone reading at a glance.
        assert_ne!(
            status_style(SyncStatus::LocalOnly),
            status_style(SyncStatus::DeviceOnly)
        );
        assert_ne!(
            status_style(SyncStatus::Differs),
            status_style(SyncStatus::Identical)
        );
    }
}
