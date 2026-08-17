//! The row-2 panes' operation buttons, as a custom widget
//! (`https://ratatui.rs/examples/widgets/custom_widget/`, minus the
//! example's gradient). The buttons are stacked in one bordered group that
//! shares its top and bottom rules --- drawn in the theme's `muted` color,
//! quiet enough that the frame never competes with a selected row for
//! attention while still following the active theme --- one centered label
//! per row: N buttons cost N+2 lines, so a pane full of them still fits
//! vertically. Each button's icon label is bold in the theme's `fg` while
//! the action can run, `muted` while it waits for the checklist's answers.
//!
//! The selection highlight is `palette.selection`/`palette.fg` --- an
//! explicit, deterministic fill instead of `Modifier::REVERSED` (which this
//! widget used to rely on). `REVERSED` swaps whatever the terminal's own
//! default colors happen to be, on top of the same style's `bold`/`dim`
//! weight modifier --- a combination different terminals render with
//! different fidelity, sometimes leaving a selected row looking patchy
//! rather than a solid bar. An explicit theme color sidesteps that
//! entirely, and reads consistently with the rest of the themed UI
//! (`crate::ui::Palette`) besides. It is applied via
//! [`ratatui::buffer::Buffer::set_style`] over the row's inner cells
//! *after* the border and label are drawn (see [`highlight_selected`]) ---
//! one uniform pass over the whole row, rather than folded into the same
//! `Span` as the icon/label text, so the fill and the label's own weight
//! stay two independent concerns.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::{Palette, muted_style};

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

    /// The button's text color and weight: bold `fg` by readiness, `muted`
    /// while it waits. The selection highlight is a separate `set_style`
    /// patch (see [`ButtonStack::render`]), not part of this style.
    fn row_style(&self, palette: Palette) -> Style {
        if self.enabled {
            Style::new().fg(palette.fg).bold()
        } else {
            muted_style(palette)
        }
    }
}

/// A stack's full height: the top rule, one row per button, a divider
/// between each pair. Zero for no buttons (nothing is drawn then).
pub(super) fn stack_height(buttons: &[Button]) -> u16 {
    if buttons.is_empty() {
        0
    } else {
        2 * buttons.len() as u16 + 1
    }
}

/// One shared-border stack of [`Button`]s.
#[derive(Debug, Clone)]
pub struct ButtonStack {
    buttons: Vec<Button>,
    palette: Palette,
}

impl ButtonStack {
    /// The stack's full height: the top rule, one row per button, a
    /// divider between each pair.
    fn height(&self) -> u16 {
        stack_height(&self.buttons)
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
                let y = area.y + offset as u16;
                put(
                    buf,
                    area,
                    y,
                    Line::styled(
                        truncate(&button.label, width),
                        button.row_style(self.palette),
                    ),
                );
                let row = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                };
                highlight_selected(buf, area, row, button.selected, self.palette);
            }
            return;
        }
        let rule = "─".repeat(width - 2);
        let inner = width - 2;
        let frame = muted_style(self.palette);
        put(buf, area, area.y, Line::styled(format!("╭{rule}╮"), frame));
        let mut y = area.y + 1;
        let last = self.buttons.len() - 1;
        for (index, button) in self.buttons.iter().enumerate() {
            let label = truncate(&button.label, inner);
            let label_width = label.chars().count();
            let left = (inner - label_width) / 2;
            let right = inner - label_width - left;
            put(
                buf,
                area,
                y,
                Line::from(vec![
                    Span::styled("│", frame),
                    Span::styled(
                        format!("{}{}{}", " ".repeat(left), label, " ".repeat(right)),
                        button.row_style(self.palette),
                    ),
                    Span::styled("│", frame),
                ]),
            );
            // A `set_style` patch over the row's inner cells, done *after*
            // the label --- see the module doc for why this, and not a
            // styled `Span`, carries the selection color. Confined to
            // `x+1..width-1` so the side rules `│` never get painted over.
            let inner_row = Rect {
                x: area.x + 1,
                y,
                width: area.width.saturating_sub(2),
                height: 1,
            };
            highlight_selected(buf, area, inner_row, button.selected, self.palette);
            y += 1;
            // The divider between stacked buttons: never after the last
            // one, and in the same muted frame color as the outer rules ---
            // the selection highlight stays confined to a button's inner
            // row.
            if index < last {
                put(buf, area, y, Line::styled(format!("├{rule}┤"), frame));
                y += 1;
            }
        }
        put(buf, area, y, Line::styled(format!("╰{rule}╯"), frame));
    }
}

/// Patches `row`'s cells to the theme's selection colors when `selected`
/// --- a no-op otherwise, and clipped the same way [`put`] clips: nothing
/// past `area`'s bottom. See the module doc: this runs after the label is
/// drawn and overrides only fg/bg (`Cell::set_style` patches, it does not
/// touch the glyph or its weight), so it always lands as one uniform,
/// deterministic band regardless of what the label drew underneath.
fn highlight_selected(buf: &mut Buffer, area: Rect, row: Rect, selected: bool, palette: Palette) {
    if !selected || row.y >= area.bottom() {
        return;
    }
    buf.set_style(row, Style::new().bg(palette.selection).fg(palette.fg));
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
pub(super) fn render_stack(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    buttons: &[Button],
    palette: Palette,
) -> u16 {
    if buttons.is_empty() {
        return y;
    }
    let stack = ButtonStack {
        buttons: buttons.to_vec(),
        palette,
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

    fn palette() -> Palette {
        ratatui_themes::ThemeName::TokyoNight.palette()
    }

    /// The rendered stack, without TestBackend's quoting.
    fn render(width: u16, buttons: &[Button]) -> String {
        let stack = ButtonStack {
            buttons: buttons.to_vec(),
            palette: palette(),
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
        let palette = palette();
        let stack = ButtonStack {
            buttons: vec![
                Button::new("▶ Build").selected(true),
                Button::new("× Clean").selected(true),
            ],
            palette,
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(14, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let filled = |x: u16, y: u16| buf[(x, y)].bg == palette.selection;
        // The outer rules and the divider between the buttons keep the
        // frame color, never the selection fill.
        for y in [0, 2, 4] {
            for x in 0..14 {
                assert!(!filled(x, y), "rule cell ({x},{y}) must stay unfilled");
            }
        }
        // A selected button is a solid bar between the side rules: every
        // inner cell filled with the theme's selection color, the `│`
        // themselves untouched.
        for y in [1, 3] {
            assert!(!filled(0, y) && !filled(13, y), "side rule on row {y}");
            for x in 1..13 {
                assert!(filled(x, y), "inner cell ({x},{y}) should be filled");
                assert_eq!(buf[(x, y)].fg, palette.fg, "inner cell ({x},{y}) fg");
            }
        }
    }

    /// The color fill and the label's color are two independent passes
    /// now (`row_style` vs. [`highlight_selected`]) --- a disabled-but-
    /// selected button (waiting on the checklist, cursor parked on it
    /// anyway) must still get the full theme-colored bar. The selection
    /// patch owns the colors on a selected row; what survives from
    /// readiness is the *weight* --- bold when enabled, plain while it
    /// waits.
    #[test]
    fn a_disabled_selected_button_still_gets_the_full_fill() {
        use ratatui::style::Modifier;

        let palette = palette();
        let stack = ButtonStack {
            buttons: vec![
                Button::new("▶ Build").selected(true),
                Button::new("↻ Update Zephyr").enabled(false).selected(true),
            ],
            palette,
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        for (y, bold) in [(1, true), (3, false)] {
            for x in 1..19 {
                assert_eq!(buf[(x, y)].bg, palette.selection, "cell ({x},{y}) bg");
                assert_eq!(
                    buf[(x, y)].modifier.contains(Modifier::BOLD),
                    bold,
                    "cell ({x},{y}) weight"
                );
            }
        }
    }

    /// An enabled button reads in the theme's `fg`, a disabled one in its
    /// `muted` --- the weight difference the checklist gates, expressed in
    /// theme colors rather than the terminal's default dim.
    #[test]
    fn button_readiness_is_the_difference_between_fg_and_muted() {
        let palette = palette();
        let stack = ButtonStack {
            buttons: vec![
                Button::new("▶ Build"),
                Button::new("↻ Update Zephyr").enabled(false),
            ],
            palette,
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let label_cell = |x: u16, y: u16| &buf[(x, y)];
        assert_eq!(label_cell(9, 1).fg, palette.fg, "enabled button label");
        assert_eq!(label_cell(9, 3).fg, palette.muted, "disabled button label");
    }
}
