//! Dashboard panes: project, device, the no-filesystem placeholder, and log.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs, Wrap};

use crate::app::{App, Focus, LogTab, MonitorSource, ProjectRow};
use crate::backend::Capability;
use crate::backend::micropython::esptool::features;
use crate::device::ScriptState;
use crate::firmware_id::{FirmwareVerdict, FlashFirmware};
use crate::flash::RunState;
use crate::logs::{Level, PREFIX_WIDTH};
use crate::project::DetectionOutcome;
use crate::ui::{
    Palette, SPINNER, content_style, dashboard_focused, muted_style, output_style, pane_block,
    pane_border, tilde_path,
};

/// Row 1's fixed content height: the Project and the Device info panes
/// both render exactly this many lines --- shorter content is padded with
/// blanks --- in every backend and state, so the rows below never shift
/// when a workspace resolves or device details accumulate. Four: the
/// device pane's fullest report (chip+crystal, features, MAC, firmware)
/// is the ceiling the Project pane's questions pad up to.
pub(super) const INFO_ROWS: usize = 4;

/// Pads an info pane's lines to [`INFO_ROWS`] blank rows.
fn pad_info(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    debug_assert!(
        lines.len() <= INFO_ROWS,
        "an info pane grew past its fixed {INFO_ROWS} rows"
    );
    while lines.len() < INFO_ROWS {
        lines.push(Line::from(""));
    }
    lines
}

/// Project identity: the environment's open questions, answered in place.
///
/// For a backend that asks any (Zephyr: installation, projects folder,
/// project, target; MicroPython: projects folder, project, plus the
/// dependencies and script reports), the pane is the checklist those questions moved
/// into --- navigable through `ctrl+p`, never part of the `Tab` tour. A
/// backend that asks nothing falls back to plain detection info (root and
/// type). The environment's versions ride the pane's bottom border (right
/// edge, like the Log tab's status rides the top), so they never cost a
/// content row.
pub fn draw_project(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let focused = dashboard_focused(app, Focus::Project);
    // The pane's own shortcut rides the title --- it is the one pane off
    // the `Tab` tour, so its way in must be visible where the pane is.
    let block = pane_block("Project (ctrl+p)", focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = app.project_rows();
    if rows.is_empty() {
        // No backend asks anything here yet: plain detection info, padded
        // to the pane's fixed height like the checklist rows would be.
        frame.render_widget(
            Paragraph::new(pad_info(detection_fallback(
                app,
                inner.width as usize,
                palette,
            )))
            .style(content_style(focused))
            .wrap(Wrap { trim: false }),
            inner,
        );
    } else {
        let mut y = inner.y;
        for (position, row) in rows.iter().enumerate() {
            let selected = focused && app.project_cursor == position;
            let line = project_row_line(app, *row, inner.width as usize, palette, selected);
            y = super::workspace::render_row(frame, inner, y, line, selected, palette);
        }
    }
    draw_versions_badge(frame, area, app, palette);
}

/// One checklist row of the Project pane, in the shared `✓/□/✗ + label +
/// answer` grammar (`crate::ui::workspace::checklist_row`); the target row
/// and the two MicroPython reports build their own values.
fn project_row_line(
    app: &App,
    row: ProjectRow,
    width: usize,
    palette: Palette,
    selected: bool,
) -> Line<'static> {
    match row {
        ProjectRow::ZephyrPath => {
            let panel = app.workspace.as_ref();
            super::workspace::checklist_row(
                panel.is_some_and(|panel| panel.resolved.is_some()),
                panel.is_some_and(|panel| panel.invalid.is_some()),
                "Zephyr path",
                super::workspace::answer_value(
                    panel
                        .and_then(|panel| panel.resolved.as_ref())
                        .map(|workspace| tilde_path(&workspace.dir, app.home_dir())),
                    panel.is_some_and(|panel| panel.invalid.is_some()),
                    width as u16,
                    palette,
                ),
                palette,
            )
        }
        ProjectRow::ProjectsBase => {
            let panel = app.workspace.as_ref();
            super::workspace::checklist_row(
                panel.is_some_and(|panel| panel.projects.is_some()),
                panel.is_some_and(|panel| panel.projects_invalid.is_some()),
                "Projects base",
                super::workspace::answer_value(
                    panel
                        .and_then(|panel| panel.projects.as_ref())
                        .map(|dir| tilde_path(dir, app.home_dir())),
                    panel.is_some_and(|panel| panel.projects_invalid.is_some()),
                    width as u16,
                    palette,
                ),
                palette,
            )
        }
        ProjectRow::ProjectPath => {
            let project_ok = app.project_gate_ok();
            let answer = project_ok.then(|| {
                format!(
                    "{} · {}",
                    tilde_path(&app.build.as_ref().unwrap().root, app.home_dir()),
                    app.build.as_ref().unwrap().project_origin.label()
                )
            });
            super::workspace::checklist_row(
                project_ok,
                false,
                "Project path",
                super::workspace::answer_value(answer, false, width as u16, palette),
                palette,
            )
        }
        ProjectRow::BoardShield => board_shield_row(app, width as u16, palette, selected),
        ProjectRow::MpyProjectsBase => super::workspace::checklist_row(
            app.mpy_projects.is_some(),
            app.mpy_projects_invalid.is_some(),
            "Projects base",
            super::workspace::answer_value(
                app.mpy_projects
                    .as_ref()
                    .map(|dir| tilde_path(dir, app.home_dir())),
                app.mpy_projects_invalid.is_some(),
                width as u16,
                palette,
            ),
            palette,
        ),
        ProjectRow::MpyProjectPath => {
            let root = tilde_path(&app.mpy_effective_root(), app.home_dir());
            // The cwd note rides the row only before a pick re-roots the
            // project: after one, the pick *is* the answer and the launch
            // directory is history. Same rule the old root row followed ---
            // a fixed-height pane owes every field its own row.
            let shows_cwd = app.mpy_root.is_none()
                && app
                    .manager
                    .root()
                    .is_some_and(|root| root != app.manager.start_dir());
            let mut value =
                super::workspace::answer_value(Some(root), false, width as u16, palette);
            if shows_cwd {
                let cwd = tilde_path(app.manager.start_dir(), app.home_dir());
                let take = (width / 3).saturating_sub(7).min(cwd.chars().count());
                if take > 0 {
                    value.spans.push(Span::styled(
                        format!(" (cwd {})", truncate_start(&cwd, take)),
                        muted_style(palette),
                    ));
                }
            }
            super::workspace::checklist_row(true, false, "Project path", value, palette)
        }
        ProjectRow::MpyDependencies => {
            let root = app.mpy_effective_root();
            let entry = |name: &str| root.join(name).is_file();
            let (reqs, manifest) = (entry("requirements.txt"), entry("manifest.py"));
            let mark = |present: bool| {
                Span::styled(
                    if present { "✓" } else { "✗" },
                    Style::new()
                        .fg(if present {
                            palette.success
                        } else {
                            palette.error
                        })
                        .bold(),
                )
            };
            super::workspace::checklist_row(
                reqs || manifest,
                false,
                "Dependencies",
                Line::from(vec![
                    mark(reqs),
                    Span::raw(" requirements.txt "),
                    mark(manifest),
                    Span::raw(" manifest.py"),
                ]),
                palette,
            )
        }
        ProjectRow::MpyScript => {
            let (text, style, answered) = match app.devices.script_state() {
                ScriptState::Running => ("running", Style::new().fg(palette.warning).bold(), true),
                ScriptState::Stopped => ("idle", Style::new().fg(palette.success).bold(), true),
                ScriptState::Unknown => ("unknown", muted_style(palette), false),
            };
            super::workspace::checklist_row(
                answered,
                false,
                "Script",
                Line::from(Span::styled(text, style)),
                palette,
            )
        }
    }
}

/// The target row: the board with its optional shield riding on the same
/// line (`Board: name · Shield: name`). While the row is selected the half
/// `Enter` acts on is underlined --- the segment cursor `←`/`→` moves.
fn board_shield_row(app: &App, width: u16, palette: Palette, selected: bool) -> Line<'static> {
    let budget = super::workspace::value_budget(width);
    let board = app
        .build
        .as_ref()
        .and_then(|panel| panel.board.as_ref().map(|choice| choice.name.clone()));
    let shield = app.build.as_ref().and_then(|panel| panel.shield.clone());

    // The separator (11 columns) plus both names fit the budget together
    // in the common case; when they do not, the shield (the optional half)
    // is capped at half the slack and the board keeps the rest --- its
    // tail is the identity, so it is shortened from the left, like every
    // other path here.
    let separator = " · Shield: ";
    let shield_text = shield.clone().unwrap_or_else(|| "none".to_string());
    let both_fit = board.as_deref().is_some_and(|name| {
        name.chars().count() + separator.chars().count() + shield_text.chars().count() <= budget
    });
    let shield_budget = if both_fit {
        shield_text.chars().count()
    } else {
        (budget.saturating_sub(separator.chars().count()) / 2).min(shield_text.chars().count())
    };
    let board_budget = budget
        .saturating_sub(separator.chars().count() + shield_budget)
        .max(4);

    let segment = |text: String, active: bool, answered: bool| {
        let mut style = if answered {
            Style::new().fg(palette.success).bold()
        } else {
            muted_style(palette)
        };
        if active && selected {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        Span::styled(text, style)
    };

    let mut spans = vec![
        segment(
            board.as_ref().map_or_else(
                || "?".to_string(),
                |name| shorten_start_owned(name, board_budget),
            ),
            app.board_segment,
            board.is_some(),
        ),
        Span::styled(separator, muted_style(palette)),
        segment(
            shorten_start_owned(&shield_text, shield_budget.max(1)),
            !app.board_segment,
            shield.is_some(),
        ),
    ];
    if let Some(choice) = app.build.as_ref().and_then(|panel| panel.board.as_ref()) {
        let origin = match choice.origin {
            crate::build::BoardOrigin::Picked => "picked",
            crate::build::BoardOrigin::Config => "saved",
            crate::build::BoardOrigin::Cache => "from build/",
        };
        // The origin rides along only when the *whole* line fits --- both
        // segments included, never at their expense.
        let used: usize = spans.iter().map(|span| span.width()).sum();
        let suffix = format!("  · {origin}");
        if used + suffix.chars().count() <= budget {
            spans.push(Span::styled(suffix, muted_style(palette)));
        }
    }
    super::workspace::checklist_row(board.is_some(), false, "Board", Line::from(spans), palette)
}

/// The pane's content when the backend asks nothing: where the project is
/// and what it is (the pre-checklist Project pane, kept for the
/// no-backend/window-before-detection states).
fn detection_fallback(app: &App, width: usize, palette: Palette) -> Vec<Line<'static>> {
    let detection = app.manager.detection();
    let mut lines = Vec::new();

    let path_budget = width.saturating_sub(2 + LABEL_WIDTH);
    let root = app.manager.root().map_or_else(
        || tilde_path(app.manager.start_dir(), app.home_dir()),
        |root| tilde_path(root, app.home_dir()),
    );
    let shows_cwd = app
        .manager
        .root()
        .is_some_and(|root| root != app.manager.start_dir());
    let mut root_budget = path_budget;
    let mut cwd_suffix = None;
    if shows_cwd {
        let cwd = tilde_path(app.manager.start_dir(), app.home_dir());
        let take = if root.chars().count() + 7 + cwd.chars().count() <= path_budget {
            cwd.chars().count()
        } else {
            (path_budget / 3).saturating_sub(7).min(cwd.chars().count())
        };
        if take > 0 {
            root_budget = path_budget.saturating_sub(7 + take);
            cwd_suffix = Some(truncate_start(&cwd, take));
        }
    }
    let mut spans = vec![label_span("root", palette)];
    spans.push(Span::styled(
        truncate_start(&root, root_budget),
        Style::new().fg(palette.fg),
    ));
    if let Some(cwd) = cwd_suffix {
        spans.push(Span::styled(format!(" (cwd {cwd})"), muted_style(palette)));
    }
    lines.push(Line::from(spans));

    match detection.map(|d| &d.outcome) {
        Some(DetectionOutcome::Detected(kind)) => {
            lines.push(field_styled(
                "type",
                kind.display_name().to_string(),
                Style::new().fg(palette.success).bold(),
                palette,
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
                Style::new().fg(palette.warning),
                palette,
            ));
        }
        Some(DetectionOutcome::Unknown) => {
            lines.push(field_styled(
                "type",
                "unknown".to_string(),
                Style::new().fg(palette.error),
                palette,
            ));
        }
        None => lines.push(field("type", "not detected yet".to_string(), palette)),
    }
    lines
}

/// The environment's versions, on the pane's bottom border's right edge ---
/// the same place the Log tab's status sits on its top border, so a fact
/// that arrives late costs no content row and never moves the rows above.
/// Drawn only once a Zephyr installation resolves (the user's rule: no
/// base, no versions).
fn draw_versions_badge(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(workspace) = app
        .workspace
        .as_ref()
        .and_then(|panel| panel.resolved.as_ref())
    else {
        return;
    };
    // Read from files (`zephyr/VERSION`, the venv's `pyvenv.cfg`), never
    // from a subprocess --- this pane reports the state it builds against.
    // No label: the values name themselves (`zephyr 4.1 · python 3.12`),
    // and the border is the badge's context enough.
    let mut versions = format!(
        "zephyr {}",
        workspace
            .zephyr_version()
            .unwrap_or_else(|| "unknown".to_string())
    );
    if let Some(python) = workspace.python_version() {
        versions.push_str(&format!(" · python {python}"));
    }
    if area.width < 4 {
        return;
    }
    let versions = truncate_end(&versions, (area.width - 2) as usize);
    let badge = Rect {
        x: area.x + 1,
        y: area.bottom().saturating_sub(1),
        width: area.width - 2,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {versions} "),
            muted_style(palette),
        )))
        .alignment(Alignment::Right),
        badge,
    );
}

/// Shortens from the left: a path's tail (its distinctive part) matters
/// more than its `/tmp` prefix. The workspace pane's own `shorten_start`
/// returns a `String` for its rows; this is the same rule for borrowed
/// values.
fn shorten_start_owned(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut short: String = text
        .chars()
        .skip(text.chars().count() - budget.max(1) + 1)
        .collect();
    short.insert(0, '…');
    short
}

/// What esptool has reported about the connected board so far: identity
/// (chip/revision/features/crystal/MAC), accumulated across whatever
/// `chip-id`/`flash-id`/flash/erase/verify runs have happened in the Flash
/// view (`crate::flash::FlashPanel::details`). The backend's own name already
/// lives in the Project pane above, so this space is spent on the board itself
/// instead of repeating it. Like the Project pane it is informational only and
/// never holds focus.
pub fn draw_detection(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let block = pane_block("Device info", false, palette);
    let lines = device_content(app, area.width as usize, palette);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Builds the Device info pane's content lines (placeholder or details),
/// padded to [`INFO_ROWS`]; see [`project_content`] for why.
fn device_content(app: &App, width: usize, palette: Palette) -> Vec<Line<'static>> {
    let caps = app.manager.capabilities();
    let muted = muted_style(palette);

    // This pane is esptool's report (chip identity). A backend that flashes
    // another way (Zephyr: `west flash` through the build panel) has no
    // esptool Flash menu of its own, but its board may still be an esptool
    // one --- Zephyr runs on ESP32s --- so the background identity query
    // fills this pane for any backend; only the empty-state wording depends
    // on whether the user has a Project actions tab to reach ('x').
    let esptool_flash_view =
        caps.contains(Capability::DeviceInfo) || caps.contains(Capability::EraseFlash);
    let Some(flash) = app.flash.as_ref() else {
        return pad_info(vec![
            if esptool_flash_view {
                Line::from("no device data yet --- press 'x' for Project actions")
            } else {
                Line::from("no device information for this project")
            }
            .style(muted),
        ]);
    };
    let details = &flash.details;
    if details.is_empty() {
        return pad_info(vec![
            if esptool_flash_view {
                Line::from(
                    "no device data yet --- connect a board, or run Flash information from Project actions",
                )
            } else {
                Line::from("no device information for this project")
            }
            .style(muted),
        ]);
    }

    let mut lines = Vec::new();
    // The crystal rides the chip's own line as a muted suffix (the pane is
    // a fixed [`INFO_ROWS`] rows and the firmware answer below the MAC now
    // takes one); a report with a crystal but no chip keeps its own row
    // rather than losing the fact.
    if details.family.is_none()
        && let Some(crystal) = &details.crystal_mhz
    {
        lines.push(field("Crystal", crystal.clone(), palette));
    }
    if let Some(family) = details.family {
        // One row, always --- the same rule the features line below
        // follows, and for the same reason: a wrapped chip line pushes the
        // MAC and Firmware rows past the pane's fixed [`INFO_ROWS`]
        // height. At the minimum width an `ESP32-S3 (revision 3) · 40MHz`
        // report is exactly what overflows, so the suffixes are dropped
        // whole rather than wrapped --- the crystal first (it rides this
        // line only because the pane had no row to spare for it), then the
        // revision, then the chip's own name truncates as a last resort.
        let budget = width.saturating_sub(2 + LABEL_WIDTH);
        let name = family.label().to_string();
        let revision = details
            .revision
            .as_ref()
            .map(|revision| format!(" (revision {revision})"))
            .unwrap_or_default();
        let crystal = details
            .crystal_mhz
            .as_ref()
            .map(|crystal| format!(" · {crystal}"))
            .unwrap_or_default();
        let fits = |suffix: &str| name.chars().count() + suffix.chars().count() <= budget;
        let (revision, crystal) = if fits(&format!("{revision}{crystal}")) {
            (revision, crystal)
        } else if fits(&revision) {
            (revision, String::new())
        } else {
            (String::new(), String::new())
        };

        let mut spans = vec![
            label_span("Chip", palette),
            Span::styled(
                truncate_end(&name, budget),
                Style::new().fg(palette.success).bold(),
            ),
        ];
        if !revision.is_empty() {
            spans.push(Span::styled(revision, Style::new().fg(palette.fg)));
        }
        if !crystal.is_empty() {
            spans.push(Span::styled(crystal, muted_style(palette)));
        }
        lines.push(Line::from(spans));
    }
    if let Some(raw) = &details.features {
        // One row, always: a wrapped features line would push the MAC and
        // firmware rows below past the pane's fixed [`INFO_ROWS`] height.
        // esptool's own list does not remotely fit one --- a plain ESP32
        // reports 74 characters against the 27 this row has at the minimum
        // width --- so [`features::compact`] re-expresses it, most
        // identifying first, and [`features_spans`] drops whole entries off
        // the tail. The raw line is not lost: it reaches the Log pane
        // (`FlashPanel::complete`), the pairing [`short_version`] and the
        // Firmware row already use.
        let budget = width.saturating_sub(2 + LABEL_WIDTH);
        let items = features::compact(raw);
        if !items.is_empty() {
            lines.push(Line::from(features_spans(&items, budget, palette)));
        }
    }
    if let Some(mac) = &details.mac {
        lines.push(Line::from(vec![
            label_span("MAC", palette),
            Span::styled(mac.clone(), Style::new().fg(palette.fg)),
        ]));
        // The firmware answer sits directly under the MAC, the identity it
        // belongs beside, and carries the version the same read found in
        // the banner/descriptor bytes (`Zephyr v4.0.0`, `ESP-IDF v5.3.1`);
        // MicroPython keeps its second source: a read that found no
        // version string still shows the one the REPL banner gave. The
        // rest is as ever: `undefined` is the honest value while the
        // identification read has not run, or could not recognize
        // anything; a blank chip is a different, answerable condition:
        // erased flash means no firmware at all.
        let (value, style) = match &details.firmware {
            Some(FirmwareVerdict::Firmware(kind, version)) => {
                let version = version.as_deref().or(match kind {
                    FlashFirmware::MicroPython => app.mpy_version.as_deref(),
                    _ => None,
                });
                let label = match version {
                    Some(version) => format!("{} {}", kind.label(), short_version(version)),
                    None => kind.label().to_string(),
                };
                (label, Style::new().fg(palette.success).bold())
            }
            Some(FirmwareVerdict::Erased) => (
                "none (erased flash)".to_string(),
                Style::new().fg(palette.warning),
            ),
            None => ("undefined".to_string(), muted_style(palette)),
        };
        lines.push(Line::from(vec![
            label_span("Firmware", palette),
            Span::styled(value, style),
        ]));
    }

    pad_info(lines)
}

/// Row 2 placeholder for the window before a browser exists at all (a
/// fresh `bootstrap()` that has not yet run `App::maybe_scan_devices`).
/// Once the browser exists the local pane always renders, whatever the
/// backend; the right half is the device pane under
/// [`Capability::Filesystem`], else [`crate::ui::files`]'s placeholder.
pub fn draw_no_filesystem(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let block = pane_block("Files", false, palette);
    let backend = app
        .manager
        .selected_kind()
        .map_or("this backend".to_string(), |kind| kind.to_string());
    frame.render_widget(
        Paragraph::new(format!("{backend}: file browsing not implemented yet").fg(palette.muted))
            .block(block),
        area,
    );
}

/// Rolling status/log output.
///
/// Long entries wrap at the pane's width with a hanging indent past the
/// stamp, so a wrapped paragraph stays visually tied to its timestamp
/// instead of overflowing or being cut off at the terminal edge.
pub fn draw_logs(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    // The tab strip owns the border row (see `draw_log_tabs`), so the pane
    // itself carries no title.
    let focused = dashboard_focused(app, Focus::Logs);
    let block = pane_border(focused, palette);

    // Publish the usable height so page-scrolling matches the rendered view,
    // and the wrapping width so clamping matches too. One column is reserved
    // for the scrollbar whether or not it is showing, so text never reflows
    // the moment the log outgrows the pane.
    let inner = block.inner(area);
    let gutter = if inner.width > 0 { 1 } else { 0 };
    app.log_viewport = (inner.height as usize).max(1);
    app.logs
        .set_view_width(inner.width.saturating_sub(gutter) as usize);

    let lines: Vec<Line> = app
        .logs
        .visible_rows(app.log_viewport)
        .into_iter()
        .map(|row| {
            let style = match row.entry.level {
                Level::Info => Style::new().fg(palette.fg),
                Level::Success => Style::new().fg(palette.success),
                Level::Warn => Style::new().fg(palette.warning),
                Level::Error => Style::new().fg(palette.error),
            };
            if row.first {
                let centis = row.entry.at.millisecond() / 10;
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{:02}:{:02}:{:02}.{centis:02} ",
                            row.entry.at.hour(),
                            row.entry.at.minute(),
                            row.entry.at.second()
                        ),
                        muted_style(palette),
                    ),
                    Span::styled(format!("{} ", row.entry.level.marker()), style),
                    Span::styled(row.text, style),
                ])
            } else {
                // Continuation of a wrapped entry: indented past the stamp so
                // the whole paragraph reads as one timestamped line.
                Line::from(vec![
                    Span::raw(" ".repeat(PREFIX_WIDTH)),
                    Span::styled(row.text, style),
                ])
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(block).style(output_style(app)),
        area,
    );

    draw_log_scrollbar(frame, inner, app, palette);
}

/// The log pane's scrollbar: the shared bar (see [`crate::ui::draw_scrollbar`])
/// over visual (post-wrap) lines, with the thumb reflecting the scroll
/// position (bottom while following the tail).
fn draw_log_scrollbar(frame: &mut Frame, inner: Rect, app: &App, palette: Palette) {
    let total = app.logs.total_lines();
    let viewport = app.log_viewport;
    if total <= viewport {
        return;
    }
    let max_scroll = total - viewport;
    let top = max_scroll - app.logs.scroll().min(max_scroll);
    crate::ui::draw_scrollbar(frame, inner, total, viewport, top, palette);
}

/// Row 3's tab strip, drawn over the pane's own top border like the Ratatui
/// `Tabs` example: ` Log • Monitor `. `Monitor` is omitted entirely when the
/// backend has no `Capability::Monitor` --- capability-gated, never
/// backend-kind-gated (`AGENTS.md` §3). At the strip's right edge rides the
/// active tab's status: for Monitor, the source's title, a live icon (an
/// animated spinner while a command runs, a green check --- red cross on
/// failure --- for the last finished one) and the output's row count; for
/// Log, the entry count and, while scrolled, how far up the view sits.
pub fn draw_log_tabs(frame: &mut Frame, pane: Rect, app: &App, palette: Palette) {
    // The strip spans the border row between the corners; drawn after the
    // pane's own widgets so it sits on top of the border.
    let strip = Rect {
        x: pane.x.saturating_add(1),
        y: pane.y,
        width: pane.width.saturating_sub(2),
        height: 1,
    };
    if strip.width == 0 {
        return;
    }

    let focused = dashboard_focused(app, Focus::Logs);
    let has_monitor = app.manager.capabilities().contains(Capability::Monitor);

    // The active tab reads brighter than the dim inactive one even without
    // focus, and like every focused pane goes cyan while Logs holds focus;
    // the underline makes the selection unmistakable either way (a
    // background highlight would vanish against some color schemes when the
    // pane is unfocused).
    let active_style = if focused {
        Style::new()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let inactive_style = muted_style(palette);

    let monitor = vec![Span::styled(
        "Monitor",
        if app.log_tab == LogTab::Monitor {
            active_style
        } else {
            inactive_style
        },
    )];

    let titles = vec![
        Line::from(Span::styled(
            "Log",
            if app.log_tab == LogTab::Log {
                active_style
            } else {
                inactive_style
            },
        )),
        Line::from(monitor),
    ];

    let selected_index = match app.log_tab {
        LogTab::Log => Some(0),
        LogTab::Monitor => has_monitor.then_some(1),
    };

    // The selection style lives on the titles themselves; the highlight
    // style adds nothing.
    let tabs = Tabs::new(titles)
        .select(selected_index)
        .style(inactive_style)
        .highlight_style(Style::new())
        .padding(" ", " ")
        .divider(symbols::DOT);

    frame.render_widget(tabs, strip);
    frame.render_widget(
        Paragraph::new(tab_status(app, palette)).alignment(Alignment::Right),
        strip,
    );
}

/// The active tab's status, drawn at the strip's right edge. A leading
/// space keeps the pane's border dashes from touching it.
fn tab_status(app: &App, palette: Palette) -> Line<'static> {
    match app.log_tab {
        LogTab::Log => {
            let mut text = format!(" Log ({})", app.logs.total_lines());
            if !app.logs.is_following() {
                text.push_str(&format!(" \u{2191}{}", app.logs.scroll()));
            }
            Line::from(text).fg(palette.muted)
        }
        LogTab::Monitor => monitor_status(app, palette),
    }
}

/// The Monitor tab's status: the source's title with its live icon and the
/// output's row count (`App::monitor_view.rows`, published by the console's
/// renderer --- the strip draws after it, so the count is fresh).
fn monitor_status(app: &App, palette: Palette) -> Line<'static> {
    let spinner = || {
        Some((
            SPINNER[(app.ticks as usize) % SPINNER.len()],
            Style::new().fg(palette.warning),
        ))
    };
    let check = || ("\u{2713}", Style::new().fg(palette.success));
    let cross = || ("\u{2717}", Style::new().fg(palette.error));

    let (icon, title): (Option<(&str, Style)>, String) = match app.monitor_source {
        MonitorSource::Build => match app.build.as_ref() {
            Some(panel) => {
                if let Some(label) = panel.running_label() {
                    (spinner(), label.to_string())
                } else if let Some(report) = panel.last.as_ref() {
                    let icon = if report.ok { check() } else { cross() };
                    (Some(icon), report.what.to_string())
                } else {
                    (None, "Build".to_string())
                }
            }
            None => (None, "Monitor".to_string()),
        },
        MonitorSource::Flash => match app.flash.as_ref() {
            Some(flash) => {
                let icon = match &flash.state {
                    RunState::Running => spinner(),
                    RunState::Succeeded => Some(check()),
                    RunState::Failed(_) => Some(cross()),
                    RunState::Idle => None,
                };
                let action = flash.selected_action();
                let title = if action.needs_firmware() {
                    action.label().to_string()
                } else {
                    "Monitor".to_string()
                };
                (icon, title)
            }
            None => (None, "Monitor".to_string()),
        },
        MonitorSource::Run => {
            let icon = match app.run_state {
                crate::app::RunState::Running => spinner(),
                crate::app::RunState::Finished => Some(check()),
                crate::app::RunState::Idle => None,
            };
            let title = app.run_script.as_ref().map_or_else(
                || "Run".to_string(),
                |path| format!("Run: {}", path.display()),
            );
            (icon, title)
        }
        MonitorSource::Device => {
            let icon = if app.device_monitor_process.is_some() {
                spinner()
            } else {
                None
            };
            (icon, "Monitor".to_string())
        }
    };

    let mut spans = vec![Span::raw(" ")];
    if let Some((icon, style)) = icon {
        spans.push(Span::styled(icon.to_string(), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(title, muted_style(palette)));
    spans.push(Span::styled(
        format!(" ({})", app.monitor_view.rows),
        muted_style(palette),
    ));
    // The scrolled indicator Log shows: `↑N` with the rows *below* the view
    // (the same meaning as `LogStore::scroll`), shown only once the user
    // leaves the tail.
    if !app.monitor_scroll.following {
        let below = app
            .monitor_view
            .rows
            .saturating_sub(app.monitor_view.viewport + app.monitor_scroll.offset);
        spans.push(Span::styled(
            format!(" \u{2191}{below}"),
            muted_style(palette),
        ));
    }
    Line::from(spans)
}

/// Width of the field-label column, including its colon and trailing space.
const LABEL_WIDTH: usize = 11;

fn label_span(label: &str, palette: Palette) -> Span<'static> {
    Span::styled(
        format!("{:<LABEL_WIDTH$}", format!("{label}:")),
        muted_style(palette),
    )
}

/// Shortens `text` from the left, keeping the tail.
///
/// Paths are truncated at the front because the last components --- the project
/// directory --- are what identify it.
pub(super) fn truncate_start(text: &str, max: usize) -> String {
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

/// Shortens `text` from the right, keeping the head --- the sibling of
/// [`truncate_start`] for lists whose first items are the identity.
fn truncate_end(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{head}…")
}

/// The version's display form: the semver-ish prefix only, cutting the
/// git-describe suffix (`-N-gHASH`, `-N.gHASH`) a dev build's banner
/// carries. `firmware_id::version()` keeps the full string --- the log line
/// that reports it (`{kind} build {version}`, `src/flash.rs`) still gets it
/// whole; only the Firmware row's fixed line is shortened.
fn short_version(version: &str) -> &str {
    version.split('-').next().unwrap_or(version)
}

/// What separates two feature entries, muted like the chip line's own ` · `
/// crystal suffix. esptool's own separator, and a column shorter than the
/// ` · ` the rest of this pane uses --- on a row this tight that column is
/// worth more than the consistency.
const FEATURE_SEPARATOR: &str = ", ";

/// Columns `items` occupy once joined by [`FEATURE_SEPARATOR`].
fn features_width(items: &[features::Item]) -> usize {
    items.iter().map(features::Item::width).sum::<usize>()
        + FEATURE_SEPARATOR.chars().count() * items.len().saturating_sub(1)
}

/// The features row: entries in [`features::compact`]'s priority order,
/// dropped **whole** off the tail until they fit `budget`.
///
/// The same rule the chip line above follows with its crystal and revision
/// suffixes, for the same reason --- half an entry says less than none --- and
/// with the same last resort: only when the leading entry alone overruns does
/// anything truncate. An entry esptool printed but this build does not
/// recognise is muted, so what a narrow row loses is the trivia, not the
/// radios.
fn features_spans(items: &[features::Item], budget: usize, palette: Palette) -> Vec<Span<'static>> {
    let mut shown = items.len();
    while shown > 1 && features_width(&items[..shown]) > budget {
        shown -= 1;
    }

    let mut spans = vec![label_span("Features", palette)];
    for (index, item) in items[..shown].iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(FEATURE_SEPARATOR, muted_style(palette)));
        }
        let style = if item.muted {
            muted_style(palette)
        } else {
            Style::new().fg(palette.fg)
        };
        let text = if shown == 1 {
            truncate_end(&item.text, budget)
        } else {
            item.text.clone()
        };
        spans.push(Span::styled(text, style));
    }
    spans
}

fn field(label: &str, value: String, palette: Palette) -> Line<'static> {
    Line::from(vec![
        label_span(label, palette),
        Span::styled(value, Style::new().fg(palette.fg)),
    ])
}

fn field_styled(label: &str, value: String, style: Style, palette: Palette) -> Line<'static> {
    Line::from(vec![label_span(label, palette), Span::styled(value, style)])
}

#[cfg(test)]
mod tests {
    use super::{short_version, truncate_start};

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

    #[test]
    fn a_git_describe_suffix_is_cut() {
        assert_eq!(short_version("v4.4.0-11847-gc5dffcb7c9da"), "v4.4.0");
        assert_eq!(short_version("v1.25.0-123.g0123abcdef"), "v1.25.0");
    }

    #[test]
    fn a_bare_version_is_left_alone() {
        assert_eq!(short_version("v5.3.1"), "v5.3.1");
        assert_eq!(short_version("v1"), "v1");
    }
}
