//! The four panes: project, detection evidence, capabilities and log.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::backend::Capability;
use crate::logs::Level;
use crate::project::{DetectionOutcome, DetectionSource};
use crate::ui::{confidence_bar, confidence_color, pane};

/// Project identity: where it is, what it is, and how sure we are.
pub fn draw_project(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane("Project", Focus::Project, app);
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
            let confidence = detection.and_then(|d| d.confidence()).unwrap_or(0.0);
            lines.push(field_styled(
                "type",
                kind.display_name().to_string(),
                Style::new().fg(Color::Green).bold(),
            ));
            lines.push(field_styled(
                "confidence",
                confidence_bar(confidence),
                Style::new().fg(confidence_color(confidence)),
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
                format!("ambiguous: {names}"),
                Style::new().fg(Color::Yellow),
            ));
            lines.push(field_styled(
                "confidence",
                "press 'o' to choose".to_string(),
                Style::new().dim(),
            ));
        }
        Some(DetectionOutcome::Unknown) => {
            lines.push(field_styled(
                "type",
                "unknown".to_string(),
                Style::new().fg(Color::Red),
            ));
            lines.push(field_styled(
                "confidence",
                "no backend reached the threshold".to_string(),
                Style::new().dim(),
            ));
        }
        None => lines.push(field("type", "not detected yet".to_string())),
    }

    if let Some(detection) = detection {
        let source = match detection.source {
            DetectionSource::Automatic => "automatic",
            DetectionSource::Manual => "manual override",
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
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Why detection concluded what it did (`AGENTS.md` §4: explainable).
pub fn draw_detection(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane("Detection", Focus::Project, app);
    let Some(detection) = app.manager.detection() else {
        frame.render_widget(
            Paragraph::new("detection has not run yet".dim()).block(block),
            area,
        );
        return;
    };

    let mut lines = Vec::new();
    for score in &detection.scores {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", score.kind.display_name()),
                Style::new().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(
                confidence_bar(score.confidence),
                Style::new().fg(confidence_color(score.confidence)),
            ),
        ]));

        if score.signals.is_empty() {
            lines.push(Line::from("    no signals".dim()));
            continue;
        }
        for signal in &score.signals {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    +{:.2}  ", signal.weight),
                    Style::new().fg(Color::Cyan),
                ),
                Span::styled(signal.detail, Style::new().dim()),
            ]));
        }
    }

    if detection.scores.is_empty() {
        lines.push(Line::from(
            format!(
                "searched {} director{}",
                detection.searched.len(),
                plural(detection.searched.len())
            )
            .dim(),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// What the selected backend can do --- the UI's only source of actions.
pub fn draw_capabilities(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane("Capabilities", Focus::Capabilities, app);
    let supported = app.manager.capabilities();

    let items: Vec<ListItem> = Capability::ALL
        .iter()
        .map(|cap| {
            let available = supported.contains(*cap);
            let (mark, style) = if available {
                ("●", Style::new().fg(Color::Green))
            } else {
                ("○", Style::new().dim())
            };

            let mut spans = vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(
                    format!("{:<16}", cap.label()),
                    if available {
                        Style::new()
                    } else {
                        Style::new().dim()
                    },
                ),
            ];
            // Destructive operations are flagged wherever they appear (SPEC.md §15).
            if cap.is_destructive() {
                let warn = if available {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new().dim()
                };
                spans.push(Span::styled("confirm", warn));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(app.capability_cursor));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(ratatui::style::Modifier::REVERSED))
        .highlight_symbol("");

    frame.render_stateful_widget(list, area, &mut state);
}

/// Rolling status/log output.
pub fn draw_logs(frame: &mut Frame, area: Rect, app: &mut App) {
    let following = app.logs.is_following();
    let title = if following {
        format!("Log ({})", app.logs.len())
    } else {
        format!("Log ({}) ↑{}", app.logs.len(), app.logs.scroll())
    };
    let block = pane(&title, Focus::Logs, app);

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

    frame.render_widget(Paragraph::new(lines).block(block), area);
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

const fn plural(count: usize) -> &'static str {
    if count == 1 { "y" } else { "ies" }
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
