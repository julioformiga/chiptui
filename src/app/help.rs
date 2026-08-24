//! The keybinding table: one declaration per binding, consumed by *both*
//! surfaces that show keys --- the help overlay (`?`) renders the rows,
//! the contextual footer renders each row's [`Site`]s --- so footer and
//! help cannot drift apart. Adding or changing a binding means editing one
//! row here and both pick it up; the footer's context filtering is data
//! ([`Site`]), not a hand-maintained twin of the help list.
//!
//! [`HelpSection::Navigation`] holds the movement keys as plain rows;
//! [`HelpSection::Commands`] is the select --- arrows move, `Enter`
//! activates the row by replaying its key (`HelpBinding::event`) once the
//! help has closed, so the list doubles as a launcher. Help follows the
//! screen (see [`View`]) and narrows under a `/` filter (the same grammar
//! the board picker uses): the dashboard alone lists thirty-two rows, so
//! search is the way through them.
//!
//! The descriptions are part of the data, not the rendering: each is
//! summarized to fit its row on one line at the width the table needs (the
//! widest is 49 columns against a 21-column key column, so the popup fits a
//! stock 80-column terminal). The renderer truncates rather than wraps only
//! as a fallback for terminals narrower than the table.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::{Focus, LogTab, View};
use crate::backend::{Capabilities, Capability};
use crate::flash::FlashScreen;

/// One of the two divisions the help bindings are split into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpSection {
    /// Moving around: focus, scrolling, directories, screens. Rendered as
    /// plain rows --- these keys move the cursor itself, so they are never
    /// part of the select.
    Navigation,
    /// The actions: detection, files, build, device. The select: arrows
    /// move, `Enter` activates the row's key.
    Commands,
}

impl HelpSection {
    pub const ALL: [HelpSection; 2] = [HelpSection::Navigation, HelpSection::Commands];

    pub fn title(self) -> &'static str {
        match self {
            HelpSection::Navigation => "Navigation",
            HelpSection::Commands => "Commands",
        }
    }
}

/// One key binding: the keys that trigger it, what they do, and --- for the
/// select's rows --- the event `Enter` replays to activate it. `None` rows
/// (the help toggle, plain typing) have no sensible replay and simply close
/// the overlay.
///
/// `sites` is the footer half of the same declaration: every context in
/// which the binding is live, and what the footer calls it there. One row
/// can have several sites with different labels --- `enter` is "menu" on
/// the file columns and "run / stop" in the build pane.
#[derive(Debug, Clone, Copy)]
pub struct HelpBinding {
    pub key: &'static str,
    pub description: &'static str,
    pub event: Option<(KeyCode, KeyModifiers)>,
    pub sites: &'static [Site],
}

/// One context a [`HelpBinding`] is live in, as the footer shows it: the
/// key label and one-line description the footer renders, and the
/// conditions under which it applies. `rank` is the footer position ---
/// a number, so the ordering lives with the row instead of in the code
/// that assembles the footer.
#[derive(Debug, Clone, Copy)]
pub struct Site {
    pub label: &'static str,
    pub short: &'static str,
    pub rank: u8,
    /// The focused panes this site applies to; empty means any.
    pub foci: &'static [Focus],
    /// Capabilities of which *any one* must be present; empty means none
    /// are required.
    pub caps: &'static [Capability],
    pub when: When,
}

/// The extra state beyond view/focus/capabilities that a [`Site`] can
/// depend on. Kept as data so the whole table stays declarative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// A script is running on the device (`ctrl+c` interrupts it instead
    /// of quitting).
    RunActive,
    /// The Monitor tab is showing a captured run output (so `s` can save
    /// it).
    RunView,
    /// The Log tab (not the Monitor one) is showing --- its scroll keys
    /// are inert on Monitor.
    LogTab,
    /// A specific flash-view screen; `None` is the flash view without a
    /// panel (its fallback footer).
    Screen(Option<FlashScreen>),
    /// The device pane is showing its **Actions** tab (the flash
    /// menu's new home) and holds focus.
    ActionsTab,
    /// The device pane is *not* showing the actions tab --- the files
    /// grammar's sites, so they stop claiming keys the tab took over.
    FilesTab,
    /// The device pane has a tab strip at all (a filesystem backend that
    /// can flash or erase). The chord's destination depends on it: with a
    /// strip the chord drives it from panes without one of their own,
    /// without one it falls through to the Log • Monitor strip.
    DeviceStrip(bool),
}

/// Everything a [`Site`] can be tested against: the state the footer is
/// rendered for, built by `App::shortcuts`.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    pub focus: Focus,
    pub caps: Capabilities,
    pub run_active: bool,
    pub run_view: bool,
    pub log_tab: LogTab,
    pub flash_screen: Option<FlashScreen>,
    /// Whether the device pane's Actions tab is showing and
    /// focused (see [`When::ActionsTab`]).
    pub actions_tab: bool,
    /// Whether the device pane has a tab strip at all (see
    /// [`When::DeviceStrip`]).
    pub device_strip: bool,
}

impl Site {
    /// Whether this site is live for `ctx`.
    fn matches(&self, ctx: &Context) -> bool {
        (self.foci.is_empty() || self.foci.contains(&ctx.focus))
            && (self.caps.is_empty() || self.caps.iter().any(|cap| ctx.caps.contains(*cap)))
            && self.when.matches(ctx)
    }
}

impl When {
    fn matches(self, ctx: &Context) -> bool {
        match self {
            When::Always => true,
            When::RunActive => ctx.run_active,
            When::RunView => ctx.run_view,
            When::LogTab => ctx.log_tab == LogTab::Log,
            When::Screen(screen) => ctx.flash_screen == screen,
            When::ActionsTab => ctx.actions_tab,
            When::FilesTab => !ctx.actions_tab,
            When::DeviceStrip(want) => ctx.device_strip == want,
        }
    }
}

const fn binding(key: &'static str, description: &'static str) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: None,
        sites: &[],
    }
}

/// A row with footer sites but no replay event (the help toggle: replaying
/// `?` would just reopen the window).
const fn sited(
    key: &'static str,
    description: &'static str,
    sites: &'static [Site],
) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: None,
        sites,
    }
}

const fn action(
    key: &'static str,
    description: &'static str,
    code: KeyCode,
    sites: &'static [Site],
) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: Some((code, KeyModifiers::NONE)),
        sites,
    }
}

const fn shifted(
    key: &'static str,
    description: &'static str,
    code: KeyCode,
    sites: &'static [Site],
) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: Some((code, KeyModifiers::SHIFT)),
        sites,
    }
}

/// A Ctrl-chord row: the replay event the help select sends on `Enter`.
const fn ctrl(
    key: &'static str,
    description: &'static str,
    code: KeyCode,
    sites: &'static [Site],
) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: Some((code, KeyModifiers::CONTROL)),
        sites,
    }
}

const fn site(
    label: &'static str,
    short: &'static str,
    rank: u8,
    foci: &'static [Focus],
    caps: &'static [Capability],
    when: When,
) -> Site {
    Site {
        label,
        short,
        rank,
        foci,
        caps,
        when,
    }
}

const ANY_FOCUS: &[Focus] = &[];
const FILES: &[Focus] = &[Focus::FilesLocal, Focus::FilesDevice];

/// Footer ranks: 10..=49 the focused pane's own rows, 50..=59 the
/// dashboard-wide commands, 60..=69 the Logs extras, 70..=71 the tail
/// every context keeps. Flash screens reuse the same bands per screen
/// (their sites never co-match).
///
/// The footer shows only what a user cannot guess: navigation rows (and
/// rarely used cosmetic ones) carry no sites at all, staying documented
/// here in the help window instead.
const DASHBOARD_NAVIGATION: [HelpBinding; 10] = [
    binding("tab / shift+tab", "move focus between panes"),
    // Help-only: the footer's width budget is already tight at the
    // minimum terminal size, and this binding needs no footer chip to be
    // discoverable --- pressing Ctrl (or `ctrl+k`) reveals its own letters
    // directly on the panes it jumps to.
    binding("ctrl+k", "reveal pane letters; press one to jump"),
    binding("↑ ↓ / k j", "navigate inside the focused pane"),
    binding("page up/down", "scroll the log by one screen"),
    binding("home / end", "jump to start / end"),
    binding("→", "descend into the selected directory"),
    binding("backspace / ←", "go to the parent directory"),
    sited(
        "ctrl+← / ctrl+→",
        "switch the device pane's tabs from any pane",
        &[
            site(
                "ctrl+←/→",
                "actions",
                13,
                &[Focus::FilesDevice],
                &[Capability::Flash, Capability::EraseFlash],
                When::FilesTab,
            ),
            // The actions side keeps the plain arrows: its stacked buttons
            // take ↑/↓ alone, so ←/→ are free to switch.
            site(
                "←/→",
                "files",
                13,
                &[Focus::FilesDevice],
                &[Capability::Flash, Capability::EraseFlash],
                When::ActionsTab,
            ),
            // The chord is dashboard-wide: panes without a strip of their
            // own drive the device pane's strip without giving up the
            // cursor. Gated on `DeviceStrip`, not the caps --- Zephyr
            // declares `Flash` too, but its build row has no device pane
            // to strip. Its rank sits in the dashboard-commands band, not
            // beside the files keys: it reaches *another* pane, and the
            // footer's middle-dropping may sacrifice it before a key the
            // focused pane itself uses.
            site(
                "ctrl+←/→",
                "actions",
                59,
                &[Focus::FilesLocal, Focus::Project],
                &[],
                When::DeviceStrip(true),
            ),
        ],
    ),
    sited(
        "← / →",
        "switch the Log, Monitor and Terminal tabs",
        &[
            site("←/→", "tabs", 60, &[Focus::Logs], &[], When::Always),
            // Panes with no strip of their own and no device pane beside
            // them (the Zephyr row): the chord lands on row 3's strip.
            site(
                "ctrl+←/→",
                "tabs",
                61,
                &[Focus::Workspace, Focus::Build, Focus::Project],
                &[],
                When::DeviceStrip(false),
            ),
        ],
    ),
    sited(
        "shift+p",
        "back to the project list",
        &[site(
            "shift+p",
            "projects",
            70,
            ANY_FOCUS,
            &[],
            When::Always,
        )],
    ),
];

const DASHBOARD_COMMANDS: [HelpBinding; 25] = [
    action(
        "r",
        "re-detect, reload, or rename (file list)",
        KeyCode::Char('r'),
        &[
            site("r", "reload", 13, FILES, &[], When::FilesTab),
            site("r", "rename", 15, &[Focus::Workspace], &[], When::Always),
            site("r", "re-detect", 10, &[Focus::Logs], &[], When::Always),
        ],
    ),
    // Help-only: a theme is picked once and remembered, so the chip had
    // no context left to earn (same call as `ctrl+i` below).
    action("t", "pick a color theme", KeyCode::Char('t'), &[]),
    // The chord only exists where the Kitty keyboard protocol answered:
    // a legacy terminal sends Ctrl+I as plain Tab (byte 0x09), which keeps
    // its focus-tour meaning there --- a caveat the one-line budget cannot
    // carry, so it lives here and in `App::cycle_icon_set`'s doc. Help-only
    // beside `t`: icons are configured once.
    ctrl(
        "ctrl+i",
        "cycle icons (unicode/nerd/none)",
        KeyCode::Char('i'),
        &[],
    ),
    action(
        "x",
        "open the device pane's Actions tab",
        KeyCode::Char('x'),
        &[site(
            "x",
            "flash",
            52,
            ANY_FOCUS,
            &[Capability::Flash, Capability::EraseFlash],
            When::Always,
        )],
    ),
    // Help-only: `Enter` activating the highlighted row is universal.
    action(
        "enter (files)",
        "browser: entry menu; workspace: open or answer",
        KeyCode::Enter,
        &[],
    ),
    action(
        "v (workspace files)",
        "view a text file in the viewer",
        KeyCode::Char('v'),
        &[site(
            "v",
            "view",
            12,
            &[Focus::Workspace],
            &[],
            When::Always,
        )],
    ),
    action(
        "del (workspace files)",
        "delete the selected entry (asks first)",
        KeyCode::Delete,
        &[site(
            "del",
            "delete",
            13,
            &[Focus::Workspace],
            &[],
            When::Always,
        )],
    ),
    action(
        "a",
        "create a file, or a dir if the name ends with /",
        KeyCode::Char('a'),
        &[
            site("a", "new", 14, FILES, &[], When::FilesTab),
            site("a", "new", 14, &[Focus::Workspace], &[], When::Always),
        ],
    ),
    action(
        "c",
        "compare the selected file by sha256",
        KeyCode::Char('c'),
        &[site(
            "c",
            "compare",
            15,
            FILES,
            &[Capability::Filesystem],
            When::FilesTab,
        )],
    ),
    shifted(
        "shift+s",
        "sync local files to the device",
        KeyCode::Char('s'),
        &[site(
            "shift+s",
            "sync",
            16,
            FILES,
            &[Capability::Filesystem],
            When::FilesTab,
        )],
    ),
    action(
        "h",
        "show or hide dot-files",
        KeyCode::Char('h'),
        &[site("h", "hidden", 17, FILES, &[], When::FilesTab)],
    ),
    // Help-only, like the other `Enter` rows.
    action(
        "enter (build pane)",
        "run the selected action (build or flash pane)",
        KeyCode::Enter,
        &[],
    ),
    // Help-only: the hotplug poll rescans on its own every second, so
    // the manual scan is a recovery path, not something to advertise.
    action(
        "d",
        "scan for devices (mpremote or USB serial)",
        KeyCode::Char('d'),
        &[],
    ),
    action(
        "i",
        "open the package manager",
        KeyCode::Char('i'),
        &[site(
            "i",
            "packages",
            18,
            &[Focus::FilesDevice],
            &[Capability::PackageInstall],
            When::FilesTab,
        )],
    ),
    action(
        "v (actions tab)",
        "verify the flash against a firmware file",
        KeyCode::Char('v'),
        &[site(
            "v",
            "verify",
            19,
            &[Focus::FilesDevice],
            &[Capability::EraseFlash],
            When::ActionsTab,
        )],
    ),
    // Help-only: inside the manager the footer is `App::shortcuts`, and
    // no action can live on a plain letter there (the filter line takes
    // every printable character), so these two are worth spelling out.
    action(
        "del (packages)",
        "remove a package from requirements.txt and the board",
        KeyCode::Delete,
        &[],
    ),
    action(
        "s (dependencies row)",
        "open the package manager",
        KeyCode::Char('s'),
        &[site(
            "s",
            "packages",
            20,
            &[Focus::Project],
            &[Capability::PackageInstall],
            When::Always,
        )],
    ),
    action(
        "m",
        "open the device monitor/REPL; ctrl+] exits",
        KeyCode::Char('m'),
        &[site(
            "m",
            "monitor/REPL",
            53,
            ANY_FOCUS,
            &[Capability::Monitor],
            When::Always,
        )],
    ),
    binding(
        "terminal (tab)",
        "the Terminal tab runs your shell (ctrl+] detaches)",
    ),
    action(
        "s",
        "add or update the SDK's toolchains",
        KeyCode::Char('s'),
        &[site(
            "s",
            "toolchains",
            54,
            ANY_FOCUS,
            &[Capability::WorkspaceSync],
            When::Always,
        )],
    ),
    shifted(
        "shift+r",
        "restart the device (soft-reset)",
        KeyCode::Char('r'),
        &[site(
            "shift+r",
            "restart device",
            54,
            ANY_FOCUS,
            &[Capability::Reset],
            When::Always,
        )],
    ),
    action(
        "e",
        "edit the viewed file with $EDITOR",
        KeyCode::Char('e'),
        &[],
    ),
    action(
        "s",
        "save the run output to a file",
        KeyCode::Char('s'),
        // Declares the capability it actually depends on: a *captured run*
        // is `Capability::Run`'s output. Without it the site claimed `s`
        // for every backend, which collided with the SDK-toolchain shortcut
        // that a workspace backend puts on the same key.
        &[site(
            "s",
            "save output",
            62,
            &[Focus::Logs],
            &[Capability::Run],
            When::RunView,
        )],
    ),
    sited(
        "?",
        "toggle this help",
        &[site("?", "help", 71, ANY_FOCUS, &[], When::Always)],
    ),
    action(
        "q / ctrl+c",
        "quit; interrupts a running script",
        KeyCode::Char('q'),
        // `q` itself is help-only (universal way out); the interrupt is
        // the non-obvious half: it depends on a run being active, and it
        // replaces quitting rather than adding to it.
        &[site(
            "ctrl+c",
            "interrupt",
            61,
            &[Focus::Logs],
            &[],
            When::RunActive,
        )],
    ),
];

const FLASH_NAVIGATION: [HelpBinding; 3] = [
    // Help-only: the menu cursor follows the arrows everywhere.
    binding("↑ ↓ / k j", "move the menu cursor"),
    // Help-only: Tab between fields is a form convention.
    binding("tab", "move between option fields"),
    // Help-only: `q`/`Esc` leaving a screen is universal.
    binding("q / esc", "back one screen, then the dashboard"),
];

const FLASH_COMMANDS: [HelpBinding; 7] = [
    // Help-only: activating the highlighted row is universal.
    action("enter", "run the selected action", KeyCode::Enter, &[]),
    // Help-only: cycling a field's value is what arrows do in a form.
    action("← →", "cycle an option's value", KeyCode::Right, &[]),
    // Help-only: a field with a cursor in it invites typing.
    binding("type / backspace", "edit offset, flags, or a URL"),
    action("ctrl+c", "quit", KeyCode::Char('q'), &[]),
    // The tail every flash screen keeps: with the navigation rows gone,
    // this is the one pointer to the rest of the keys.
    sited(
        "?",
        "toggle this help",
        &[site("?", "help", 71, ANY_FOCUS, &[], When::Always)],
    ),
    action(
        "s",
        "search boards and firmware online",
        KeyCode::Char('s'),
        &[site(
            "s",
            "search online",
            12,
            ANY_FOCUS,
            &[],
            When::Screen(Some(FlashScreen::Menu)),
        )],
    ),
    action(
        "u",
        "flash firmware from a URL",
        KeyCode::Char('u'),
        &[
            site(
                "u",
                "paste URL",
                13,
                ANY_FOCUS,
                &[],
                When::Screen(Some(FlashScreen::Menu)),
            ),
            site(
                "u",
                "paste URL",
                13,
                ANY_FOCUS,
                &[],
                When::Screen(Some(FlashScreen::OnlineBoards)),
            ),
            site(
                "u",
                "paste URL",
                13,
                ANY_FOCUS,
                &[],
                When::Screen(Some(FlashScreen::OnlineFirmware)),
            ),
        ],
    ),
];

/// The bindings `view` shows under `section`.
pub fn bindings(view: View, section: HelpSection) -> &'static [HelpBinding] {
    match (view, section) {
        (View::Dashboard, HelpSection::Navigation) => &DASHBOARD_NAVIGATION,
        (View::Dashboard, HelpSection::Commands) => &DASHBOARD_COMMANDS,
        (View::Flash, HelpSection::Navigation) => &FLASH_NAVIGATION,
        (View::Flash, HelpSection::Commands) => &FLASH_COMMANDS,
    }
}

/// The rows of `table` whose key or description mention `filter`
/// (case-insensitive), or all of them when `filter` is empty --- the same
/// grammar the board picker's filter uses.
pub fn visible<'a>(table: &'a [HelpBinding], filter: &str) -> Vec<&'a HelpBinding> {
    let filter = filter.to_lowercase();
    table
        .iter()
        .filter(|row| {
            filter.is_empty()
                || row.key.to_lowercase().contains(&filter)
                || row.description.to_lowercase().contains(&filter)
        })
        .collect()
}

/// The footer for `view` under `ctx`: every live site's label and
/// one-liner, in `rank` order.
pub fn footer(view: View, ctx: &Context) -> Vec<(&'static str, &'static str)> {
    let mut hits: Vec<(u8, &'static str, &'static str)> = HelpSection::ALL
        .iter()
        .flat_map(|&section| bindings(view, section))
        .flat_map(|binding| binding.sites.iter())
        .filter(|site| site.matches(ctx))
        .map(|site| (site.rank, site.label, site.short))
        .collect();
    hits.sort_by_key(|&(rank, _, _)| rank);
    hits.into_iter()
        .map(|(_, label, short)| (label, short))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_and_section_has_bindings() {
        for view in [View::Dashboard, View::Flash] {
            for section in HelpSection::ALL {
                let rows = bindings(view, section);
                assert!(!rows.is_empty(), "{view:?}/{section:?} is empty");
                for row in rows {
                    assert!(!row.key.is_empty());
                    assert!(!row.description.is_empty());
                }
            }
        }
    }

    #[test]
    fn only_command_rows_can_be_activated() {
        // Navigation is never part of the select, so it carries no replay
        // event; commands do, except the rows with no sensible replay.
        for view in [View::Dashboard, View::Flash] {
            for row in bindings(view, HelpSection::Navigation) {
                assert!(row.event.is_none(), "{}: {}", row.key, row.description);
            }
            let commands = bindings(view, HelpSection::Commands);
            assert!(
                commands.iter().any(|row| row.event.is_some()),
                "{view:?}: the select has nothing to activate"
            );
        }
    }

    #[test]
    fn descriptions_fit_the_single_line_budget() {
        // One command per line, no wrapping: the table's own width (widest
        // key + widest description + padding + borders) must stay within a
        // stock 80-column terminal.
        for view in [View::Dashboard, View::Flash] {
            let key_col = HelpSection::ALL
                .iter()
                .flat_map(|&section| bindings(view, section))
                .map(|row| row.key.chars().count())
                .max()
                .unwrap_or(0);
            for section in HelpSection::ALL {
                for row in bindings(view, section) {
                    let width = 2 + key_col + 2 + row.description.chars().count() + 2;
                    assert!(
                        width <= 80,
                        "{view:?} '{}/{}' needs {width} columns",
                        row.key,
                        row.description
                    );
                }
            }
        }
    }

    #[test]
    fn the_widest_dashboard_row_fits_the_reference_width() {
        let key_col = HelpSection::ALL
            .iter()
            .flat_map(|&section| bindings(View::Dashboard, section))
            .map(|row| row.key.chars().count())
            .max()
            .unwrap_or(0);
        let widest = HelpSection::ALL
            .iter()
            .flat_map(|&section| bindings(View::Dashboard, section))
            .map(|row| row.description.chars().count())
            .max()
            .unwrap_or(0);
        assert!(2 + key_col + 2 + widest + 2 <= 80);
    }

    /// A MicroPython-shaped capability set (files + device session).
    fn micropython() -> Capabilities {
        Capabilities::from_slice(&[
            Capability::Filesystem,
            Capability::Repl,
            Capability::Monitor,
            Capability::Run,
            Capability::Reset,
            Capability::Upload,
            Capability::Download,
            Capability::EraseFlash,
            Capability::DeviceInfo,
            Capability::PackageInstall,
            Capability::ProjectSelect,
        ])
    }

    /// A Zephyr-shaped capability set (build, no device filesystem).
    fn zephyr() -> Capabilities {
        Capabilities::from_slice(&[
            Capability::Build,
            Capability::Clean,
            Capability::Flash,
            Capability::Monitor,
            Capability::BoardSelect,
            Capability::ShieldSelect,
            Capability::ProjectSelect,
            Capability::WorkspaceSync,
            Capability::DeviceInfo,
        ])
    }

    fn ctx(caps: Capabilities, focus: Focus) -> Context {
        Context {
            focus,
            caps,
            run_active: false,
            run_view: false,
            log_tab: LogTab::Log,
            flash_screen: None,
            actions_tab: false,
            device_strip: false,
        }
    }

    fn footer_keys(view: View, ctx: &Context) -> Vec<&'static str> {
        footer(view, ctx).into_iter().map(|(key, _)| key).collect()
    }

    #[test]
    fn the_footer_lists_every_live_site_in_rank_order() {
        // The files columns: only the keys a user cannot guess --- the
        // pane's own grammar plus the dashboard-wide commands, then the
        // tail. Navigation (tab, arrows, enter) stays in the help window.
        // MicroPython's device pane carries a strip, so the chord is
        // advertised from the local pane too (it drives that strip from
        // wherever the cursor sits).
        let mut files_ctx = ctx(micropython(), Focus::FilesLocal);
        files_ctx.device_strip = true;
        let files = footer(View::Dashboard, &files_ctx);
        assert_eq!(
            files,
            vec![
                ("r", "reload"),
                ("a", "new"),
                ("c", "compare"),
                ("shift+s", "sync"),
                ("h", "hidden"),
                ("x", "flash"),
                ("m", "monitor/REPL"),
                ("shift+r", "restart device"),
                ("ctrl+←/→", "actions"),
                ("shift+p", "projects"),
                ("?", "help"),
            ]
        );

        // The device column adds the package install.
        let device_keys = footer_keys(View::Dashboard, &ctx(micropython(), Focus::FilesDevice));
        assert!(device_keys.contains(&"i"), "{device_keys:?}");

        // Logs while a run is active: the run's own keys ride between the
        // dashboard commands and the tail.
        let mut logs = ctx(micropython(), Focus::Logs);
        logs.run_active = true;
        logs.run_view = true;
        assert_eq!(
            footer_keys(View::Dashboard, &logs),
            vec![
                "r", "x", "m", "shift+r", "←/→", "ctrl+c", "s", "shift+p", "?",
            ]
        );

        // The tab strip's arrows survive on the Monitor tab too.
        let mut monitor = logs;
        monitor.log_tab = LogTab::Monitor;
        let keys = footer_keys(View::Dashboard, &monitor);
        assert!(keys.contains(&"←/→"), "{keys:?}");

        // Zephyr's build pane: no device strip, so the chord falls to
        // row 3's strip and is advertised as such.
        assert_eq!(
            footer_keys(View::Dashboard, &ctx(zephyr(), Focus::Build)),
            vec!["x", "m", "s", "ctrl+←/→", "shift+p", "?"]
        );

        // The project-files pane (all of it, now that the checklist moved
        // to the Project pane).
        assert_eq!(
            footer_keys(View::Dashboard, &ctx(zephyr(), Focus::Workspace)),
            vec![
                "v",
                "del",
                "a",
                "r",
                "x",
                "m",
                "s",
                "ctrl+←/→",
                "shift+p",
                "?"
            ]
        );

        // The Project pane: the questions' own grammar.
        assert_eq!(
            footer_keys(View::Dashboard, &ctx(zephyr(), Focus::Project)),
            vec!["x", "m", "s", "ctrl+←/→", "shift+p", "?"]
        );
    }

    #[test]
    fn the_flash_footer_follows_the_screen() {
        // Navigation and the way out stay in the help window; what remains
        // is the one action each screen offers that cannot be guessed,
        // plus the help tail that points at the rest.
        for (screen, expected) in [
            (
                Some(FlashScreen::Menu),
                vec![("s", "search online"), ("u", "paste URL"), ("?", "help")],
            ),
            (Some(FlashScreen::Options), vec![("?", "help")]),
            (
                Some(FlashScreen::OnlineBoards),
                vec![("u", "paste URL"), ("?", "help")],
            ),
            (
                Some(FlashScreen::OnlineFirmware),
                vec![("u", "paste URL"), ("?", "help")],
            ),
            (Some(FlashScreen::CustomUrl), vec![("?", "help")]),
            (None, vec![("?", "help")]),
        ] {
            let mut context = ctx(zephyr(), Focus::Logs);
            context.flash_screen = screen;
            assert_eq!(footer(View::Flash, &context), expected, "screen {screen:?}");
        }
    }

    #[test]
    fn no_context_repeats_a_footer_label() {
        // Sites are the single source, so two of them firing at once would
        // render the same key twice. Walk every context shape.
        for caps in [micropython(), zephyr(), Capabilities::empty()] {
            for focus in [
                Focus::Project,
                Focus::FilesLocal,
                Focus::FilesDevice,
                Focus::Workspace,
                Focus::Build,
                Focus::Logs,
            ] {
                for actions_tab in [false, true] {
                    for device_strip in [false, true] {
                        for run_active in [false, true] {
                            for run_view in [false, true] {
                                for log_tab in [LogTab::Log, LogTab::Monitor, LogTab::Terminal] {
                                    let mut context = ctx(caps, focus);
                                    context.actions_tab = actions_tab;
                                    context.device_strip = device_strip;
                                    context.run_active = run_active;
                                    context.run_view = run_view;
                                    context.log_tab = log_tab;
                                    let keys = footer_keys(View::Dashboard, &context);
                                    let mut sorted = keys.clone();
                                    sorted.sort_unstable();
                                    sorted.dedup();
                                    assert_eq!(
                                        keys.len(),
                                        sorted.len(),
                                        "duplicate label: {keys:?} (focus {focus:?})"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_footer_entry_is_a_described_binding() {
        // The parity guarantee: anything the footer can show belongs to a
        // row the help window lists, so there is always a place to read
        // what a footer chip does.
        for view in [View::Dashboard, View::Flash] {
            let described: Vec<&str> = HelpSection::ALL
                .iter()
                .flat_map(|&section| bindings(view, section))
                .map(|row| row.key)
                .collect();
            for binding in HelpSection::ALL
                .iter()
                .flat_map(|&section| bindings(view, section))
            {
                for site in binding.sites {
                    assert!(!site.label.is_empty());
                    assert!(!site.short.is_empty());
                    assert!(described.contains(&binding.key));
                }
            }
        }
    }

    #[test]
    fn the_filter_narrows_keys_and_descriptions() {
        let rows = bindings(View::Dashboard, HelpSection::Commands);
        assert_eq!(visible(rows, "").len(), rows.len());

        let sync = visible(rows, "sync");
        assert!(
            sync.iter().any(|row| row.key == "shift+s"),
            "matches the description"
        );

        let theme = visible(rows, "THEME");
        assert_eq!(theme.len(), 1, "case-insensitive, key or description");
        assert_eq!(theme[0].key, "t");

        assert!(visible(rows, "no such binding").is_empty());
    }
}
