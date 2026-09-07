//! The OTA modal.
//!
//! The installer's modal grammar applied to a project's over-the-air flow
//! ([`super::install`]'s twin): sections stacked in one near-full-screen
//! dialog --- **target** (project, board, image, address), the
//! **requirements** the machine must have, the **prepare** steps the
//! instrumentation writes, the **update** stages the driver declared, and
//! the **output** of whatever is running --- over the same reserved
//! three-row footer (state line left, the one action button boxed right),
//! so nothing ever reflows when a stage starts or ends.
//!
//! Prepare and stage rows share the panes' checklist grammar
//! ([`super::workspace::marked_row`]); a stage row's value is the literal
//! `smpmgr` command, muted --- what runs is never hidden behind a friendly
//! label (`SPEC.md` §15).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::ota::prepare::{Requirement, SlotCheck, Step, ToolProbe, TransportCheck};
use crate::ota::update::{OtaAction, OtaPanel};
use crate::stepper::{Phase, StepState};

use super::button::{self, Button};
use super::workspace::{RowMark, marked_row};
use super::{Palette, SPINNER, muted_style, tilde_path};

/// The label column on both checklists (`board Kconfig fragment` is the
/// longest), wider than the panes' 13 --- these are phrases.
const LABEL: usize = 21;
/// Rows the target section always occupies: project, image, address.
const TARGET_ROWS: u16 = 3;

/// Rows the sections above the output occupy at this panel's list lengths,
/// so the output area is whatever is left rather than a guess. Hand-counted
/// per modal (the reason this file is a copy rather than a parameterisation
/// of the installer's); the two list lengths are the driver's and the
/// prepare flow's data.
fn fixed_rows(panel: &OtaPanel) -> u16 {
    // target, blank, heading, one requirement, the slot row, the transport
    // row, blank, heading, the prepare steps, blank, heading, the driver's
    // stages, blank, output heading.
    TARGET_ROWS
        + 1
        + 1
        + 1
        + 1
        + 1
        + 1
        + 1
        + panel.prepare.steps.len() as u16
        + 1
        + 1
        + panel.stages.len() as u16
        + 1
        + 1
}

/// The dialog fills the screen bar a margin, like the installer's.
pub(crate) fn area(body: Rect) -> Rect {
    super::layout::wide_modal(body)
}

/// Rows of output the modal can show at `body`'s size --- published so the
/// key handler's page scrolling matches what is drawn, the same contract
/// the installer's `output_viewport` keeps.
pub fn output_viewport(body: Rect, panel: &OtaPanel) -> usize {
    let popup = area(body);
    usize::from(
        popup
            .height
            .saturating_sub(2)
            .saturating_sub(fixed_rows(panel))
            .saturating_sub(3),
    )
}

pub(super) fn draw(
    frame: &mut Frame,
    body: Rect,
    panel: &OtaPanel,
    home: &std::path::Path,
    ticks: u64,
    icons: crate::icons::IconSet,
    palette: Palette,
) {
    let popup = area(body);
    frame.render_widget(Clear, popup);
    let block = super::overlay::modal("OTA", palette);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 6 {
        return;
    }

    let mut y = inner.y;
    for line in target_lines(panel, home, palette) {
        y = row(frame, inner, y, line);
    }
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Requirements", "r re-checks", palette),
    );
    for state in &panel.prepare.requirements {
        y = row(frame, inner, y, requirement_line(state, palette));
    }
    y = row(frame, inner, y, slot_line(&panel.prepare, palette));
    y = row(frame, inner, y, transport_line(panel, palette));
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Prepare", prepare_hint(panel), palette),
    );
    for (index, step) in Step::ALL.iter().enumerate() {
        y = row(frame, inner, y, prepare_line(panel, index, *step, palette));
    }
    y = row(frame, inner, y, Line::from(""));
    y = row(frame, inner, y, heading("Update", "", palette));
    for (index, stage) in panel.stage_list().iter().enumerate() {
        y = row(
            frame,
            inner,
            y,
            stage_line(panel, index, *stage, ticks, palette),
        );
    }
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Output", output_hint(panel), palette),
    );

    let footer_top = inner.bottom().saturating_sub(3);
    draw_output(frame, inner, y, footer_top, panel, palette);
    draw_footer(frame, inner, footer_top, panel, icons, palette);
}

fn row(frame: &mut Frame, area: Rect, y: u16, line: Line<'static>) -> u16 {
    if y >= area.bottom() {
        return y;
    }
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            y,
            height: 1,
            ..area
        },
    );
    y + 1
}

fn heading(text: &str, hint: impl Into<String>, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(text.to_string(), Style::new().fg(palette.accent).bold()),
        Span::raw("  "),
        Span::styled(hint.into(), muted_style(palette)),
    ])
}

/// The target block: what project and board, what image, where the board
/// answers --- the four facts the update runs against, stated before
/// anything is asked.
fn target_lines(panel: &OtaPanel, home: &std::path::Path, palette: Palette) -> Vec<Line<'static>> {
    let field = |name: &str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("{name:<9}"), muted_style(palette)),
            Span::styled(value, style),
        ])
    };
    let project = panel
        .prepare
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| tilde_path(&panel.prepare.root, home));
    let mut lines = vec![
        field(
            "Target",
            format!("{project} · {}", panel.board()),
            Style::new().fg(palette.fg).bold(),
        ),
        field(
            "Image",
            panel.image().map_or_else(
                || "no signed image yet --- build with --sysbuild".to_string(),
                |image| tilde_path(image, home),
            ),
            if panel.image().is_some() {
                Style::new().fg(palette.fg)
            } else {
                muted_style(palette)
            },
        ),
    ];
    let config = panel.config();
    let (address, style) = match &config.address {
        Some(address) => (
            format!("{address} ({})", config.transport.label()),
            Style::new().fg(palette.fg),
        ),
        None => (
            format!(
                "unanswered ({}) --- the button below asks",
                config.transport.label()
            ),
            muted_style(palette),
        ),
    };
    lines.push(field("Address", address, style));
    lines
}

/// The one requirement row: `smpmgr`, its answer, and the install hint
/// while it is missing --- reported, never installed.
fn requirement_line(
    state: &crate::ota::prepare::RequirementState,
    palette: Palette,
) -> Line<'static> {
    let (mark, status, style) = match &state.probe {
        ToolProbe::Probing => (RowMark::Open, "checking…".to_string(), muted_style(palette)),
        ToolProbe::Present(version) => (
            RowMark::Done,
            version.clone(),
            Style::new().fg(palette.success).bold(),
        ),
        ToolProbe::Missing => (
            RowMark::Broken,
            format!("not found --- {}", Requirement::Smpmgr.install_hint()),
            Style::new().fg(palette.error).bold(),
        ),
    };
    marked_row(
        mark,
        LABEL,
        state.requirement.label(),
        Line::from(Span::styled(status, style)),
        palette,
    )
}

/// The A/B slot precondition, read from the build's own devicetree.
///
/// It gates the button (`OtaPanel::action` returns `Blocked` on a
/// known-absent layout) and used to be drawn nowhere at all, so a blocked
/// panel showed a green `smpmgr` row, a dim button, and a state line
/// pointing at a requirement that read fine. Three states were
/// specified for it; this is them.
fn slot_line(prepare: &crate::ota::prepare::Prepare, palette: Palette) -> Line<'static> {
    // The build directory is the interesting half of the path and the root
    // is already on the Target row, so the row shows the path relative to
    // the project rather than repeating the project.
    let relative = |path: &std::path::Path| {
        path.strip_prefix(&prepare.root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let (mark, text, style) = match &prepare.slots {
        // Never a block: refusing to prepare a project that was never built
        // would be ChipTUI asserting no slots exist, which it cannot know.
        SlotCheck::NotChecked => (
            RowMark::Open,
            "not checked --- build once first".to_string(),
            muted_style(palette),
        ),
        SlotCheck::Found(path) => (
            RowMark::Done,
            format!("found in {}", relative(path)),
            Style::new().fg(palette.success).bold(),
        ),
        SlotCheck::Missing(missing, path) => (
            RowMark::Broken,
            format!("{} absent from {}", missing.join(", "), relative(path)),
            Style::new().fg(palette.error).bold(),
        ),
    };
    marked_row(
        mark,
        LABEL,
        "slot0/slot1",
        Line::from(Span::styled(text, style)),
        palette,
    )
}

/// The net shell toggle's hint, which has to name the direction `s` would
/// move it *from where the project actually is* --- it used to read "adds"
/// for a project that already had the block, beside a key that did nothing.
/// Whether the build actually carries the transport the project asked for.
///
/// The one failure in this flow with no symptom of its own: every
/// `MCUMGR_TRANSPORT_*` symbol is a `depends on`, so an unmet dependency
/// drops it to `n` and everything else still reports success --- the board
/// simply never answers, which reads like a wrong address.
fn transport_line(panel: &OtaPanel, palette: Palette) -> Line<'static> {
    let prepare = &panel.prepare;
    let relative = |path: &std::path::Path| {
        path.strip_prefix(&prepare.root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let transport = prepare.config().transport;
    let (mark, text, style) = match &prepare.transport {
        TransportCheck::NotChecked => (
            RowMark::Open,
            "not checked --- build once first".to_string(),
            muted_style(palette),
        ),
        TransportCheck::Enabled(path) => (
            RowMark::Done,
            format!("{}=y in {}", transport.symbol(), relative(path)),
            Style::new().fg(palette.success).bold(),
        ),
        TransportCheck::Stale(_) => (
            RowMark::Open,
            "the build predates the fragment --- rebuild to check".to_string(),
            muted_style(palette),
        ),
        // A warning, never an error: it is the project's configuration to
        // fix, and naming the unmet dependencies is the whole help there is
        // to give (`p` proves it either way).
        TransportCheck::Dropped(_) => (
            RowMark::Warn,
            // The symbol is named by the row's own label and by the
            // enabled state; what a truncated row must keep is the two
            // facts that are actionable.
            format!("dropped by the build --- needs {}", transport.requires()),
            Style::new().fg(palette.warning).bold(),
        ),
    };
    marked_row(
        mark,
        LABEL,
        "transport",
        Line::from(Span::styled(text, style)),
        palette,
    )
}

fn prepare_hint(panel: &OtaPanel) -> String {
    if panel.prepare.netshell() {
        "s removes the net shell".to_string()
    } else {
        "s adds the net shell".to_string()
    }
}

fn output_hint(panel: &OtaPanel) -> String {
    if panel.output_scroll > 0 {
        format!("↑{}  j/k scroll", panel.output_scroll)
    } else {
        "j/k scroll".to_string()
    }
}

fn prepare_line(panel: &OtaPanel, index: usize, step: Step, palette: Palette) -> Line<'static> {
    let state = &panel.prepare.steps[index];
    let mark = match state {
        StepState::Done => RowMark::Done,
        StepState::Failed(_) => RowMark::Broken,
        StepState::Skipped => RowMark::Warn,
        StepState::Pending | StepState::Running => RowMark::Open,
    };
    let mut spans = vec![Span::styled(
        panel.prepare.step_detail(step),
        if matches!(state, StepState::Skipped) {
            muted_style(palette).crossed_out()
        } else {
            muted_style(palette)
        },
    )];
    if let StepState::Failed(reason) = state {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(reason.clone(), Style::new().fg(palette.error)));
    }
    marked_row(mark, LABEL, step.label(), Line::from(spans), palette)
}

fn stage_line(
    panel: &OtaPanel,
    index: usize,
    stage: crate::ota::OtaStage,
    ticks: u64,
    palette: Palette,
) -> Line<'static> {
    let state = &panel.stages[index];
    let mark = match state {
        StepState::Done => RowMark::Done,
        StepState::Failed(_) => RowMark::Broken,
        StepState::Pending | StepState::Running => RowMark::Open,
        StepState::Skipped => RowMark::Warn,
    };
    // The literal command, or --- when it cannot be built yet --- the
    // driver's own reason, which is the honest placeholder. A generic
    // "waiting on an earlier stage" was drawn even when nothing was waiting
    // on a stage at all: with no address every row said it.
    let text = match panel.stage_command(index) {
        Ok(command) => command.to_string(),
        Err(reason) => reason,
    };
    let mut spans = vec![Span::styled(text, muted_style(palette))];
    match state {
        StepState::Running => {
            let frame = SPINNER[(ticks as usize) % SPINNER.len()];
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                frame.to_string(),
                Style::new().fg(palette.accent).bold(),
            ));
        }
        StepState::Failed(reason) => {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(reason.clone(), Style::new().fg(palette.error)));
        }
        _ => {}
    }
    marked_row(mark, LABEL, stage.label(), Line::from(spans), palette)
}

/// The output window: the tail of what the running stage printed, or the
/// slice the user scrolled back to.
fn draw_output(
    frame: &mut Frame,
    area: Rect,
    top: u16,
    footer_top: u16,
    panel: &OtaPanel,
    palette: Palette,
) {
    if top >= footer_top {
        return;
    }
    let height = usize::from(footer_top - top);
    let total = panel.output.len();
    let end = total.saturating_sub(panel.output_scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line<'static>> = panel
        .output
        .iter()
        .skip(start)
        .take(end - start)
        .map(|text| {
            // The `$ command` headers are the structure of this feed; the
            // rest is the tool talking.
            let style = if text.starts_with("$ ") {
                Style::new().fg(palette.accent)
            } else {
                Style::new().fg(palette.fg)
            };
            Line::from(Span::styled(text.clone(), style))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            y: top,
            height: footer_top - top,
            ..area
        },
    );
}

/// The footer button's glyph color: `accent` for the forward-moving
/// actions, `success` for a state that is already good, `warning` for
/// `Stop` --- the vocabulary `ui::install` keeps.
fn action_icon_color(action: OtaAction, palette: Palette) -> Color {
    match action {
        OtaAction::Stop => palette.warning,
        OtaAction::ConfirmImage | OtaAction::RetryConfirm | OtaAction::Done => palette.success,
        OtaAction::Blocked
        | OtaAction::SetAddress
        | OtaAction::Prepare
        | OtaAction::RetryPrepare
        | OtaAction::Rebuild
        | OtaAction::Update
        | OtaAction::RetryUpdate => palette.accent,
    }
}

/// Where the footer's one button is drawn, given the modal's *inner* area
/// and the footer's top row.
///
/// One definition, consumed by [`draw_footer`] and by the click
/// hit-testing (`app::mouse`'s `Overlay::Ota` arm), which is the rule
/// `ui::layout::overlay_popup` keeps for every other overlay. Written on
/// both sides they had already drifted: the button is drawn half the
/// modal wide and was hit-tested at `STOP_BOX_WIDTH`'s thirteen columns,
/// so most of the visible button was not clickable.
pub(crate) fn footer_button_rect(inner: Rect, footer_top: u16) -> Rect {
    let half = inner.width / 2;
    Rect {
        x: inner.x + half,
        width: inner.width - half,
        y: footer_top,
        height: inner.bottom().saturating_sub(footer_top),
    }
}

/// The modal's inner area, from the popup the overlay layout gave it ---
/// the other half of the geometry the hit-testing has to reproduce.
pub(crate) fn footer_geometry(popup: Rect) -> (Rect, u16) {
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let footer_top = inner.y + inner.height.saturating_sub(3);
    (inner, footer_top)
}

/// The reserved footer: the state line on the left half, the panel's one
/// action as its own half-width box on the right --- identical geometry to
/// the installer's modal. The button is drawn straight from
/// [`OtaPanel::action`], so what it says and what `Enter` does cannot
/// drift apart.
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    footer_top: u16,
    panel: &OtaPanel,
    icons: crate::icons::IconSet,
    palette: Palette,
) {
    if footer_top + 1 >= area.bottom() {
        return;
    }
    let half = area.width / 2;
    let action = panel.action();
    button::render_stack(
        frame,
        footer_button_rect(area, footer_top),
        footer_top,
        &[Button::new(action.label())
            .icon(action.icon(icons), action_icon_color(action, palette))
            .enabled(action.enabled())
            .selected(true)],
        palette,
    );
    frame.render_widget(
        Paragraph::new(state_line(panel, palette)),
        Rect {
            y: footer_top + 1,
            width: half,
            ..area
        },
    );
}

/// What the panel is doing, in one line. The unconfirmed halt is the one
/// state that must never read as finished, so it is the one in warning.
fn state_line(panel: &OtaPanel, palette: Palette) -> Line<'static> {
    if let Some(stage) = panel.running_stage() {
        let mut text = format!("{}…", stage.label().to_lowercase());
        if let Some(progress) = panel.progress() {
            text.push_str(&format!(" {}", progress.render()));
        }
        if let Some(elapsed) = panel.elapsed() {
            text.push_str(&format!(" · {}s", elapsed.as_secs()));
        }
        return Line::from(Span::styled(text, Style::new().fg(palette.fg)));
    }
    if let Some(remaining) = panel.settling_remaining() {
        return Line::from(Span::styled(
            format!(
                "resetting --- the bootloader's swap takes a moment · {}s left",
                remaining.as_secs()
            ),
            Style::new().fg(palette.fg),
        ));
    }
    // Both facts can hold at once --- a `Confirm` that failed leaves the
    // image unconfirmed --- and showing only the halt hid the reason the
    // confirm did not take. One line carries both.
    if panel.awaiting_confirm() {
        let text = match &panel.update_phase {
            // The warning leads. The state line owns only the footer's left
            // half, so a long stage reason would push the one fact the user
            // must not miss --- that the running image reverts on the next
            // reset --- off the end of the line.
            Phase::Stopped(reason) => format!("unconfirmed --- {reason}"),
            _ => {
                "updated --- unconfirmed: the next reset reverts to the previous image".to_string()
            }
        };
        return Line::from(Span::styled(text, Style::new().fg(palette.warning).bold()));
    }
    if let Phase::Stopped(reason) = &panel.update_phase {
        return Line::from(Span::styled(reason.clone(), Style::new().fg(palette.error)));
    }
    if panel.prepare.stopped() {
        return Line::from(Span::styled(
            panel.prepare.stop_reason(),
            Style::new().fg(palette.error),
        ));
    }
    let text = match panel.action() {
        // One wording for three different blocks said nothing about any of
        // them --- and for the slot case it pointed at a requirement row
        // that reads fine.
        OtaAction::Blocked => blocked_reason(panel),
        OtaAction::Prepare => "the project is not instrumented for OTA yet".to_string(),
        // The button is dim and the build belongs to the Actions pane, so
        // this line is the whole explanation --- and "rebuild" alone was
        // not one. There are two ways to have no image: a directory that
        // is not a sysbuild build (an incremental `Build` never passes
        // `--sysbuild`, so one configured before the project was prepared
        // stays plain until a *pristine* rebuild) and one that simply has
        // not produced it yet. Naming where it looked tells them apart.
        OtaAction::Rebuild => match panel.build_dir() {
            Some(dir) => format!("no signed image in {dir}/ --- Rebuild (pristine) writes one"),
            None => "no build directory --- Rebuild (pristine) writes the image".to_string(),
        },
        OtaAction::SetAddress => "the board's address is unanswered".to_string(),
        // "the board answers" was a claim nothing had checked: `Probe` is
        // the first stage of the cycle, not something already run. `p` is
        // the key that actually asks.
        OtaAction::Update => "ready --- the image is signed; p probes the board".to_string(),
        OtaAction::Done => "the update is confirmed and permanent".to_string(),
        OtaAction::Stop
        | OtaAction::RetryPrepare
        | OtaAction::RetryUpdate
        | OtaAction::RetryConfirm
        | OtaAction::ConfirmImage => String::new(),
    };
    Line::from(Span::styled(text, muted_style(palette)))
}

/// Which of the three preconditions is holding the button down. They are
/// checked in the order [`OtaPanel::action`] checks them.
fn blocked_reason(panel: &OtaPanel) -> String {
    // The state line owns only the footer's left half, so it points and the
    // row above names the nodes --- spelling them out here truncated away
    // the pointer, which is the one part the row cannot supply.
    if panel.prepare.slots.blocks() {
        return "no A/B slots --- see slot0/slot1".to_string();
    }
    if panel
        .prepare
        .requirements
        .iter()
        .any(|state| matches!(state.probe, ToolProbe::Probing))
    {
        return "checking the requirements above…".to_string();
    }
    format!(
        "{} is missing --- see Requirements above",
        Requirement::Smpmgr.label()
    )
}
