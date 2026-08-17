//! The help overlay's content (`?`): one window, two titled divisions.
//! [`HelpSection::Navigation`] holds the movement keys as plain rows;
//! [`HelpSection::Commands`] is the select --- arrows move, `Enter`
//! activates the row by replaying its key (`HelpBinding::event`) once the
//! help has closed, so the list doubles as a launcher. Help follows the
//! screen (see [`View`]): listing dashboard keys while browsing files would
//! describe bindings that do nothing.
//!
//! The descriptions are part of the data, not the rendering: each is
//! summarized to fit its row on one line at the width the table needs (the
//! widest is 49 columns against a 21-column key column, so the popup fits a
//! stock 80-column terminal). The renderer truncates rather than wraps only
//! as a fallback for terminals narrower than the table.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use super::View;

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
#[derive(Debug, Clone, Copy)]
pub struct HelpBinding {
    pub key: &'static str,
    pub description: &'static str,
    pub event: Option<(KeyCode, KeyModifiers)>,
}

const fn binding(key: &'static str, description: &'static str) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: None,
    }
}

const fn action(key: &'static str, description: &'static str, code: KeyCode) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: Some((code, KeyModifiers::NONE)),
    }
}

const fn shifted(key: &'static str, description: &'static str, code: KeyCode) -> HelpBinding {
    HelpBinding {
        key,
        description,
        event: Some((code, KeyModifiers::SHIFT)),
    }
}

const DASHBOARD_NAVIGATION: [HelpBinding; 7] = [
    binding("tab / shift+tab", "move focus between panes"),
    binding("↑ ↓ / k j", "navigate inside the focused pane"),
    binding("page up/down", "scroll the log by one screen"),
    binding("home / end", "jump to start / end"),
    binding("→", "descend into the selected directory"),
    binding("backspace / ←", "go to the parent directory"),
    binding("shift+p", "back to the project list"),
];

const DASHBOARD_COMMANDS: [HelpBinding; 18] = [
    action(
        "r",
        "re-run detection, or reload the file pane",
        KeyCode::Char('r'),
    ),
    action("o", "override the detected backend", KeyCode::Char('o')),
    action("t", "pick a color theme", KeyCode::Char('t')),
    action(
        "enter (files)",
        "browser: entry menu; workspace: open/edit",
        KeyCode::Enter,
    ),
    action(
        "v (workspace files)",
        "view a text file in the viewer",
        KeyCode::Char('v'),
    ),
    action(
        "del (workspace files)",
        "delete the selected entry (asks first)",
        KeyCode::Delete,
    ),
    action(
        "a",
        "create a file, or a dir if the name ends with /",
        KeyCode::Char('a'),
    ),
    action(
        "c",
        "compare the selected file by sha256",
        KeyCode::Char('c'),
    ),
    shifted(
        "shift+s",
        "sync local files to the device",
        KeyCode::Char('s'),
    ),
    action("h", "show or hide dot-files", KeyCode::Char('h')),
    action(
        "enter (build pane)",
        "run the selected build action",
        KeyCode::Enter,
    ),
    action(
        "d",
        "scan for devices (mpremote or USB serial)",
        KeyCode::Char('d'),
    ),
    action("i", "install a package via mip", KeyCode::Char('i')),
    action(
        "m",
        "open the device monitor/REPL; ctrl+] exits",
        KeyCode::Char('m'),
    ),
    shifted(
        "shift+r",
        "restart the device (soft-reset)",
        KeyCode::Char('r'),
    ),
    action("e", "edit the viewed file with $EDITOR", KeyCode::Char('e')),
    binding("?", "toggle this help"),
    action("q / esc / ctrl+c", "quit", KeyCode::Char('q')),
];

const FLASH_NAVIGATION: [HelpBinding; 3] = [
    binding("↑ ↓ / k j", "move the menu cursor"),
    binding("tab", "move between option fields"),
    binding("q / esc", "back one screen, then the dashboard"),
];

const FLASH_COMMANDS: [HelpBinding; 4] = [
    action("enter", "run the selected action", KeyCode::Enter),
    action("← →", "cycle an option's value", KeyCode::Right),
    binding("type / backspace", "edit offset or extra flags"),
    action("ctrl+c", "quit", KeyCode::Char('q')),
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
}
