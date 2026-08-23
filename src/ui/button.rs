//! The row-2 panes' operation buttons, as a custom widget
//! (`https://ratatui.rs/examples/widgets/custom_widget/`, minus the
//! example's gradient). The buttons are stacked in one bordered group that
//! shares its top and bottom rules --- drawn in the theme's `muted` color,
//! quiet enough that the frame never competes with a selected row for
//! attention while still following the active theme --- one left-aligned
//! label per row, one indent in, so the icons line up as a single column
//! down the stack whatever the label lengths, with a divider between each
//! pair: N buttons cost 2N+1 lines, so
//! a pane full of them still fits vertically. Each button's icon label is
//! bold in the theme's `fg` while the action can run, `muted` while it
//! waits for the checklist's answers.
//!
//! A button may also carry a muted second line ([`Button::detail`]), which
//! makes it two rows tall. Nothing in a *pane* uses that --- their rows stay
//! bare, per `SPEC.md` §15 --- it is for a menu, where the reader is choosing
//! between actions rather than recognising one. [`stack_height`] therefore
//! sums the buttons' rows rather than counting the buttons.
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
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::{Palette, muted_style};

/// How far a button's label row is indented, in columns: one, so the
/// icon does not sit flush against the frame's rule.
const LABEL_INDENT: usize = 1;

/// How far its detail row is indented: past that leading space and the
/// icon's own column-plus-gap, so the second line starts under the first
/// line's *text* rather than under its icon. A detailed button's label is
/// expected to read `<icon>  <label>` for the two to line up.
const DETAIL_INDENT: usize = LABEL_INDENT + 3;

#[derive(Debug, Clone)]
pub struct Button {
    label: String,
    /// A muted second line under the label (see [`Button::detail`]). `None`
    /// --- every button in a pane's stack --- costs one row, exactly as
    /// before.
    detail: Option<String>,
    /// A leading glyph carrying its own color, independent of the label's
    /// bold/muted weight (see [`Button::icon`]). `None` renders the label as
    /// one plain span, exactly as before this field existed.
    icon: Option<(&'static str, Color)>,
    enabled: bool,
    selected: bool,
}

impl Button {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            icon: None,
            enabled: true,
            selected: false,
        }
    }

    /// A leading glyph, colored independently of the label text next to it
    /// --- the same split the file browser's sync-status markers and the
    /// header's backend icon use (a colored `Span`, then a plain-styled text
    /// `Span`), rather than baking the glyph into the label string. `glyph`
    /// is expected to be exactly one column (the crate's single-width-glyph
    /// policy --- see [`truncate`]'s doc comment), or the width math below
    /// silently miscounts. The one exception is the empty string --- the
    /// `none` icon set's way of saying "no glyph" --- which keeps the
    /// button's geometry exactly as it is (the column stays, blank) so a
    /// switch of icon sets never shifts a label or a detail line.
    pub fn icon(mut self, glyph: &'static str, color: Color) -> Self {
        self.icon = Some((glyph, color));
        self
    }

    /// Adds a muted second line explaining the action --- what it does, or
    /// the literal command it runs. A button with a detail costs two rows;
    /// it is for a menu, where the reader is choosing between actions
    /// rather than recognising one they already know.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// How many rows this button occupies.
    fn rows(&self) -> u16 {
        if self.detail.is_some() { 2 } else { 1 }
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

    /// The icon glyph's color: its own `icon()` color while the action can
    /// run, `muted` while it waits --- so a disabled row reads as uniformly
    /// dimmed rather than a grey label under a still-vivid icon.
    fn icon_style(&self, palette: Palette) -> Style {
        match (self.enabled, self.icon) {
            (true, Some((_, color))) => Style::new().fg(color),
            _ => muted_style(palette),
        }
    }
}

/// A stack's full height: the two outer rules, each button's rows, and a
/// divider between each pair. Zero for no buttons (nothing is drawn then).
///
/// Buttons are summed rather than counted because a detailed one takes two
/// rows ([`Button::detail`]); for a stack of plain buttons this is still
/// `2N + 1`, which is the number `ui::MIN_HEIGHT` was measured against.
pub(super) fn stack_height(buttons: &[Button]) -> u16 {
    if buttons.is_empty() {
        return 0;
    }
    let rows: u16 = buttons.iter().map(Button::rows).sum();
    let dividers = buttons.len() as u16 - 1;
    rows + dividers + 2
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
                let content = icon_content(button.icon.map(|(glyph, _)| glyph), &button.label);
                let body = truncate(&content, width);
                let spans = icon_body_spans(
                    body,
                    0,
                    button.icon.is_some(),
                    button.icon_style(self.palette),
                    button.row_style(self.palette),
                );
                put(buf, area, y, Line::from(spans));
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
            let top = y;
            // Left-aligned, one indent in, whether the label stands alone
            // or carries a detail under it: the icons line up as one column
            // down the stack instead of scattering with the label lengths.
            let icon_glyph = button.icon.map(|(glyph, _)| glyph);
            let content = icon_content(icon_glyph, &button.label);
            let truncated = truncate(&content, inner.saturating_sub(LABEL_INDENT));
            let body = pad_left(&format!("{}{truncated}", " ".repeat(LABEL_INDENT)), inner);
            let icon_at = LABEL_INDENT;
            let mut spans = vec![Span::styled("│", frame)];
            spans.extend(icon_body_spans(
                body,
                icon_at,
                button.icon.is_some(),
                button.icon_style(self.palette),
                button.row_style(self.palette),
            ));
            spans.push(Span::styled("│", frame));
            put(buf, area, y, Line::from(spans));
            y += 1;
            if let Some(detail) = &button.detail {
                let text = truncate(detail, inner.saturating_sub(DETAIL_INDENT));
                put(
                    buf,
                    area,
                    y,
                    Line::from(vec![
                        Span::styled("│", frame),
                        Span::styled(
                            pad_left(&format!("{}{text}", " ".repeat(DETAIL_INDENT)), inner),
                            muted_style(self.palette),
                        ),
                        Span::styled("│", frame),
                    ]),
                );
                y += 1;
            }
            // A `set_style` patch over the button's inner cells, done
            // *after* the text --- see the module doc for why this, and not
            // a styled `Span`, carries the selection color. Confined to
            // `x+1..width-1` so the side rules `│` never get painted over,
            // and spanning both rows so a detailed button highlights whole.
            let inner_row = Rect {
                x: area.x + 1,
                y: top,
                width: area.width.saturating_sub(2),
                height: y - top,
            };
            highlight_selected(buf, area, inner_row, button.selected, self.palette);
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
    // A detailed button is two rows tall and the pane may have room for
    // only the first, so the patch is clipped like `put` clips.
    let row = Rect {
        height: row.height.min(area.bottom() - row.y),
        ..row
    };
    buf.set_style(row, Style::new().bg(palette.selection).fg(palette.fg));
}

/// `text` at the left of `width` columns, padded out on the right so the
/// row still paints edge to edge (which is what lets the selection band
/// cover it whole).
fn pad_left(text: &str, width: usize) -> String {
    let used = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

/// The label text to lay out and measure: `<icon>  <label>` (two spaces,
/// one breathing column between the glyph's color and the text) when an
/// icon is set, `label` alone otherwise. That two-space gap is what
/// [`DETAIL_INDENT`] counts past the icon so a detail line starts under
/// the label's *text*.
fn icon_content(icon: Option<&str>, label: &str) -> String {
    match icon {
        Some(glyph) => format!("{glyph}  {label}"),
        None => label.to_string(),
    }
}

/// Splits an already truncated/padded/centered `body` into spans: the
/// character at `icon_at` styled `icon_style`, everything else styled
/// `label_style`. `has_icon` is `false` for a button with no [`Button::icon`]
/// set, in which case `body` renders as one plain span exactly as it did
/// before this splitting existed --- `icon_at` only means something once an
/// icon is actually there to find at that column.
fn icon_body_spans(
    body: String,
    icon_at: usize,
    has_icon: bool,
    icon_style: Style,
    label_style: Style,
) -> Vec<Span<'static>> {
    if !has_icon || icon_at >= body.chars().count() {
        return vec![Span::styled(body, label_style)];
    }
    let mut chars = body.chars();
    let before: String = (&mut chars).take(icon_at).collect();
    let icon_char = chars.next().expect("icon_at < body's char count");
    let after: String = chars.collect();
    let mut spans = Vec::with_capacity(3);
    if !before.is_empty() {
        spans.push(Span::styled(before, label_style));
    }
    spans.push(Span::styled(icon_char.to_string(), icon_style));
    if !after.is_empty() {
        spans.push(Span::styled(after, label_style));
    }
    spans
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

/// The `Stop` box's width in the footer of a pane that runs commands.
///
/// Fixed, not a share of the pane. `■ Stop` is six columns and the box adds
/// two rules, so eleven is already roomy --- while the *other* half of that
/// footer carries the live state line, which is the half that actually
/// needs room and the one that was silently losing it. Thirteen keeps the
/// box visually the same as the half-width one at a typical width and stops
/// it from eating half the pane at [`crate::ui::MIN_WIDTH`].
pub(crate) const STOP_BOX_WIDTH: u16 = 13;

/// Splits a command pane's footer into `(state, stop)` widths.
///
/// The two used to be `width / 2` each, computed separately in the build
/// pane and the flash pane (four places). That was fine while the state
/// line read `running · 12.4s`, and stopped being fine when it grew the
/// command's name: at `MIN_WIDTH` the Actions pane's half is 19 columns,
/// `state ` takes 6, and `Dashboard running · 12.4s` rendered as
/// `Dashboard run` --- the live counter, the one thing saying a long build
/// is still alive, gone. `Stop`'s needs are fixed and small, so it takes a
/// fixed box and the state keeps the rest.
///
/// A pane too narrow for both gives everything to `Stop`: the button has to
/// stay reachable, and a state line with no room is only a truncation.
pub(super) fn footer_split(width: u16) -> (u16, u16) {
    let stop = STOP_BOX_WIDTH.min(width);
    (width - stop, stop)
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
        // The icon comes from `Button::icon` (as every pane's rows do), whose
        // two-space gap to the label is part of the widget, not the string.
        let palette = palette();
        assert_eq!(
            render(
                20,
                &[
                    Button::new("Build").icon("▶", palette.fg),
                    Button::new("Clean").icon("×", palette.fg),
                ]
            ),
            "╭──────────────────╮\n\
             │ ▶  Build         │\n\
             ├──────────────────┤\n\
             │ ×  Clean         │\n\
             ╰──────────────────╯"
        );
    }

    #[test]
    fn a_detail_adds_a_muted_second_line_under_the_labels_text() {
        // The detail starts under the label's *text*, not under its icon
        // --- so the icon column and the text column each stay one line.
        assert_eq!(
            render(
                34,
                &[
                    Button::new("↻  Update").detail("west update"),
                    Button::new("▦  Dashboard").detail("west build -t dashboard"),
                ]
            ),
            "╭────────────────────────────────╮\n\
             │ ↻  Update                      │\n\
             │    west update                 │\n\
             ├────────────────────────────────┤\n\
             │ ▦  Dashboard                   │\n\
             │    west build -t dashboard     │\n\
             ╰────────────────────────────────╯"
        );
    }

    #[test]
    fn a_plain_stack_still_costs_two_rows_per_button_plus_one() {
        // `ui::MIN_HEIGHT` was measured against this number, so a detailed
        // button must not change what a pane's stack costs. Zephyr's six
        // buttons are the case that sized the constant.
        let plain: Vec<Button> = (0..6).map(|_| Button::new("▶ Build")).collect();
        assert_eq!(stack_height(&plain), 13);
        assert_eq!(stack_height(&[]), 0);
        // And a detailed one costs exactly one row more than it used to.
        let detailed = [
            Button::new("↻  Update").detail("west update"),
            Button::new("▦  Dashboard").detail("west build -t dashboard"),
        ];
        assert_eq!(stack_height(&detailed), 7);
    }

    #[test]
    fn the_selection_band_covers_both_rows_of_a_detailed_button() {
        // A band over the label alone would split the button in two --- the
        // detail is part of the thing the cursor is on.
        let palette = palette();
        let buttons = [
            Button::new("↻  Update").detail("west update"),
            Button::new("▦  Dashboard")
                .detail("west build -t dashboard")
                .selected(true),
        ];
        let stack = ButtonStack {
            buttons: buttons.to_vec(),
            palette,
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // Rows 4 and 5 are the selected button's label and detail.
        for y in [4, 5] {
            assert_eq!(
                buf[(2, y)].bg,
                palette.selection,
                "row {y} of the selected button must carry the band"
            );
        }
        // The divider above it, and the unselected button, must not.
        for y in [1, 2, 3] {
            assert_ne!(
                buf[(2, y)].bg,
                palette.selection,
                "row {y} is not the selected button"
            );
        }
        // The side rules stay unpainted.
        assert_ne!(buf[(0, 4)].bg, palette.selection, "the left rule");
        assert_ne!(buf[(33, 4)].bg, palette.selection, "the right rule");
    }

    #[test]
    fn a_long_label_is_clipped_to_the_group() {
        assert_eq!(
            render(10, &[Button::new("Rebuilding").icon("⟳", palette().fg)]),
            "╭────────╮\n│ ⟳  Rebu│\n╰────────╯"
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

    /// [`Button::icon`] colors only the glyph --- the label text next to it
    /// keeps `row_style`'s normal `fg`/bold, the same split the file
    /// browser's colored sync-status marker and plain name already use.
    #[test]
    fn an_icon_carries_its_own_color_independent_of_the_label() {
        let palette = palette();
        let stack = ButtonStack {
            buttons: vec![Button::new("Clean").icon("×", palette.warning)],
            palette,
        };
        let height = stack.height();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(stack, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let icon_x = (0..20)
            .find(|&x| buf[(x, 1)].symbol() == "×")
            .expect("the icon cell is drawn");
        assert_eq!(buf[(icon_x, 1)].fg, palette.warning, "icon cell color");
        let label_x = (0..20)
            .find(|&x| buf[(x, 1)].symbol() == "C")
            .expect("the label cell is drawn");
        assert_eq!(
            buf[(label_x, 1)].fg,
            palette.fg,
            "label cell keeps the normal fg"
        );
    }

    /// A disabled button's icon mutes along with its label --- a still-vivid
    /// icon on an otherwise-dimmed row would read as broken, not as "waiting
    /// for the checklist".
    #[test]
    fn a_disabled_buttons_icon_mutes_with_its_label() {
        let palette = palette();
        let stack = ButtonStack {
            buttons: vec![
                Button::new("Clean")
                    .icon("×", palette.warning)
                    .enabled(false),
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
        let icon_x = (0..20)
            .find(|&x| buf[(x, 1)].symbol() == "×")
            .expect("the icon cell is drawn");
        assert_eq!(
            buf[(icon_x, 1)].fg,
            palette.muted,
            "a disabled icon mutes instead of keeping its color"
        );
    }
}
