//! Dashboard panes: project, device, the no-filesystem placeholder, and log.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs, Wrap};

use crate::app::{App, Focus, LogTab};
use crate::backend::Capability;
use crate::logs::Level;
use crate::project::{DetectionOutcome, DetectionSource};
use crate::ui::{content_style, dashboard_focused, pane_block};

/// Project identity: where it is, what it is, and how sure we are.
pub fn draw_project(frame: &mut Frame, area: Rect, app: &App) {
    let focused = dashboard_focused(app, Focus::Project);
    let block = pane_block("Project", focused);
    let mut lines = Vec::new();

    let detection = app.manager.detection();

    // Deep embedded trees produce long paths; the tail identifies the project,
    // so they are shortened from the left instead of wrapping over three lines.
    let path_budget = (area.width as usize).saturating_sub(2 + LABEL_WIDTH);
    let root = app.manager.root().map_or_else(
        || app.manager.start_dir().display().to_string(),
        |root| root.display().to_string(),
    );
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

    if let Some(detection) = detection {
        let source = match detection.source {
            DetectionSource::Automatic => "automatic",
            DetectionSource::Manual => "manual override",
            DetectionSource::Config => "saved in chiptui.toml",
        };
        lines.push(field("source", source.to_string()));
    }

    // Tool availability, since a capability whose tool is missing is not usable.
    if let Some(kind) = app.manager.selected_kind() {
        let mut spans = vec![label_span("tools")];
        for (tool, available) in app.manager.registry().tool_status(kind) {
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

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(content_style(focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// What esptool has reported about the connected board so far: identity
/// (chip/revision/features/crystal/MAC) and flash geometry, accumulated
/// across whatever `chip-id`/`flash-id`/flash/erase/verify runs have
/// happened in the Flash view (`crate::flash::FlashPanel::details`). The
/// backend's own name already lives in the Project pane above, so this space
/// is spent on the board itself instead of repeating it.
pub fn draw_detection(frame: &mut Frame, area: Rect, app: &App) {
    let focused = dashboard_focused(app, Focus::Project);
    let block = pane_block("Device", focused);
    let caps = app.manager.capabilities();

    if !caps.contains(Capability::Flash) && !caps.contains(Capability::EraseFlash) {
        frame.render_widget(
            Paragraph::new("no device information for this project".dim()).block(block),
            area,
        );
        return;
    }
    let Some(flash) = app.flash.as_ref() else {
        frame.render_widget(
            Paragraph::new("press 'x' to open Flash and query the device".dim()).block(block),
            area,
        );
        return;
    };
    let details = &flash.details;
    if details.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "no device data yet --- run chip or flash information from the Flash menu".dim(),
            )
            .block(block),
            area,
        );
        return;
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
    if details.flash_manufacturer.is_some() || details.flash_device.is_some() {
        lines.push(field(
            "flash id",
            format!(
                "{} / {}",
                details.flash_manufacturer.as_deref().unwrap_or("?"),
                details.flash_device.as_deref().unwrap_or("?"),
            ),
        ));
    }
    if let Some(size) = &details.flash_size {
        lines.push(field_styled(
            "memory",
            size.clone(),
            Style::new().fg(Color::Cyan),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(content_style(focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Row 2 placeholder when the selected backend declares no
/// [`Capability::Filesystem`] (today: Zephyr) --- `SPEC.md` §11 spec's a
/// dual-pane file browser for this row, but only a backend with a device
/// filesystem can back it.
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
pub fn draw_logs(frame: &mut Frame, area: Rect, app: &mut App) {
    let following = app.logs.is_following();
    let title = if following {
        format!("Log ({})", app.logs.len())
    } else {
        format!("Log ({}) ↑{}", app.logs.len(), app.logs.scroll())
    };
    let focused = dashboard_focused(app, Focus::Logs);
    let block = pane_block(&title, focused);

    // Publish the usable height so page-scrolling matches the rendered view.
    let viewport = block.inner(area).height as usize;
    app.log_viewport = viewport.max(1);

    let lines: Vec<Line> = app
        .logs
        .visible(app.log_viewport)
        .map(|entry| {
            let style = match entry.level {
                Level::Info => Style::new(),
                Level::Success => Style::new().fg(Color::Green),
                Level::Warn => Style::new().fg(Color::Yellow),
                Level::Error => Style::new().fg(Color::Red),
            };
            Line::from(vec![
                Span::styled(
                    format!("{:>7.2}s ", entry.at.as_secs_f32()),
                    Style::new().dim(),
                ),
                Span::styled(format!("{} ", entry.level.marker()), style),
                Span::styled(entry.message.clone(), style),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(content_style(focused)),
        area,
    );
}

/// Row 3's tab strip: `Log` / `Monitor`. `Monitor` is omitted entirely when
/// the backend has no `Capability::Monitor` --- capability-gated, never
/// backend-kind-gated (`AGENTS.md` §3).
pub fn draw_log_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let focused = dashboard_focused(app, Focus::Logs);
    let has_monitor = app.manager.capabilities().contains(Capability::Monitor);

    let mut titles = vec![" Log "];
    if has_monitor {
        titles.push(" Monitor ");
    }

    let selected_index = match app.log_tab {
        LogTab::Log => 0,
        LogTab::Monitor => 1,
    };

    let highlight_style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().dim().add_modifier(Modifier::BOLD)
    };

    let tabs = Tabs::new(titles)
        .select(selected_index)
        .style(Style::new().dim())
        .highlight_style(highlight_style)
        .divider(" ");

    frame.render_widget(tabs, area);
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
