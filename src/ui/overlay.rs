//! Modal layers: help and manual backend selection.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, Overlay, PickerOption, View};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::Help => draw_help(frame, area, app),
        Overlay::BackendPicker { selected } => draw_picker(frame, area, app, selected),
        Overlay::DevicePicker { selected } => draw_device_picker(frame, area, app, selected),
    }
}

/// Serial device selection. Reached automatically when a scan finds more than
/// one board, because guessing would be the wrong kind of convenience.
fn draw_device_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let devices = app.devices.devices();

    if devices.is_empty() {
        let popup = centered(area, 52, 4);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("No MicroPython device found.".fg(Color::Yellow)),
                Line::from("Connect a board and press 'd' to scan again.".dim()),
            ])
            .block(modal("Device")),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let mut spans = vec![Span::raw(format!(" {} ", device.label()))];
            if app.devices.selected_index() == Some(index) {
                spans.push(Span::styled("(active)", Style::new().dim()));
            }
            spans.push(Span::styled(
                format!("  {}", device.vid_pid),
                Style::new().fg(Color::Cyan),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 64, devices.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Device"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    // Help follows the screen: listing dashboard keys while browsing files
    // would describe bindings that do nothing.
    let bindings: &[(&str, &str)] = match app.view {
        View::Dashboard => &[
            ("tab / shift+tab", "move focus between panes"),
            ("↑ ↓ / k j", "navigate inside the focused pane"),
            ("page up/down", "scroll the log by one screen"),
            ("home / end", "jump to start / end"),
            ("r", "re-run project detection"),
            ("o", "override the detected backend"),
            ("f", "open the device file browser"),
            ("?", "toggle this help"),
            ("q / esc / ctrl+c", "quit"),
        ],
        View::Files => &[
            ("tab", "switch between local and device"),
            ("↑ ↓ / k j", "move the cursor"),
            ("enter / →", "enter the directory"),
            ("backspace / ←", "go to the parent directory"),
            ("r", "reload the focused pane"),
            ("c", "compare contents by sha256"),
            ("h", "show or hide dot-files"),
            ("d", "scan for devices / choose one"),
            ("q / esc", "back to the dashboard"),
            ("ctrl+c", "quit"),
        ],
    };

    let mut lines = vec![Line::from("Keyboard".bold()), Line::from("")];
    lines.extend(bindings.iter().map(|(key, description)| {
        Line::from(vec![
            Span::styled(format!("  {key:<18}"), Style::new().fg(Color::Cyan)),
            Span::raw(*description),
        ])
    }));

    let popup = centered(area, 56, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(modal("Help")), popup);
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let options = PickerOption::all();
    let active = app.manager.override_kind();

    let items: Vec<ListItem> = options
        .iter()
        .map(|option| {
            let current = match option {
                PickerOption::Automatic => active.is_none(),
                PickerOption::Backend(kind) => active == Some(*kind),
            };
            let mut spans = vec![Span::raw(format!(" {} ", option.label()))];
            if current {
                spans.push(Span::styled("(active)", Style::new().dim()));
            }
            // Detection's own opinion, so an override is an informed choice.
            if let PickerOption::Backend(kind) = option
                && let Some(detection) = app.manager.detection()
            {
                spans.push(Span::styled(
                    format!("  {:.2}", detection.confidence_of(*kind)),
                    Style::new().fg(Color::Cyan),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 48, options.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Backend"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

fn modal(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
}

/// Centers a `width`x`height` box inside `area`, shrinking to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    popup
}
