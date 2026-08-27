//! Dual-pane file browser rendering.
//!
//! Two independently navigable panes, as in `mc`. The status column between
//! name and size is the comparison: it is computed against whatever the *other*
//! pane currently shows, so the panes can sit at unrelated paths and the
//! answer is still meaningful.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::app::{App, Focus};
use crate::browser::{Browser, PaneState};
use crate::device::{DiscoveryState, ScriptState};
use crate::files::SyncStatus;
use crate::ui::panels::{truncate_end, truncate_start};
use crate::ui::{
    Palette, SPINNER, border_style, content_style, dashboard_focused, draw_scrollbar,
    highlighted_line, icon_column, muted_style, numbered_title, paint_focus_wash, pane_block,
    pane_border, pane_title, render_pane, selection_style, shortcut_highlight_style,
    shortcut_letter,
};

pub fn draw(frame: &mut Frame, row: &super::layout::BrowserRow, app: &mut App, palette: Palette) {
    let Some(browser) = &app.browser else {
        // `layout::dashboard` only builds a `Browser` row when the browser
        // exists; this is the defensive half of that contract.
        let title = pane_title(app.icon_set().folder(), "Files");
        let block = pane_block(&title, false, palette, None);
        frame.render_widget(
            Paragraph::new("the file listing has not started yet".fg(palette.muted)).block(block),
            row.local,
        );
        return;
    };

    let statuses = browser.statuses();
    draw_local(frame, row.local, app, &statuses, palette);
    match row.right_kind {
        super::layout::RightKind::Device => {
            draw_device(frame, row.right, app, &statuses, palette);
        }
        super::layout::RightKind::Build => super::build::draw(frame, row.right, app, palette),
        super::layout::RightKind::NoDevice => draw_no_device(frame, row.right, app, palette),
    }
    if let Some(legend) = row.legend {
        draw_legend(frame, legend, palette);
    }
}

/// The right half of row 2 for a backend with no
/// [`Capability::Filesystem`](crate::backend::Capability::Filesystem)
/// (today: Zephyr): there is no device filesystem to browse, and this is the
/// space its build panel will occupy. Kept capability-gated, never
/// backend-kind-gated (`AGENTS.md` §3).
fn draw_no_device(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let backend = app
        .manager
        .selected_kind()
        .map_or("this backend".to_string(), |kind| kind.to_string());
    let title = pane_title(app.icon_set().folder(), "Device");
    let block = pane_block(&title, false, palette, None);
    frame.render_widget(
        Paragraph::new(format!("{backend}: no device filesystem").fg(palette.muted)).block(block),
        area,
    );
}

fn draw_local(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    statuses: &BTreeMap<String, SyncStatus>,
    palette: Palette,
) {
    let focused = dashboard_focused(app, Focus::FilesLocal);
    let icons = app.icon_set();
    // One immutable pass builds the title, block and rows (the offset seed
    // included), so the mutable write-back below cannot fight a live borrow.
    let (block, items, cursor, seed) = {
        let Some(browser) = app.browser.as_ref() else {
            return;
        };
        // The pane is the project's own files: the title names the project
        // and the path walked below it ("blinkety/src/"), the same shape the
        // Zephyr project-files pane's title follows. A walk that rose *above*
        // the project root (the pane may ascend anywhere) falls back to the
        // full path --- honest about where the listing actually is.
        let project = browser
            .local_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| crate::ui::tilde_path(&browser.local_root, app.home_dir()));
        let walked = browser
            .local_path
            .strip_prefix(&browser.local_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| format!("{project}/{}/", relative.display()))
            .unwrap_or(format!("{project}/"));
        let title = if browser.local_path.starts_with(&browser.local_root) {
            walked
        } else {
            crate::ui::tilde_path(&browser.local_path, app.home_dir())
        };
        // The prefix never truncates --- only the path shortens, from the
        // left, and the stop number's two cells are budgeted off the path,
        // not taken from the title's tail.
        let stop = app.pane_number(Focus::FilesLocal);
        let budget = area.width.saturating_sub(stop.map_or(0, |_| 2));
        let title = numbered_title(
            app,
            Focus::FilesLocal,
            icons.folder(),
            &format!("Files: {}", shorten(&title, budget)),
        );
        let block = pane_block(&title, focused, palette, shortcut_letter(app, 'f'));

        if let Some(error) = &browser.local_error {
            // The wash goes under the paragraph: its text carries fg only, so
            // the error pane keeps the focused tint too.
            paint_focus_wash(frame, block.inner(area), focused, palette);
            frame.render_widget(
                Paragraph::new(error.clone().fg(palette.error))
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        // Rows build against the scrollbar's reserved column (see
        // `render_list`), so the size column keeps its place beside the
        // border whether or not a bar is showing.
        let row_width = list_view(block.inner(area)).width;
        let items: Vec<ListItem> = browser
            .visible_local()
            .into_iter()
            .map(|entry| {
                row(
                    &entry.name,
                    entry.is_dir,
                    entry.size,
                    statuses.get(&entry.name).copied(),
                    row_width,
                    icons,
                    palette,
                )
            })
            .collect();
        (block, items, browser.local_cursor, browser.local_offset)
    };

    let inner = render_pane(frame, area, block, focused, palette);
    let settled = render_list(frame, inner, items, Some(cursor), seed, focused, palette);
    if let Some(browser) = app.browser.as_mut() {
        browser.local_offset = settled;
    }
}

fn draw_device(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    statuses: &BTreeMap<String, SyncStatus>,
    palette: Palette,
) {
    let focused = dashboard_focused(app, Focus::FilesDevice);
    let icons = app.icon_set();
    // The block is built under an immutable borrow (the title reads the
    // walked device path), released before the content path borrows `app`
    // mutably to publish the list's settled offset.
    let (block, tabbed) = {
        let Some(browser) = app.browser.as_ref() else {
            return;
        };
        // The running-script flag rides along in the title rather than the
        // body: it explains why an overlay may appear, without claiming
        // list space.
        let mut title = format!("Device Files: {}", browser.device_path);
        if app.devices.script_state() == ScriptState::Running {
            title.push_str(" · script running");
        }
        let title = numbered_title(app, Focus::FilesDevice, icons.folder(), &title);
        // A backend that can flash or erase gets the pane the flash menu
        // moved into: the border row carries the `Actions • Device Files`
        // tab strip (row 3's grammar) and the walked path rides the strip's
        // right edge as the active tab's status; anything else keeps the
        // plain titled pane the pane has always been.
        let tabbed = app.device_actions_tab_available();
        let block = if tabbed {
            pane_border(focused, palette)
        } else {
            pane_block(&title, focused, palette, shortcut_letter(app, 'd'))
        };
        (block, tabbed)
    };
    let inner = render_pane(frame, area, block, focused, palette);

    if tabbed && app.device_actions_tab_active() {
        super::flash::draw_actions_pane(frame, inner, app, palette);
    } else {
        draw_device_content(frame, inner, app, statuses, palette);
    }
    if tabbed {
        draw_device_tabs(frame, area, app, palette);
    }
}

/// The tab strip over the device pane's own top border (the same Ratatui
/// `Tabs` pattern row 3 uses): `Actions • Device Files`, the
/// active tab bold and underlined --- cyan while the pane holds focus,
/// like every focused pane. At the strip's right edge rides the *active*
/// tab's status: for the files tab the walked device path with the
/// running-script flag --- the facts the pane's old title carried,
/// displaced by the strip but still one glance away --- and for the
/// actions tab the flag alone, since there is no listing to locate there,
/// but a running script still gates every esptool action.
fn draw_device_tabs(frame: &mut Frame, pane: Rect, app: &App, palette: Palette) {
    let strip = Rect {
        x: pane.x.saturating_add(1),
        y: pane.y,
        width: pane.width.saturating_sub(2),
        height: 1,
    };
    if strip.width == 0 {
        return;
    }
    let Some(browser) = app.browser.as_ref() else {
        return;
    };

    let focused = dashboard_focused(app, Focus::FilesDevice);
    // As in row 3's strip: while the shortcuts overlay is up, which tab is
    // active stops mattering next to which letter jumps here.
    let active_style = if app.shortcuts_overlay_active {
        muted_style(palette)
    } else if focused {
        Style::new()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let inactive_style = muted_style(palette);
    let highlight_style = shortcut_highlight_style(palette);
    let actions_tab = app.device_actions_tab_active();

    let mut titles = vec![
        highlighted_line(
            &pane_title(app.icon_set().bolt(), "Actions"),
            if actions_tab {
                active_style
            } else {
                inactive_style
            },
            highlight_style,
            shortcut_letter(app, 'a'),
        ),
        highlighted_line(
            &pane_title(app.icon_set().folder(), "Device Files"),
            if actions_tab {
                inactive_style
            } else {
                active_style
            },
            highlight_style,
            shortcut_letter(app, 'd'),
        ),
    ];
    // The pane's stop number rides the strip's leading edge --- a span of
    // the frame's own muted style, deliberately unlike either tab's label
    // style, because it names the *pane* (one stop), not the tab it
    // happens to sit beside. `mouse::strip_tab` budgets these cells too.
    if let Some(number) = app.pane_number(Focus::FilesDevice) {
        let number_span = Span::styled(format!("{number} "), muted_style(palette));
        titles[0] = Line::from(
            std::iter::once(number_span)
                .chain(titles[0].spans.iter().cloned())
                .collect::<Vec<_>>(),
        );
    }
    // The base style is the border's (`border_style`), not the inactive
    // label's: the strip paints the pane's whole top border row, which must
    // keep reading as the frame it belongs to (accent while focused).
    let tabs = Tabs::new(titles)
        .select(Some(usize::from(!actions_tab)))
        .style(border_style(focused, palette))
        .highlight_style(Style::new())
        .padding(" ", " ")
        .divider(symbols::DOT);
    frame.render_widget(tabs, strip);

    // The walked path rides the strip's right edge --- except at the root:
    // every listing starts there, so a lone `/` at the pane's far end
    // locates nothing and reads as a stray mark, not a path.
    let mut status = if actions_tab || browser.device_path.is_root() {
        String::new()
    } else {
        browser.device_path.to_string()
    };
    if app.devices.script_state() == ScriptState::Running {
        if !status.is_empty() {
            status.push_str(" · ");
        }
        status.push_str("script running");
    }
    if status.is_empty() {
        return;
    }
    // The status never takes more than half the strip from the tabs; a
    // path that long is shortened from the left, its tail being the part
    // that identifies where the listing is. The leading space keeps it off
    // the border's dashes, truncated or not.
    let budget = (strip.width as usize / 2).max(8);
    let status = format!(" {}", truncate_start(&status, budget - 1));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(status, muted_style(palette))))
            .alignment(Alignment::Right),
        strip,
    );
}

/// The device listing itself (idle/loading/failed/ready), into the pane's
/// inner rect --- the border and, for a flash-capable backend, the tab
/// strip on it are drawn by [`draw_device`].
fn draw_device_content(
    frame: &mut Frame,
    inner: Rect,
    app: &mut App,
    statuses: &BTreeMap<String, SyncStatus>,
    palette: Palette,
) {
    let focused = dashboard_focused(app, Focus::FilesDevice);
    let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];
    let icons = app.icon_set();

    // The non-Ready states draw no rows; the offset is left as it was, the
    // same way the cursor is. Rows and seed are built under the immutable
    // borrow, released before the settled offset is written back.
    let (list_area, footer_area, items, cursor, seed) = {
        let Some(browser) = app.browser.as_ref() else {
            return;
        };
        match &browser.device_state {
            PaneState::Idle => {
                frame.render_widget(
                    Paragraph::new("press 'd' to look for a device".fg(palette.muted)),
                    inner,
                );
                return;
            }
            PaneState::Loading => {
                // Finding the board, waiting on the user and listing it are
                // all waits, but they fail or stall for different reasons,
                // so they are named differently.
                let what = if browser.held_for_interrupt() {
                    "waiting to interrupt a running script".to_string()
                } else if app.devices.discovery == DiscoveryState::Scanning {
                    "searching for a device".to_string()
                } else {
                    format!("listing {}", browser.device_path)
                };
                frame.render_widget(
                    Paragraph::new(format!("{spinner} {what}…").fg(palette.muted)),
                    inner,
                );
                return;
            }
            PaneState::Failed(error) => {
                frame.render_widget(
                    Paragraph::new(error.clone().fg(palette.error)).wrap(Wrap { trim: true }),
                    inner,
                );
                return;
            }
            PaneState::Ready => {}
        }

        let [list_area, footer_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        let row_width = list_view(list_area).width;
        let items: Vec<ListItem> = browser
            .visible_device()
            .into_iter()
            .map(|entry| {
                row(
                    &entry.name,
                    entry.is_dir,
                    entry.size,
                    statuses.get(&entry.name).copied(),
                    row_width,
                    icons,
                    palette,
                )
            })
            .collect();
        (
            list_area,
            footer_area,
            items,
            browser.device_cursor,
            browser.device_offset,
        )
    };

    let settled = render_list(
        frame,
        list_area,
        items,
        Some(cursor),
        seed,
        focused,
        palette,
    );
    if let Some(browser) = app.browser.as_ref() {
        draw_device_footer(frame, footer_area, browser, palette);
    }
    if let Some(browser) = app.browser.as_mut() {
        browser.device_offset = settled;
    }
}

/// Free space on the connected board, as a progress bar --- filled by the
/// used fraction, colored green/yellow/red at 70%/90% used so it doubles as
/// an early warning before an upload runs out of room.
fn draw_device_footer(frame: &mut Frame, area: Rect, browser: &Browser, palette: Palette) {
    match &browser.device_space {
        Some(Ok(usage)) if usage.total > 0 => {
            // `Gauge::ratio` panics outside 0.0..=1.0; `total > 0` above and the
            // clamp here keep a malformed or stale reading from crashing the UI.
            let used_ratio = (usage.used as f64 / usage.total as f64).clamp(0.0, 1.0);
            let color = if used_ratio >= 0.9 {
                palette.error
            } else if used_ratio >= 0.7 {
                palette.warning
            } else {
                palette.success
            };
            // `Gauge::label` is always centered, with no alignment option ---
            // left-padding the text to the full width forces it flush right
            // instead, matching the local pane's footer.
            let label = format!(
                "total: {}/{}",
                human_size(usage.used),
                human_size(usage.total)
            );
            let label = format!("{label:>width$}", width = area.width as usize);
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::new().fg(color))
                    .ratio(used_ratio)
                    .label(label),
                area,
            );
        }
        Some(Ok(_)) => {
            frame.render_widget(Paragraph::new("free space: 0".fg(palette.muted)), area);
        }
        Some(Err(_)) => {
            frame.render_widget(
                Paragraph::new("free space unavailable".fg(palette.muted)),
                area,
            );
        }
        None => {
            frame.render_widget(
                Paragraph::new("checking free space…".fg(palette.muted)),
                area,
            );
        }
    }
}

/// The list viewport inside a file pane's area: one column narrower, that
/// column kept reserved for the scrollbar [`render_list`] draws --- the same
/// rule the Log pane's wrap width and the docs picker's list follow, so a
/// row's flush-right size column never shifts when a bar appears. Callers
/// build their [`row`]s against this width and pass `area` itself to
/// [`render_list`].
pub(super) fn list_view(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// Renders `items` as a navigable list with the shared one-column scrollbar
/// riding its reserved edge column (see [`list_view`]). `cursor` is the
/// selected row's index, or `None` when this pane's cursor is currently
/// elsewhere (the Zephyr workspace pane's embedded file list draws with `None`
/// while the checklist above it has the cursor, so no row highlights).
///
/// `offset` seeds the scroll with the value the previous frame settled on,
/// and the newly settled offset is returned for the caller to publish back
/// --- the docs picker's own rule (`App::docs_list_offset`): with a fresh
/// state every frame the offset is recomputed from zero, anchoring any
/// selection past half the pane at the bottom edge, so a click on a visible
/// row jumped the list downward to re-anchor it. Seeded, the offset only
/// moves when the selection leaves the window, and the returned value is
/// what the scrollbar reports and what a click maps its row through.
pub(super) fn render_list(
    frame: &mut Frame,
    area: Rect,
    items: Vec<ListItem<'static>>,
    cursor: Option<usize>,
    offset: usize,
    focused: bool,
    palette: Palette,
) -> usize {
    if items.is_empty() {
        frame.render_widget(Paragraph::new("empty".fg(palette.muted)), area);
        return 0;
    }

    let mut state = ListState::default()
        .with_offset(offset)
        .with_selected(cursor);
    let total = items.len();
    frame.render_stateful_widget(
        List::new(items)
            .style(content_style(focused))
            .highlight_style(selection_style(palette)),
        list_view(area),
        &mut state,
    );
    let settled = state.offset();
    draw_scrollbar(frame, area, total, area.height as usize, settled, palette);
    settled
}

/// One entry: `<status> <icon> <name> <size>`, with the size flush right.
/// The status marker exists only when there is another side to compare
/// against; `None` (the workspace pane's lone directory list) draws just
/// `<icon> <name> <size>` --- a list with no comparison states no verdict.
/// The `none` icon set drops the `<icon>` column whole (see [`icon`]).
pub(super) fn row(
    name: &str,
    is_dir: bool,
    size: u64,
    status: Option<SyncStatus>,
    width: u16,
    icons: crate::icons::IconSet,
    palette: Palette,
) -> ListItem<'static> {
    ListItem::new(Line::from(row_spans(
        name, is_dir, size, status, width, icons, palette,
    )))
}

/// The spans [`row`] stacks, split out so the width test can assert them
/// (a `ListItem`'s content is private to ratatui).
fn row_spans(
    name: &str,
    is_dir: bool,
    size: u64,
    status: Option<SyncStatus>,
    width: u16,
    icons: crate::icons::IconSet,
    palette: Palette,
) -> Vec<Span<'static>> {
    let size_text = if is_dir {
        "DIR".to_string()
    } else {
        human_size(size)
    };

    // Every non-Nerd icon here is a *dedicated* pictograph codepoint
    // (`Emoji_Presentation=Yes`, never a dual-presentation symbol forced
    // wide with a variation selector --- see `icon`'s doc), so every
    // terminal draws it two cells wide the same way `unicode-width` scores
    // it: no glyph here can disagree with the terminal about its own
    // width. The column is fixed at 2 cells + the trailing space, and the
    // Nerd set's width-1 glyphs (`icon_column`) center into it so a `.py`
    // row lines up with the emoji rows beside it. A marker costs 2 more.
    // The `none` icon set drops the column (and its 3 cells) whole.
    let icon_width = usize::from(icons.shows_decorations()) * 3;
    let marker_width = usize::from(status.is_some()) * 2;
    let name_width =
        (width as usize).saturating_sub(icon_width + marker_width + size_text.len() + 1);
    let display = truncate_end(name, name_width.max(1));
    let padding = name_width.saturating_sub(display.chars().count());

    let name_style = if is_dir {
        Style::new().fg(palette.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(palette.fg)
    };

    let mut spans = Vec::with_capacity(5);
    if let Some(status) = status {
        spans.push(Span::styled(
            format!("{} ", status.marker()),
            status_style(status, palette),
        ));
    }
    if icons.shows_decorations() {
        let (glyph, single_cell) = icon(name, is_dir, icons);
        spans.push(Span::styled(
            icon_column(glyph, single_cell),
            Style::new().fg(palette.fg),
        ));
    }
    spans.push(Span::styled(display, name_style));
    spans.push(Span::raw(" ".repeat(padding + 1)));
    spans.push(Span::styled(size_text, muted_style(palette)));
    spans
}

/// A glyph hinting at the entry's kind: a folder, or a common extension's
/// language/purpose. Purely cosmetic --- unknown extensions fall back to a
/// generic file glyph rather than nothing, so the column stays aligned.
/// A `.py` file follows the backend's own mark under the Nerd set
/// ([`IconSet::python`](crate::icons::IconSet::python)), and a C-family
/// file follows [`IconSet::c_lang`] the same way: the file list reads in
/// the same vocabulary the header and the home rows do, instead of mixing
/// an emoji logo into a nerd-rendered UI. Every other extension keeps its
/// emoji in every set (none of them has a dedicated Nerd glyph in the
/// project's vetted `custom-*`/seti range --- see [`IconSet::c_lang`]'s
/// doc), and the `none` set never reaches here --- the whole column is
/// decoration and hides first (`shows_decorations`).
///
/// Every emoji here is picked to be `Emoji_Presentation=Yes` on its own
/// (never a dual-presentation symbol needing a variation selector to force
/// width --- `⚙️`/`👁️`/`🗑️` used to be exactly that, and different
/// terminals disagreed with `unicode-width` about their column count; see
/// `row_spans`' doc). The kind glyph plus whether it draws a single cell
/// wide --- true only for the Nerd set's language logos,
/// [`icon_column`]'s caller-declared width rather than a guess from the
/// codepoint.
fn icon(name: &str, is_dir: bool, icons: crate::icons::IconSet) -> (&'static str, bool) {
    if is_dir {
        return ("📁", false);
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase);
    match ext.as_deref() {
        Some("py") => match icons {
            crate::icons::IconSet::Nerd => (icons.python(), true),
            _ => ("🐍", false),
        },
        Some("c" | "h" | "cc" | "cpp" | "hpp") => match icons {
            crate::icons::IconSet::Nerd => (icons.c_lang(), true),
            _ => ("🔧", false),
        },
        Some("rs") => ("🦀", false),
        Some("dts" | "dtsi" | "overlay") => ("🔌", false),
        Some("md" | "rst") => ("📝", false),
        Some("conf" | "cfg" | "ini" | "toml" | "yaml" | "yml" | "json") => ("🔩", false),
        Some("sh") => ("🐚", false),
        _ => ("📄", false),
    }
}

fn status_style(status: SyncStatus, palette: Palette) -> Style {
    match status {
        SyncStatus::Identical => Style::new().fg(palette.success),
        SyncStatus::SameSize => Style::new().fg(palette.success).dim(),
        SyncStatus::Differs => Style::new().fg(palette.warning),
        SyncStatus::LocalOnly => Style::new().fg(palette.accent),
        SyncStatus::DeviceOnly => Style::new().fg(palette.secondary),
        SyncStatus::Directory => muted_style(palette),
        SyncStatus::TypeMismatch => Style::new().fg(palette.error),
    }
}

fn draw_legend(frame: &mut Frame, area: Rect, palette: Palette) {
    let entries = [
        (SyncStatus::Identical, "identical"),
        (SyncStatus::SameSize, "same size"),
        (SyncStatus::Differs, "differs"),
        (SyncStatus::LocalOnly, "local only"),
        (SyncStatus::DeviceOnly, "device only"),
    ];

    let mut spans = Vec::new();
    for (status, label) in entries {
        spans.push(Span::styled(
            format!(" {} ", status.marker()),
            status_style(status, palette),
        ));
        spans.push(Span::styled(format!("{label}  "), muted_style(palette)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
        area,
    );
}

/// Sizes in the width a file pane can spare.
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;

    if bytes >= MIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1}k", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes}")
    }
}

/// Shortens the pane title from the left: 7 is "Files: ".len(), plus 2
/// for the leading glyph and its gap --- budgeted for the worst icon set,
/// so a long path never rides over the pane's corner in any of them.
fn shorten(path: &str, width: u16) -> String {
    truncate_start(path, (width as usize).saturating_sub(9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons::IconSet;

    #[test]
    fn sizes_are_scaled_for_narrow_columns() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(139), "139");
        assert_eq!(human_size(2048), "2.0k");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    #[test]
    fn each_status_gets_its_own_colour() {
        // The marker alone must not be the only cue, for narrow terminals and
        // for anyone reading at a glance.
        let palette = ratatui_themes::ThemeName::TokyoNight.palette();
        assert_ne!(
            status_style(SyncStatus::LocalOnly, palette),
            status_style(SyncStatus::DeviceOnly, palette)
        );
        assert_ne!(
            status_style(SyncStatus::Differs, palette),
            status_style(SyncStatus::Identical, palette)
        );
    }

    #[test]
    fn a_directory_gets_the_folder_icon_regardless_of_name() {
        assert_eq!(icon("src", true, IconSet::Unicode), ("📁", false));
        assert_eq!(icon("main.py", true, IconSet::Unicode), ("📁", false));
    }

    #[test]
    fn known_extensions_get_a_distinct_icon() {
        assert_eq!(icon("main.py", false, IconSet::Unicode), ("🐍", false));
        assert_eq!(icon("readme.TXT", false, IconSet::Unicode), ("📄", false));
        assert_eq!(icon("prj.conf", false, IconSet::Unicode), ("🔩", false));
        assert_eq!(
            icon("board.overlay", false, IconSet::Unicode),
            ("🔌", false)
        );
    }

    /// A `.py` row borrows the backend's own mark under the Nerd set ---
    /// the same Python logo the header and the home rows carry, single
    /// cell wide --- and a C-family row follows [`IconSet::c_lang`] the
    /// same way, while every other extension keeps its two-cell emoji
    /// there (neither has a dedicated Nerd glyph in the project's vetted
    /// range).
    #[test]
    fn a_py_or_c_file_follows_its_language_mark_under_the_nerd_set() {
        assert_eq!(
            icon("main.py", false, IconSet::Nerd),
            ("\u{E73C}", true),
            "the Python logo, straight off the backend's mark"
        );
        assert_eq!(
            icon("main.c", false, IconSet::Nerd),
            ("\u{E61E}", true),
            "nf-custom-c"
        );
        assert_eq!(icon("device.h", false, IconSet::Nerd), ("\u{E61E}", true));
        assert_eq!(icon("lib.rs", false, IconSet::Nerd), ("🦀", false));
        assert_eq!(icon("firmware.bin", false, IconSet::Nerd), ("📄", false));
    }

    #[test]
    fn every_icon_reserves_the_same_column_and_leaves_the_name_its_space() {
        // The icon column is budgeted for one two-cell emoji regardless of
        // the codepoint's unicode-width score --- historically a dual-
        // presentation glyph (the old `⚙️`, scored 1 by `unicode-width`
        // but meant to draw 2) glued the name onto the icon; `icon` now
        // only ever picks `Emoji_Presentation=Yes` codepoints, so the
        // score and the intended width always agree, but the fixed budget
        // stays as the belt-and-suspenders guard. Whatever the entry, the
        // drawn spans stay inside `width` and the name keeps at least one
        // space before the size.
        let palette = ratatui_themes::ThemeName::TokyoNight.palette();
        for name in [
            "src",
            "main.py",
            "prj.conf",
            "west.yml",
            "app.json",
            "Kconfig",
            "firmware.bin",
        ] {
            let is_dir = name == "src";
            let spans = row_spans(
                name,
                is_dir,
                128,
                None,
                40,
                crate::icons::IconSet::Unicode,
                palette,
            );
            assert!(
                spans[0].content.ends_with(' '),
                "{name}: the icon column must end in a space (got {:?})",
                spans[0].content
            );
            let total: usize = spans.iter().map(|span| span.width()).sum();
            assert!(total <= 40, "{name}: row is {total} wide, beyond the pane");
        }
    }

    #[test]
    fn a_nerd_python_logo_reserves_the_same_three_cells_as_an_emoji() {
        // The Nerd set's glyph is a width-1 Private Use Area codepoint
        // where every emoji is two cells; unpadded, a `.py` row shifted
        // its name one cell left of the emoji rows beside it.
        let palette = ratatui_themes::ThemeName::TokyoNight.palette();
        let spans = row_spans(
            "main.py",
            false,
            128,
            None,
            40,
            crate::icons::IconSet::Nerd,
            palette,
        );
        assert_eq!(
            spans[0].width(),
            3,
            "the Python logo pads into the fixed icon column: {:?}",
            spans[0].content
        );
        let emoji = row_spans(
            "lib.rs",
            false,
            128,
            None,
            40,
            crate::icons::IconSet::Nerd,
            palette,
        );
        assert_eq!(emoji[0].width(), 3, "the emoji column is three cells");
    }

    #[test]
    fn the_none_icon_set_drops_the_emoji_column_whole() {
        let palette = ratatui_themes::ThemeName::TokyoNight.palette();
        let spans = row_spans(
            "main.py",
            false,
            128,
            None,
            40,
            crate::icons::IconSet::None,
            palette,
        );
        // No icon span at all: the row starts straight on the name, and
        // the three cells the icon column used to budget belong to the
        // name now (still inside the width).
        assert!(
            spans[0].content.starts_with("main.py"),
            "the name leads: {:?}",
            spans[0].content
        );
        let total: usize = spans.iter().map(|span| span.width()).sum();
        assert!(total <= 40, "row is {total} wide, beyond the pane");
    }

    #[test]
    fn an_unknown_or_missing_extension_falls_back_to_a_generic_file_icon() {
        assert_eq!(icon("Kconfig", false, IconSet::Unicode), ("📄", false));
        assert_eq!(icon("firmware.bin", false, IconSet::Unicode), ("📄", false));
    }
}
