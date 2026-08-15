//! The row-2 panes' operation buttons, as a custom widget
//! (`https://ratatui.rs/examples/widgets/custom_widget/`, minus the
//! example's gradient --- `AGENTS.md` asks for terminal-native colors
//! rather than an imposed theme, so the widget speaks only in
//! modifiers). The buttons are stacked in one bordered group that shares
//! its top and bottom rules, one centered label per row: N buttons cost
//! N+2 lines, so a pane full of them still fits vertically. Each
//! button's icon label is bold while the action can run, dim while it
//! waits for the checklist's answers --- and the selection highlight is
//! *internal*: reversed fills the button's whole inner row from side
//! rule to side rule, a solid bar that never paints over the group's
//! frame.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

#[derive(Debug, Clone)]
pub struct Button {
    label: String,
    enabled: bool,
    selected: bool,
}

impl Button {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            selected: false,
        }
    }

    /// Whether `Enter` runs the action: bold says yes, dim says "still
    /// waiting for the checklist".
    pub fn enabled(mut self, yes: bool) -> Self {
        self.enabled = yes;
        self
    }

    /// Whether the cursor sits on this button.
    pub fn selected(mut self, yes: bool) -> Self {
        self.selected = yes;
        self
    }

    /// The button's content style: bold/dim by readiness, reversed only
    /// when selected --- the group's frame stays unstyled either way.
    fn row_style(&self) -> Style {
        let style = if self.enabled {
            Style::new().bold()
        } else {
            Style::new().dim()
        };
        if self.selected {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        }
    }
}

/// One shared-border stack of [`Button`]s.
#[derive(Debug, Clone, Default)]
pub struct ButtonStack {
    buttons: Vec<Button>,
}

impl ButtonStack {
    /// The stack's full height: the top rule, one row per button, a
    /// divider between each pair.
    fn height(&self) -> u16 {
        if self.buttons.is_empty() {
            0
        } else {
            2 * self.buttons.len() as u16 + 1
        }
    }
}

impl Widget for ButtonStack {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.buttons.is_empty() {
            return;
        }
        let width = area.width as usize;
        if width < 4 {
            // No room for a box: one bare label per row, clipped by the
            // buffer like any other row.
            for (offset, button) in self.buttons.iter().enumerate() {
                put(
                    buf,
                    area,
                    area.y + offset as u16,
                    Line::styled(truncate(&button.label, width), button.row_style()),
                );
            }
            return;
        }
        let rule = "─".repeat(width - 2);
        let inner = width - 2;
        put(buf, area, area.y, Line::raw(format!("╭{rule}╮")));
        let mut y = area.y + 1;
        let last = self.buttons.len() - 1;
        for (index, button) in self.buttons.iter().enumerate() {
            let label = truncate(&button.label, inner);
            let label_width = label.chars().count();
            let left = (inner - label_width) / 2;
            let right = inner - label_width - left;
            // The button's style reaches only its inner cells: the side
            // rules stay plain whatever the state, so a selected button
            // fills its row completely --- edge to edge between the rules
            // --- without ever painting over them.
            put(
                buf,
                area,
                y,
                Line::from(vec![
                    Span::raw("│"),
                    Span::styled(
                        format!("{}{}{}", " ".repeat(left), label, " ".repeat(right)),
                        button.row_style(),
                    ),
                    Span::raw("│"),
                ]),
            );
            y += 1;
            // The divider between stacked buttons: never after the last
            // one, and unstyled like the outer rules --- the selection
            // highlight stays confined to a button's inner row.
            if index < last {
                put(buf, area, y, Line::raw(format!("├{rule}┤")));
                y += 1;
            }
        }
        put(buf, area, y, Line::raw(format!("╰{rule}╯")));
    }
}

/// Writes one row at `y` unless it falls past the area's bottom.
fn put(buf: &mut Buffer, area: Rect, y: u16, line: Line<'_>) {
    if y >= area.bottom() {
        return;
    }
    line.render(
        Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        },
        buf,
    );
}

/// Keeps at most `max` columns of `text` (the labels are single-width
/// glyphs by policy, so chars are columns).
fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Renders the buttons as one stacked group at `y` and returns the next
/// row's y. The full height is consumed even when the pane clips the
/// rows, so callers measuring their content agree with what is drawn.
pub(super) fn render_stack(frame: &mut Frame, area: Rect, y: u16, buttons: &[Button]) -> u16 {
    if buttons.is_empty() {
        return y;
    }
    let stack = ButtonStack {
        buttons: buttons.to_vec(),
    };
    let height = stack.height();
    frame.render_widget(
        stack,
        Rect {
            x: area.x,
            y,
            width: area.width,
            height: height.min(area.bottom().saturating_sub(y)),
        },
    );
    y + height
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered stack, without TestBackend's quoting.
    fn render(width: u16, buttons: &[Button]) -> String {
        let stack = ButtonStack {
            buttons: buttons.to_vec(),
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        terminal
            .backend()
            .to_string()
            .lines()
            .map(|line| line.trim_matches('"'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn buttons_stack_in_one_shared_border_group_with_dividers() {
        assert_eq!(
            render(20, &[Button::new("▶ Build"), Button::new("× Clean")]),
            "╭──────────────────╮\n\
             │     ▶ Build      │\n\
             ├──────────────────┤\n\
             │     × Clean      │\n\
             ╰──────────────────╯"
        );
    }

    #[test]
    fn a_long_label_is_clipped_to_the_group() {
        assert_eq!(
            render(10, &[Button::new("⟳ Rebuilding")]),
            "╭────────╮\n│⟳ Rebuil│\n╰────────╯"
        );
    }

    #[test]
    fn a_selected_button_fills_its_inner_row_without_touching_the_rules() {
        let stack = ButtonStack {
            buttons: vec![
                Button::new("▶ Build").selected(true),
                Button::new("× Clean").selected(true),
            ],
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(14, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let reversed = |x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        // The outer rules and the divider between the buttons stay plain.
        for y in [0, 2, 4] {
            for x in 0..14 {
                assert!(!reversed(x, y), "rule cell ({x},{y}) must stay plain");
            }
        }
        // A selected button is a solid bar between the side rules: every
        // inner cell reversed, the `│` themselves untouched.
        for y in [1, 3] {
            assert!(!reversed(0, y) && !reversed(13, y), "side rule on row {y}");
            for x in 1..13 {
                assert!(reversed(x, y), "inner cell ({x},{y}) should be filled");
            }
        }
    }
}
