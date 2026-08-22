//! The Zephyr installer's modal.
//!
//! Three sections stacked in one near-full-screen dialog: the
//! **prerequisites** the machine must already have, the **steps** the
//! installation runs, and the **output** of whichever step is running ---
//! over the same three-row reserved footer the build and flash panes use
//! (state line on the left half, `■ Stop` as its own half-width button box
//! on the right), so a command starting or ending never moves anything.
//!
//! Prerequisite and step rows share the panes' checklist grammar
//! ([`super::workspace::marked_row`]): one mark, one label column, one
//! value. The step rows' value is the literal command, muted --- what runs
//! is never hidden behind a friendly label (`SPEC.md` §15).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::install::{Action, Installer, Phase, Prereq, Probe, Step, StepState};

use super::button::{self, Button};
use super::workspace::{RowMark, marked_row};
use super::{Palette, SPINNER, centered, muted_style, tilde_path};

/// The step labels' column (`Update the workspace` is the longest), wider
/// than the panes' 13 --- these are sentences, not field names.
const STEP_LABEL: usize = 21;
/// The prerequisite labels' column: four short program names.
const PREREQ_LABEL: usize = 7;
/// Rows the sections above the output always occupy, so the output area is
/// whatever is left rather than a guess: target, blank, two headings, four
/// prerequisites, twelve steps, two blanks, the output heading.
const FIXED_ROWS: u16 = 1 + 1 + 1 + 4 + 1 + 1 + 12 + 1 + 1;

/// The dialog fills the screen bar a margin: the step list alone is twelve
/// rows and the output needs room to be worth reading. Capped so it does
/// not sprawl on a very wide terminal.
pub(super) fn area(body: Rect) -> Rect {
    centered(
        body,
        body.width.saturating_sub(4).min(96),
        body.height.saturating_sub(2),
    )
}

/// Rows of output the modal can show at `body`'s size --- published so the
/// key handler's page scrolling matches what is drawn, the same contract
/// [`crate::app::App::log_viewport`] has for the log pane.
pub fn output_viewport(body: Rect) -> usize {
    let popup = area(body);
    // Two border rows, the fixed sections, and the three-row footer.
    usize::from(
        popup
            .height
            .saturating_sub(2)
            .saturating_sub(FIXED_ROWS)
            .saturating_sub(3),
    )
}

pub(super) fn draw(
    frame: &mut Frame,
    body: Rect,
    installer: &Installer,
    home: &std::path::Path,
    ticks: u64,
    icons: crate::icons::IconSet,
    palette: Palette,
) {
    let popup = area(body);
    frame.render_widget(Clear, popup);
    let block = super::overlay::modal("Install Zephyr", palette);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 6 {
        return;
    }

    let mut y = inner.y;
    y = row(frame, inner, y, target_line(installer, home, palette));
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Prerequisites", "r re-checks", palette),
    );
    for state in &installer.prereqs {
        y = row(frame, inner, y, prereq_line(state, palette));
    }
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Steps", steps_hint(installer), palette),
    );
    for (index, step) in Step::ALL.iter().enumerate() {
        y = row(
            frame,
            inner,
            y,
            step_line(installer, index, *step, ticks, palette),
        );
    }
    y = row(frame, inner, y, Line::from(""));
    y = row(
        frame,
        inner,
        y,
        heading("Output", output_hint(installer), palette),
    );

    let footer_top = inner.bottom().saturating_sub(3);
    draw_output(frame, inner, y, footer_top, installer, palette);
    draw_footer(frame, inner, footer_top, installer, icons, palette);
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

fn target_line(installer: &Installer, home: &std::path::Path, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<9}", "Target"), muted_style(palette)),
        Span::styled(
            tilde_path(&installer.root, home),
            Style::new().fg(palette.warning).bold(),
        ),
    ])
}

fn steps_hint(installer: &Installer) -> String {
    if installer.sdk_skipped {
        "s installs the SDK · t picks toolchains".to_string()
    } else {
        "s skips the SDK · t picks toolchains".to_string()
    }
}

fn output_hint(installer: &Installer) -> String {
    if installer.output_scroll > 0 {
        format!("↑{}  j/k scroll", installer.output_scroll)
    } else {
        "j/k scroll".to_string()
    }
}

/// A prerequisite row: its mark, its name, the version found, the minimum
/// asked of it --- and, when something is wrong with it, the command that
/// would fix it here. The three fields are fixed-width so the remedies
/// start in one column down the list rather than wherever the previous
/// row's version happened to end.
fn prereq_line(state: &crate::install::PrereqState, palette: Palette) -> Line<'static> {
    let prereq = state.prereq;
    let mark = match &state.probe {
        Probe::Probing => RowMark::Open,
        Probe::Ok(_) | Probe::Unreadable => RowMark::Done,
        Probe::OffSeries(_) => RowMark::Warn,
        Probe::Old(_) | Probe::Missing => {
            if prereq.blocking() {
                RowMark::Broken
            } else {
                RowMark::Warn
            }
        }
    };
    // One fixed-width status column, whatever the answer is, so the
    // minimums and the remedies below line up down the list instead of
    // starting wherever the previous row's version happened to end.
    let (status, style) = match &state.probe {
        Probe::Probing => ("checking…".to_string(), muted_style(palette)),
        Probe::Unreadable => ("present".to_string(), muted_style(palette)),
        Probe::Missing => (
            "not found".to_string(),
            Style::new()
                .fg(if prereq.blocking() {
                    palette.error
                } else {
                    palette.warning
                })
                .bold(),
        ),
        Probe::Ok(version) | Probe::Old(version) | Probe::OffSeries(version) => {
            (version.to_string(), Style::new().fg(palette.success).bold())
        }
    };
    // `Version`'s `Display` writes directly rather than through `f.pad`,
    // so the width has to be applied to the finished string.
    let mut spans: Vec<Span<'static>> = vec![Span::styled(format!("{status:<10}"), style)];
    // Same fixed width, same reason: the remedies after it start in one
    // column whether or not the row has a minimum to state.
    let minimum = prereq
        .minimum()
        .map_or_else(String::new, |minimum| format!("min {minimum}"));
    spans.push(Span::styled(format!("{minimum:<12}"), muted_style(palette)));
    if prereq == Prereq::Python {
        // The one row that reports rather than demands: say why, so a ⚠
        // beside "3.11.9" does not read as something to go fix.
        spans.push(Span::styled(
            "pyenv provides 3.12 for the workspace",
            muted_style(palette),
        ));
    }
    let hints = remedy(state);
    for (index, hint) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", muted_style(palette)));
        }
        spans.push(Span::styled(hint.clone(), Style::new().fg(palette.warning)));
    }
    marked_row(
        mark,
        PREREQ_LABEL,
        prereq.label(),
        Line::from(spans),
        palette,
    )
}

/// The install command for a prerequisite that needs one --- only for the
/// rows that actually block, and only while they do.
fn remedy(state: &crate::install::PrereqState) -> Vec<String> {
    if state.satisfied() || matches!(state.probe, Probe::Probing) {
        return Vec::new();
    }
    state.prereq.install_hint(crate::backend::tool_available)
}

fn step_line(
    installer: &Installer,
    index: usize,
    step: Step,
    ticks: u64,
    palette: Palette,
) -> Line<'static> {
    let state = &installer.steps[index];
    let mark = match state {
        StepState::Done => RowMark::Done,
        StepState::Failed(_) => RowMark::Broken,
        StepState::Skipped => RowMark::Warn,
        StepState::Pending | StepState::Running => RowMark::Open,
    };
    // The literal command, or --- while an earlier query still owes it an
    // answer --- an honest placeholder rather than a half-built line.
    let text = installer.step_command(index).map_or_else(
        || "waiting on an earlier step".to_string(),
        |c| c.to_string(),
    );
    let mut spans = vec![Span::styled(
        text,
        if matches!(state, StepState::Skipped) {
            muted_style(palette).crossed_out()
        } else {
            muted_style(palette)
        },
    )];
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
        StepState::Skipped => {
            spans.push(Span::styled(" skipped", muted_style(palette)));
        }
        StepState::Pending | StepState::Done => {}
    }
    marked_row(mark, STEP_LABEL, step.label(), Line::from(spans), palette)
}

/// The output window: the tail of what the running step printed, or the
/// slice the user scrolled back to.
fn draw_output(
    frame: &mut Frame,
    area: Rect,
    top: u16,
    footer_top: u16,
    installer: &Installer,
    palette: Palette,
) {
    if top >= footer_top {
        return;
    }
    let height = usize::from(footer_top - top);
    let total = installer.output.len();
    let end = total.saturating_sub(installer.output_scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line<'static>> = installer
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

/// The footer button's glyph color: `accent` for every forward-moving step
/// (`▶`), `success` for a state that is already good (`✓`), `warning` for
/// `Stop` --- the same vocabulary `ui::build`/`ui::flash` use for their own
/// action icons. Kept here rather than on `Action` itself: that type stays
/// UI-free.
fn action_icon_color(action: Action, palette: Palette) -> Color {
    match action {
        Action::Stop => palette.warning,
        Action::Blocked
        | Action::Install
        | Action::PickToolchains
        | Action::Retry
        | Action::InstallSdk
        | Action::AddToolchains => palette.accent,
        Action::Adopt | Action::Done => palette.success,
    }
}

/// The reserved footer: the state line on the left half, the panel's one
/// action as its own half-width box on the right. Identical geometry to the
/// build and flash panes, so the modal reads as part of the same app.
///
/// The button is drawn straight from [`Installer::action`] --- label and
/// enabled state both --- so what it says and what `Enter` does cannot
/// drift apart.
fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    footer_top: u16,
    installer: &Installer,
    icons: crate::icons::IconSet,
    palette: Palette,
) {
    if footer_top + 1 >= area.bottom() {
        return;
    }
    let half = area.width / 2;
    let action = installer.action();
    button::render_stack(
        frame,
        Rect {
            x: area.x + half,
            width: area.width - half,
            y: footer_top,
            height: area.bottom().saturating_sub(footer_top),
        },
        footer_top,
        &[Button::new(action.label())
            .icon(action.icon(icons), action_icon_color(action, palette))
            .enabled(action.enabled())
            .selected(true)],
        palette,
    );
    frame.render_widget(
        Paragraph::new(state_line(installer, palette)),
        Rect {
            y: footer_top + 1,
            width: half,
            ..area
        },
    );
}

fn state_line(installer: &Installer, palette: Palette) -> Line<'static> {
    let label = Span::styled(format!("{:<6}", "state"), muted_style(palette));
    let rest = match &installer.phase {
        Phase::Running => {
            let what = installer
                .running_step()
                .map_or("running", crate::install::Step::label);
            let elapsed = installer.elapsed().unwrap_or_default();
            Span::styled(
                format!("{what} · {}", crate::build::BuildPanel::secs(elapsed)),
                Style::new().fg(palette.accent),
            )
        }
        Phase::Finished => Span::styled(
            "✓ Zephyr installed".to_string(),
            Style::new().fg(palette.success).bold(),
        ),
        Phase::Stopped(reason) => {
            Span::styled(format!("✗ {reason}"), Style::new().fg(palette.error))
        }
        // These arms follow `Installer::action`'s own order, adopting
        // ahead of the prerequisites: neither adopting nor `west sdk
        // install` needs cmake or dtc, so reporting them as the blocker
        // there would contradict the button beside this line.
        Phase::Idle if installer.adopted() && installer.sdk_missing() => Span::styled(
            "installed, no SDK".to_string(),
            Style::new().fg(palette.warning),
        ),
        // The bundle is there but does not carry everything asked for ---
        // adding those costs a `setup.sh -t`, not the bundle again.
        Phase::Idle if installer.adopted() && !installer.pending_toolchains().is_empty() => {
            let pending = installer.pending_toolchains().len();
            Span::styled(
                format!("installed · {pending} to add"),
                Style::new().fg(palette.warning),
            )
        }
        Phase::Idle if installer.adopted() => Span::styled(
            "already installed".to_string(),
            Style::new().fg(palette.success).bold(),
        ),
        Phase::Idle if !installer.prereqs_ready() => Span::styled(
            "prerequisites missing".to_string(),
            Style::new().fg(palette.warning),
        ),
        // Kept short: the state line owns only the footer's left half, and
        // the button beside it already says what to press.
        Phase::Idle if !installer.sdk_ready() => Span::styled(
            "no SDK toolchains picked".to_string(),
            Style::new().fg(palette.warning),
        ),
        // Answered: say *how* it was answered, so "ready" is never a
        // surprise about what the SDK step is going to do.
        Phase::Idle if installer.sdk_skipped => {
            Span::styled("ready · SDK skipped".to_string(), muted_style(palette))
        }
        Phase::Idle => Span::styled(
            format!(
                "ready · {} toolchain{}",
                installer.picked_toolchains.len(),
                if installer.picked_toolchains.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            muted_style(palette),
        ),
    };
    Line::from(vec![label, rest])
}
