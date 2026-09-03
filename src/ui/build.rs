//! Project-panel rendering (the build lifecycle's pane, titled "Project
//! actions"): the project checklist (`Project path`, then `Board`) over
//! a horizontal separator and the lifecycle buttons (`crate::ui::button`),
//! dimmed until both answers exist, with the command state pinned to the
//! pane's last line. Selected rows highlight full-width. While a command
//! runs, the pane's footer sits directly under the stack: the state on the
//! left half, `Stop` as its own half-width button box on the right --- side
//! by side, never one pushing the other, and never a row of the stack. The
//! footer's three rows are reserved even while idle, so the pane's height
//! never changes when a command starts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::button::{self, Button};
use super::workspace::label;
use crate::app::{App, Focus};
use crate::build::{BuildPanel, BuildReport};
use crate::ui::{
    Palette, dashboard_focused, numbered_title, pane_block, render_pane, shortcut_letter,
};

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(panel) = &app.build else {
        return;
    };
    let focused = dashboard_focused(app, Focus::Build);
    let title = numbered_title(app, Focus::Build, app.icon_set().bolt(), "Actions");
    let block = pane_block(&title, focused, palette, shortcut_letter(app, 'a'));
    let inner = render_pane(frame, area, block, focused, palette);

    let footer_top = draw_rows(frame, inner, app, panel, palette);
    draw_state(frame, inner, panel, footer_top, palette);
}

/// The command state: the live counter while a command runs, the last
/// result once it finishes, never built before the first one. It rides
/// the reserved footer's label row (see [`draw_rows`]) on the same line
/// whether or not the `Stop` box is showing --- beside the box, on the
/// left half, while one runs; full-width over the empty reservation
/// otherwise. Nothing in the pane moves when a command starts or ends.
fn draw_state(
    frame: &mut Frame,
    area: Rect,
    panel: &BuildPanel,
    footer_top: u16,
    palette: Palette,
) {
    if area.height < 2 || footer_top + 1 >= area.bottom() {
        return;
    }
    // The state shares its row with the `Stop` box while a command runs and
    // owns the whole row otherwise.
    let width = if panel.is_busy() {
        button::footer_split(area.width).0
    } else {
        area.width
    };
    let line = if let Some(elapsed) = panel.elapsed() {
        // The tool's own progress (ninja's step counter, or an esptool
        // percentage from a runner that shells out to it) --- the
        // command's label instead of the generic "running", since the
        // count alone does not say what it counts. Without one (and the
        // dashboard deliberately never adopts progress, see
        // `BuildPanel::on_process`), the elapsed counter takes that place.
        //
        // The label and the counter are what carry meaning here; the word
        // "running" carried none, because the counter only ever ticks while
        // something runs and the Monitor strip is already spinning a
        // spinner. It cost eight columns of a line that has few
        // (`button::footer_split`), and those columns were coming out of
        // the counter.
        let name = panel.running_label().unwrap_or("command");
        let text = match panel.progress() {
            Some(progress) => format!("{name} · {}", progress.render()),
            None => format!("{name} · {}", BuildPanel::secs(elapsed)),
        };
        Line::from(vec![label("state", palette), text.fg(palette.accent)])
    } else if let Some(report) = &panel.last {
        report_line(report, palette, width)
    } else {
        Line::from(vec![
            label("state", palette),
            "never built".fg(palette.muted),
        ])
    };
    let rect = Rect {
        y: footer_top + 1,
        width,
        ..area
    };
    frame.render_widget(Paragraph::new(line), rect);
}

fn report_line(report: &BuildReport, palette: Palette, width: u16) -> Line<'static> {
    // A host build is the one thing the pane cannot otherwise say: the
    // checklist names the project's board either way, and the action stack
    // is the same six rows. So the report names it, and stops naming it the
    // moment the next build replaces the report.
    let what = if report.simulator {
        format!("{} (simulator)", report.what)
    } else {
        report.what.to_string()
    };
    let what = what.as_str();
    // Three outcomes, and three marks: a command the user stopped is not a
    // failure --- stopping is exactly what was asked for --- but it is not a
    // success either, and giving it the success check made the two
    // indistinguishable at a glance, which is the only way this line is ever
    // read. `◼` in the warning color says "ended early, by choice" without
    // claiming either. `ok` itself stays `false`: nothing was completed, and
    // the cursor logic must not treat a stop as a green light for Flash.
    //
    // The duration comes back with it. "How far did it get before I stopped
    // it" is a real question --- and it was the one outcome of the three
    // dropping the answer.
    let (mark, style, outcome) = if report.cancelled {
        (
            "◼",
            ratatui::style::Style::new().fg(palette.warning),
            format!("{what} stopped after {}", BuildPanel::secs(report.duration)),
        )
    } else if report.ok {
        (
            "✓",
            ratatui::style::Style::new().fg(palette.success),
            format!("{what} ok in {}", BuildPanel::secs(report.duration)),
        )
    } else {
        (
            "✗",
            ratatui::style::Style::new().fg(palette.error),
            format!("{what} failed"),
        )
    };
    // The clock is the line's least load-bearing part --- what happened
    // outranks the minute it happened --- so when the row cannot hold both
    // it is dropped whole rather than truncated. A half-written `10:2` reads
    // as a bug; no clock reads as no clock. Same rule the Device Info pane's
    // chip line follows with its crystal/revision suffixes, and
    // `esptool::features::compact` with its tail entries.
    let head = vec![
        label("last", palette),
        Span::styled(format!("{mark} {outcome}"), style),
    ];
    let clock = format!(" {:02}:{:02}", report.at.hour(), report.at.minute());
    let used: usize = head.iter().map(|span| span.content.chars().count()).sum();
    if used + clock.chars().count() > usize::from(width) {
        return Line::from(head);
    }
    let mut spans = head;
    spans.push(clock.fg(palette.muted));
    Line::from(spans)
}

/// The pane's rows: the operation buttons stacked in one shared-border
/// group --- bold when both checklist answers (asked in the workspace
/// pane) exist, dim while they wait, the selection highlighting only the
/// button's own row --- and the three-row footer under the stack's bottom
/// rule, reserved whether or not a command runs: `Stop` as its own
/// half-width box on the right half while one does (invisible otherwise,
/// its space waiting), the state line on the left half. Reserving the
/// rows is what keeps the pane's height --- and every row's place ---
/// constant when a command starts or ends. Returns the footer's top row.
fn draw_rows(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    panel: &BuildPanel,
    palette: Palette,
) -> u16 {
    let caps = app.manager.capabilities();
    let actions = panel.actions(&caps);
    // `Stop` trails the list exactly while a command runs; the stack shows
    // the buttons above it, and `Stop` itself becomes the footer's box.
    let stop = panel.is_busy() && matches!(actions.last(), Some(crate::build::BuildAction::Stop));
    let icons = app.icon_set();
    let y = area.y;
    let mut buttons: Vec<Button> = Vec::new();
    let mains = &actions[..actions.len() - usize::from(stop)];
    for (position, action) in mains.iter().enumerate() {
        let selected = panel.cursor == position;
        match action {
            crate::build::BuildAction::Build(kind) => buttons.push(
                Button::new(kind.label())
                    .icon(kind_icon(*kind, icons), kind_icon_color(*kind, palette))
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Flash => buttons.push(
                Button::new("Flash")
                    .icon(icons.flash(), palette.warning)
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Menuconfig => buttons.push(
                Button::new("Menuconfig")
                    .icon(icons.pencil(), palette.info)
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::UpdateZephyr => buttons.push(
                Button::new("Zephyr Actions")
                    .icon(icons.cogs(), palette.secondary)
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::InstallZephyr => buttons.push(
                Button::new("Install Zephyr")
                    .icon(icons.download(), palette.success)
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Stop => unreachable!("Stop is drawn as the footer box"),
            // Reached through the Zephyr Actions menu, never a panel row
            // (see `BuildAction::Dashboard`).
            crate::build::BuildAction::Dashboard => {
                unreachable!("Dashboard is drawn in the Zephyr Actions menu")
            }
            crate::build::BuildAction::Run => {
                unreachable!("the simulator run follows its own build, from no row")
            }
            crate::build::BuildAction::SizeReport => {
                unreachable!("the memory report is started from the dashboard window")
            }
        }
    }
    // The footer sits directly under the stack's bottom rule --- no blank
    // row between Flash and Stop --- unless the pane is too short for
    // both, when it pins to the bottom instead and the stack clips above
    // it (the box carries the cursor while a command runs; a clipped
    // button row comes back when the command ends).
    let stack_end = y + button::stack_height(&buttons);
    let footer_top = stack_end.min(area.bottom().saturating_sub(3)).max(area.y);
    let stack_area = Rect {
        height: footer_top.saturating_sub(area.y),
        ..area
    };
    button::render_stack(frame, stack_area, y, &buttons, palette);
    if stop {
        // The right end of the footer: the same stacked-button widget, one
        // button of its own, sharing its label row with the state. Its
        // width is fixed --- see `button::footer_split`, which the state
        // line reads from the other side.
        let (state, stop_width) = button::footer_split(area.width);
        let corner = Rect {
            x: area.x + state,
            width: stop_width,
            y: footer_top,
            height: area.bottom().saturating_sub(footer_top),
        };
        let selected = panel.cursor == actions.len() - 1;
        button::render_stack(
            frame,
            corner,
            footer_top,
            &[Button::new("Stop")
                .icon(icons.stop(), palette.warning)
                .selected(selected)],
            palette,
        );
    }
    footer_top
}

/// The button glyph for a lifecycle kind: one glance tells the actions
/// apart, the way `Stop`'s `■` already does. Which rendering the glyph
/// comes from is the active [`IconSet`](crate::icons::IconSet).
fn kind_icon(kind: crate::backend::BuildKind, icons: crate::icons::IconSet) -> &'static str {
    match kind {
        crate::backend::BuildKind::Build => icons.play(),
        crate::backend::BuildKind::Clean => icons.clean(),
        crate::backend::BuildKind::Rebuild => icons.rebuild(),
    }
}

/// The glyph's color: `accent` for the primary action, `warning` for the
/// destructive-confirmed one (previewing the confirm screen's own target
/// color), `secondary` for the less-prominent variant.
fn kind_icon_color(kind: crate::backend::BuildKind, palette: Palette) -> Color {
    match kind {
        crate::backend::BuildKind::Build => palette.accent,
        crate::backend::BuildKind::Clean => palette.warning,
        crate::backend::BuildKind::Rebuild => palette.secondary,
    }
}
