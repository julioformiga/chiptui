//! The app-wide colour theme and the icon set, and the two pickers that
//! answer them.
//!
//! Both are user config (`[ui] theme`, `[ui] icons`) with a live preview:
//! the theme picker repaints the dashboard behind itself as the cursor
//! moves, and `ctrl+i` cycles the icon sets in place. Split out of `app.rs`
//! because nothing else reads these --- every other subsystem takes the
//! resolved `Palette`/`IconSet` as an argument.

use crate::backend::BackendKind;

use super::{App, Overlay};

/// One row of the theme picker. Every `Named` row is a fixed theme that
/// applies to all projects alike; `Auto` is the one answer that depends on
/// the session --- it follows the active backend, so a Zephyr project
/// renders in Catppuccin Mocha and a MicroPython one in Everforest, with
/// Tokyo Night standing in wherever no backend is active yet (the home
/// screen, an unresolved project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    Auto,
    Named(ratatui_themes::ThemeName),
}

impl ThemeChoice {
    /// The picker's rows: `Auto` first, then every fixed theme --- the same
    /// "the meta answer leads" order the backend picker follows.
    pub fn all() -> Vec<Self> {
        std::iter::once(Self::Auto)
            .chain(
                ratatui_themes::ThemeName::all()
                    .iter()
                    .copied()
                    .map(Self::Named),
            )
            .collect()
    }

    /// Parses a stored `[ui] theme` slug: `auto` is ours, every other slug
    /// belongs to `ratatui_themes`.
    pub fn from_slug(slug: &str) -> Option<Self> {
        if slug.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else {
            slug.parse().ok().map(Self::Named)
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Named(theme) => theme.slug(),
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Named(theme) => theme.display_name(),
        }
    }

    /// The concrete theme this choice renders as for the active backend.
    pub fn resolve(self, backend: Option<BackendKind>) -> ratatui_themes::ThemeName {
        match (self, backend) {
            (Self::Auto, Some(BackendKind::Zephyr)) => ratatui_themes::ThemeName::CatppuccinMocha,
            (Self::Auto, Some(BackendKind::MicroPython)) => ratatui_themes::ThemeName::Everforest,
            (Self::Auto, None) => ratatui_themes::ThemeName::TokyoNight,
            (Self::Named(theme), _) => theme,
        }
    }
}

impl App {
    /// The palette actually rendered this frame --- deliberately named apart
    /// from [`crate::backend::BackendKind::palette`], which answers a
    /// different question ("which backend is this row?") and coexists with
    /// this one rather than being replaced by it. While [`Overlay::ThemePicker`]
    /// is open this previews the hovered row live (the whole UI behind the
    /// popup, popup included, so a pick can be judged before it commits);
    /// [`Self::theme`] itself is untouched until `Enter`, so `Esc` reverts
    /// for free --- nothing was ever committed to preview.
    pub fn theme_palette(&self) -> ratatui_themes::ThemePalette {
        self.previewed_theme().palette()
    }

    /// The theme this session renders in right now: the stored choice
    /// resolved against the active backend, so an `Auto` choice follows a
    /// backend override or re-detection live. Deliberately named apart
    /// from [`crate::backend::BackendKind::palette`], which answers a
    /// different question ("which backend is this row?") and coexists with
    /// this one rather than being replaced by it. While [`Overlay::ThemePicker`]
    /// is open the palette previews the hovered row live (the whole UI behind the
    /// popup, popup included, so a pick can be judged before it commits);
    /// this value itself is untouched until `Enter`, so `Esc` reverts for
    /// free --- nothing was ever committed to preview.
    pub fn theme(&self) -> ratatui_themes::ThemeName {
        self.theme.resolve(self.manager.selected_kind())
    }

    /// The stored choice behind [`Self::theme`] --- what the picker's
    /// "(active)" marker sits on and what a restart would reload: `Auto`
    /// when the session follows the backend, a fixed theme otherwise.
    pub fn theme_choice(&self) -> ThemeChoice {
        self.theme
    }

    /// `ctrl+i`: steps the icon rendering through its three values in
    /// declaration order (`unicode` → `nerd` → `none` → `unicode`), applies
    /// it immediately (the next frame's button stacks read a different
    /// [`Self::icon_set`]) and persists it the same way the theme picker
    /// does --- a failed write still applies the set for this session, so
    /// it is logged as a warning rather than lost silently. The chord only
    /// ever arrives as `Char('i')` + `CONTROL` on a terminal that answered
    /// the Kitty keyboard protocol probe; a legacy terminal sends Ctrl+I as
    /// plain Tab (byte `0x09`), which keeps its focus-tour meaning there,
    /// and nothing about this arm can fire.
    pub(super) fn cycle_icon_set(&mut self) {
        let next = match self.icons {
            crate::icons::IconSet::Unicode => crate::icons::IconSet::Nerd,
            crate::icons::IconSet::Nerd => crate::icons::IconSet::None,
            crate::icons::IconSet::None => crate::icons::IconSet::Unicode,
        };
        self.icons = next;
        let name = match next {
            crate::icons::IconSet::Unicode => "unicode",
            crate::icons::IconSet::Nerd => "nerd",
            crate::icons::IconSet::None => "none",
        };
        let config = self.user_config_path();
        match crate::settings::save_icons(&config, name) {
            Ok(()) => self
                .logs
                .info(format!("icon set cycled to {name} ({})", config.display())),
            Err(err) => self.logs.warn(format!(
                "icon set cycled to {name} for this session, but could not save it to {}: {err}",
                config.display()
            )),
        }
    }

    /// The button glyphs' rendering for this session ([`resolve_icons`]).
    /// [`Self::set_icon_set`] is the test seam; `ctrl+i`
    /// ([`Self::cycle_icon_set`]) is the runtime switch.
    pub fn icon_set(&self) -> crate::icons::IconSet {
        self.icons
    }

    /// Points the session at another icon rendering --- the test seam the
    /// render tests use to draw the panes with the Nerd set, the same role
    /// `set_terminal_tool`/`set_keyboard_enhanced` play for theirs. Real
    /// sessions get theirs from `[ui] icons` at startup and switch it with
    /// `ctrl+i` ([`Self::cycle_icon_set`]), which also persists the answer.
    pub fn set_icon_set(&mut self, icons: crate::icons::IconSet) {
        self.icons = icons;
    }

    pub(super) fn previewed_theme(&self) -> ratatui_themes::ThemeName {
        match &self.overlay {
            Some(Overlay::ThemePicker { selected }) => ThemeChoice::all()
                .get(*selected)
                .copied()
                .map(|choice| choice.resolve(self.manager.selected_kind()))
                .unwrap_or_else(|| self.theme()),
            _ => self.theme(),
        }
    }

    /// Opens the theme picker (`t`) with the cursor on the currently active
    /// choice, the same "start where the current answer is" convention the
    /// backend override picker follows.
    pub(super) fn open_theme_picker(&mut self) {
        let selected = ThemeChoice::all()
            .iter()
            .position(|&candidate| candidate == self.theme)
            .unwrap_or(0);
        self.overlay = Some(Overlay::ThemePicker { selected });
    }

    /// Applies the picked choice immediately (no restart needed --- the next
    /// frame just reads a different [`Self::theme_palette`]) and persists it
    /// to the user config the same way `workspace_view`'s
    /// `accept_workspace_dir` saves the workspace answer: a failed write
    /// still applies the theme for this session, it just cannot survive a
    /// restart, so it is logged as a warning rather than lost silently.
    pub(super) fn apply_theme_picker(&mut self, selected: usize) {
        let Some(choice) = ThemeChoice::all().get(selected).copied() else {
            return;
        };
        self.theme = choice;
        let applied = match choice {
            ThemeChoice::Auto => {
                "Auto --- Zephyr: Catppuccin Mocha, MicroPython: Everforest".to_string()
            }
            ThemeChoice::Named(theme) => theme.display_name().to_string(),
        };
        let config = self.user_config_path();
        match crate::settings::save_theme(&config, choice.slug()) {
            Ok(()) => self
                .logs
                .info(format!("theme set to {applied} ({})", config.display())),
            Err(err) => self.logs.warn(format!(
                "theme set to {applied} for this session, but could not save it to {}: {err}",
                config.display()
            )),
        }
    }
}
