//! The project configuration screen's state.
//!
//! `SPEC.md` §7, §13. Three ideas shape everything below.
//!
//! **The window is the file, rendered.** Whoever uses this commits
//! `chiptui.toml` and reads the diff later, so the rows carry the file's own
//! key names and the details pane quotes the literal line each answer will
//! write. Nothing is paraphrased into prose that the file does not contain.
//!
//! **The whole window is one transaction.** Answers are collected as
//! [`Pending`] edits and go to disk only when the user applies them, behind
//! one review dialog naming every line about to be written. Leaving with
//! edits outstanding asks before dropping them. The two exceptions are
//! deliberate and visible: the theme and the icon set *preview* live, since
//! their value is the appearance itself --- and they preview by being read
//! off this panel rather than by mutating the session, so discarding
//! restores them for free.
//!
//! **The backend is chosen, not typed.** It is a picture of two boards'
//! worth of tooling, not a string, so it is a pair of cards carrying each
//! backend's own mark and colour --- the vocabulary the home screen already
//! uses to tell the two kinds apart. Choosing one reveals the sections that
//! backend owns; it writes nothing until the transaction is applied.

use std::path::{Path, PathBuf};

use crate::app::{DocsFocus, ThemeChoice};
use crate::backend::{BackendKind, Capabilities, Capability};
use crate::icons::IconSet;
use crate::ota::{OtaMethod, Transport};
use crate::project::config;

/// Where a row's answer is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// The project's `chiptui.toml` --- committed, shared with the team.
    Project,
    /// The user config's `[ui]` section --- this machine, every project.
    User,
    /// The user config's `[[project]]` registry entry for this directory.
    Registry,
    /// Nothing: a row that reports rather than asks.
    ReadOnly,
}

impl Destination {
    /// The file named beside a pending change, so a review dialog says
    /// where each line lands.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "chiptui.toml",
            Self::User => "user config",
            Self::Registry => "project registry",
            Self::ReadOnly => "",
        }
    }
}

/// A group of rows. The names are the reader's, not the file's --- the file's
/// own `[section]` spelling appears in the details pane, on the line that
/// will actually be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Answers that hold whatever the backend is: what this project is
    /// called, where it lives, and how ChipTUI itself looks.
    General,
    Zephyr,
    OverTheAir,
    MicroPython,
}

impl Section {
    pub const fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Zephyr => "Zephyr",
            Self::OverTheAir => "Over the air",
            Self::MicroPython => "MicroPython",
        }
    }
}

/// One line of the window's list. The backend choice is not here: it is the
/// card strip above, which answers a question a list row cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectConfigRow {
    Heading(Section),
    /// The project's name in the registry --- what the home screen lists it
    /// under.
    Name,
    /// Where the project is. A report: moving it is not this window's job.
    Root,
    Theme,
    Icons,
    Mouse,
    ZephyrWorkspace,
    ZephyrProjects,
    ZephyrSdk,
    ZephyrWest,
    ZephyrBoard,
    ZephyrShield,
    OtaMethod,
    OtaTransport,
    OtaAddress,
    OtaAutoConfirm,
    MpyProjects,
    /// How many `[[variant]]` blocks the file declares. A report: an array
    /// of tables is a shape the surgical writer cannot express.
    Variants,
}

/// How a row is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Heading,
    /// One of a fixed set of spellings. The empty answer is always the last
    /// stop of the cycle: a key ChipTUI has no answer for must be *absent*
    /// from the file, which is a different statement from an empty string.
    Choice(Vec<String>),
    Text,
    Report,
}

const BOOL_IDS: [&str; 2] = ["true", "false"];

impl ProjectConfigRow {
    /// The `[section] key` this row writes, or `None` for a heading or a
    /// report. An empty section is the file's top level.
    pub const fn slot(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Theme => Some(("ui", "theme")),
            Self::Icons => Some(("ui", "icons")),
            Self::Mouse => Some(("ui", "mouse")),
            Self::ZephyrWorkspace => Some((config::ZEPHYR_SECTION, "workspace")),
            Self::ZephyrProjects => Some((config::ZEPHYR_SECTION, "projects")),
            Self::ZephyrSdk => Some((config::ZEPHYR_SECTION, "sdk")),
            Self::ZephyrWest => Some((config::ZEPHYR_SECTION, "west")),
            Self::ZephyrBoard => Some((config::ZEPHYR_SECTION, "board")),
            Self::ZephyrShield => Some((config::ZEPHYR_SECTION, "shield")),
            Self::OtaMethod => Some((config::OTA_SECTION, "method")),
            Self::OtaTransport => Some((config::OTA_SECTION, "transport")),
            Self::OtaAddress => Some((config::OTA_SECTION, "address")),
            Self::OtaAutoConfirm => Some((config::OTA_SECTION, "auto_confirm")),
            Self::MpyProjects => Some((config::MICROPYTHON_SECTION, "projects")),
            Self::Heading(_) | Self::Name | Self::Root | Self::Variants => None,
        }
    }

    /// The section this row belongs to --- which is also the question "does
    /// the backend choice govern this answer", and the one the tint behind
    /// the list is drawn from.
    pub const fn section(self) -> Section {
        match self {
            Self::Heading(section) => section,
            Self::Name | Self::Root | Self::Theme | Self::Icons | Self::Mouse => Section::General,
            Self::MpyProjects => Section::MicroPython,
            Self::OtaMethod | Self::OtaTransport | Self::OtaAddress | Self::OtaAutoConfirm => {
                Section::OverTheAir
            }
            _ => Section::Zephyr,
        }
    }

    pub const fn destination(self) -> Destination {
        match self {
            Self::Theme | Self::Icons | Self::Mouse => Destination::User,
            Self::Name => Destination::Registry,
            Self::Heading(_) | Self::Root | Self::Variants => Destination::ReadOnly,
            _ => Destination::Project,
        }
    }

    /// The row's label: a phrase saying what the answer *does*, not the
    /// key's own spelling --- `auto_confirm` is a string the file needs,
    /// "Auto-confirm image" is a question a person answers. The key keeps
    /// its place where the file is the subject: the details pane's
    /// will-write line and the apply review both quote it literally.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Heading(section) => section.title(),
            Self::Name => "Project name",
            Self::Root => "Project folder",
            Self::Theme => "Color theme",
            Self::Icons => "Icon set",
            Self::Mouse => "Mouse support",
            Self::ZephyrWorkspace => "Workspace path",
            Self::ZephyrProjects => "Projects folder",
            Self::ZephyrSdk => "SDK path",
            Self::ZephyrWest => "west program",
            Self::ZephyrBoard => "Target board",
            Self::ZephyrShield => "Shield",
            Self::OtaMethod => "Mechanism",
            Self::OtaTransport => "Transport",
            Self::OtaAddress => "Board address",
            Self::OtaAutoConfirm => "Auto-confirm image",
            Self::MpyProjects => "Projects folder",
            Self::Variants => "Build variants",
        }
    }

    pub fn kind(self) -> RowKind {
        let ids = |values: &[&str]| {
            RowKind::Choice(values.iter().map(|value| (*value).to_string()).collect())
        };
        match self {
            Self::Heading(_) => RowKind::Heading,
            Self::Root | Self::Variants => RowKind::Report,
            Self::Theme => RowKind::Choice(
                ThemeChoice::all()
                    .iter()
                    .map(|choice| choice.slug().to_string())
                    .collect(),
            ),
            Self::Icons => ids(&["unicode", "nerd", "none"]),
            Self::Mouse | Self::OtaAutoConfirm => ids(&BOOL_IDS),
            Self::OtaMethod => RowKind::Choice(
                OtaMethod::ALL
                    .iter()
                    .map(|method| method.id().to_string())
                    .collect(),
            ),
            Self::OtaTransport => RowKind::Choice(
                Transport::ALL
                    .iter()
                    .map(|transport| transport.id().to_string())
                    .collect(),
            ),
            _ => RowKind::Text,
        }
    }

    /// What the key is for, in the reader's words, under the details pane's
    /// heading.
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Heading(Section::General) => {
                "What this project is called, and how ChipTUI looks while you work on it."
            }
            Self::Heading(Section::Zephyr) => "Where the environment this project builds in lives.",
            Self::Heading(Section::OverTheAir) => {
                "How a new image reaches the board without a cable."
            }
            Self::Heading(Section::MicroPython) => "Where this project's sources and boards live.",
            Self::Name => "The name the project list shows. Defaults to the folder's own name.",
            Self::Root => "The folder this configuration belongs to. Switch projects to change it.",
            Self::Theme => "The colours the whole application draws in. Auto follows the backend.",
            Self::Icons => {
                "The glyph set the buttons and pane titles use. Nerd needs a patched font."
            }
            Self::Mouse => "Click and wheel reporting. Takes effect the next time ChipTUI starts.",
            Self::ZephyrWorkspace => "The west workspace root: the folder holding .west/.",
            Self::ZephyrProjects => "Where this machine's Zephyr applications live.",
            Self::ZephyrSdk => "The toolchain folder, exported as ZEPHYR_SDK_INSTALL_DIR.",
            Self::ZephyrWest => "An explicit west program, instead of the workspace venv's.",
            Self::ZephyrBoard => "The board target west build -b is given.",
            Self::ZephyrShield => "The shield on that board, passed as --shield.",
            Self::OtaMethod => "The update mechanism. mcumgr is the only one implemented.",
            Self::OtaTransport => "How smpmgr reaches the board.",
            Self::OtaAddress => "The board's address for that transport.",
            Self::OtaAutoConfirm => {
                "Confirm a verified image automatically, which makes it permanent."
            }
            Self::MpyProjects => {
                "The folder the project picker lists this project's siblings from."
            }
            Self::Variants => "The parallel build configurations the file declares. Read only.",
        }
    }
}

/// One answer waiting to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub row: ProjectConfigRow,
    /// `None` removes the key.
    pub value: Option<String>,
}

impl Pending {
    /// The line this change writes, in the file's own syntax --- what the
    /// review dialog shows and what a later `git diff` will carry.
    pub fn line(&self) -> String {
        match (&self.value, self.row.slot()) {
            (Some(value), Some((_, key))) => format!("{key} = \"{value}\""),
            (None, Some((_, key))) => format!("remove {key}"),
            (Some(value), None) => format!("{} = {value}", self.row.label()),
            (None, None) => format!("clear {}", self.row.label()),
        }
    }

    /// The `[section]` the line belongs to, when the destination has one.
    pub fn section(&self) -> Option<&'static str> {
        self.row
            .slot()
            .map(|(section, _)| section)
            .filter(|section| !section.is_empty())
    }
}

/// A line for the header: what just happened. Two kinds, because the two
/// things the window has to report are opposites --- a transaction that
/// landed, and answers that were dropped to make room for a choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Done(String),
    Lost(String),
}

impl Notice {
    pub fn text(&self) -> &str {
        match self {
            Self::Done(text) | Self::Lost(text) => text,
        }
    }
}

/// Where the cursor is: on the backend cards, or on a row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Cards,
    Row(usize),
}

/// A row being typed into.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    row: ProjectConfigRow,
    input: String,
}

/// The window.
pub struct ProjectConfigPanel {
    root: PathBuf,
    path: PathBuf,
    user_config: PathBuf,
    /// The two files, read once when the window opens. Everything the
    /// window shows about "what is on disk" comes from here, so a row and
    /// its pending change can never disagree about what they replace.
    text: String,
    user_text: String,
    /// The backend the *session* resolved, and the one the cards show. They
    /// differ exactly while a choice waits to be applied.
    backend: Option<BackendKind>,
    chosen: Option<BackendKind>,
    rows: Vec<ProjectConfigRow>,
    cursor: Cursor,
    pending: Vec<Pending>,
    edit: Option<Edit>,
    error: Option<String>,
    notice: Option<Notice>,
    /// Which half of the window the arrows drive (the pickers' `Tab`
    /// grammar): the list walks rows, the details pane scrolls --- a
    /// choice row's full option list lives there now, and a long one (the
    /// themes) is only reachable by scrolling it.
    details_focus: DocsFocus,
    /// The details pane's scroll offset, clamped by the renderer, which
    /// knows the wrapped length (the docs pickers' own contract).
    details_scroll: usize,
    from_startup: bool,
}

impl ProjectConfigPanel {
    pub fn new(
        root: &Path,
        user_config: &Path,
        backend: Option<BackendKind>,
        caps: Capabilities,
        from_startup: bool,
    ) -> Self {
        let path = root.join(config::FILE_NAME);
        let mut panel = Self {
            root: root.to_path_buf(),
            text: std::fs::read_to_string(&path).unwrap_or_default(),
            user_text: std::fs::read_to_string(user_config).unwrap_or_default(),
            path,
            user_config: user_config.to_path_buf(),
            backend,
            chosen: backend,
            rows: Vec::new(),
            cursor: Cursor::Cards,
            pending: Vec::new(),
            edit: None,
            error: None,
            notice: None,
            details_focus: DocsFocus::List,
            details_scroll: 0,
            from_startup,
        };
        panel.rebuild(caps);
        // A window that opened by itself has one question; one the user
        // opened has a file to read, so it starts in the list.
        if !from_startup && panel.chosen.is_some() {
            panel.cursor = Cursor::Row(panel.first_selectable());
        }
        panel
    }

    /// Rebuilds the row list.
    ///
    /// `caps` describes the *chosen* backend, not the session's --- the
    /// sections appear the moment a card is picked, which is what makes the
    /// choice legible before it is applied. Capability-driven and never
    /// backend-kind-driven (`AGENTS.md` §3), with one honest exception: a
    /// backend with no project-level keys at all would show an empty
    /// heading, so the section itself is gated on having rows.
    pub fn rebuild(&mut self, caps: Capabilities) {
        let mut rows = vec![
            ProjectConfigRow::Heading(Section::General),
            ProjectConfigRow::Name,
            ProjectConfigRow::Root,
            ProjectConfigRow::Theme,
            ProjectConfigRow::Icons,
            ProjectConfigRow::Mouse,
        ];
        if caps.contains(Capability::WorkspaceSync) {
            rows.extend([
                ProjectConfigRow::Heading(Section::Zephyr),
                ProjectConfigRow::ZephyrWorkspace,
                ProjectConfigRow::ZephyrProjects,
                ProjectConfigRow::ZephyrSdk,
                ProjectConfigRow::ZephyrWest,
                ProjectConfigRow::ZephyrBoard,
                ProjectConfigRow::ZephyrShield,
            ]);
            if self.variants() > 0 {
                rows.push(ProjectConfigRow::Variants);
            }
        }
        if caps.contains(Capability::ProjectSelect) && caps.contains(Capability::Filesystem) {
            rows.extend([
                ProjectConfigRow::Heading(Section::MicroPython),
                ProjectConfigRow::MpyProjects,
            ]);
        }
        if caps.contains(Capability::OtaPrepare) {
            rows.extend([
                ProjectConfigRow::Heading(Section::OverTheAir),
                ProjectConfigRow::OtaMethod,
                ProjectConfigRow::OtaTransport,
                ProjectConfigRow::OtaAddress,
                ProjectConfigRow::OtaAutoConfirm,
            ]);
        }
        self.rows = rows;
        self.clamp();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn user_config(&self) -> &Path {
        &self.user_config
    }

    pub fn file_exists(&self) -> bool {
        self.path.exists()
    }

    pub fn rows(&self) -> &[ProjectConfigRow] {
        &self.rows
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// The chosen backend --- the card the strip highlights, which is the
    /// session's own until a different one is picked.
    pub fn chosen(&self) -> Option<BackendKind> {
        self.chosen
    }

    /// Whether the choice differs from what the session resolved.
    pub fn backend_changed(&self) -> bool {
        self.chosen != self.backend
    }

    pub fn selected(&self) -> Option<ProjectConfigRow> {
        match self.cursor {
            Cursor::Cards => None,
            Cursor::Row(index) => self.rows.get(index).copied(),
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub fn from_startup(&self) -> bool {
        self.from_startup
    }

    pub fn editing(&self) -> Option<(ProjectConfigRow, &str)> {
        self.edit
            .as_ref()
            .map(|edit| (edit.row, edit.input.as_str()))
    }

    /// Which half the arrows drive.
    pub fn details_focus(&self) -> DocsFocus {
        self.details_focus
    }

    /// `Tab`: hands the keyboard to the other half of the window.
    pub fn toggle_details_focus(&mut self) {
        self.details_focus = self.details_focus.toggled();
    }

    /// A click names the half it landed on (the docs pickers' rule).
    pub fn set_details_focus(&mut self, focus: DocsFocus) {
        self.details_focus = focus;
    }

    /// The details pane's scroll offset (rows).
    pub fn details_scroll(&self) -> usize {
        self.details_scroll
    }

    /// Scrolls the details pane; the renderer clamps against the wrapped
    /// length it alone knows.
    pub fn scroll_details(&mut self, delta: isize) {
        self.details_scroll = self.details_scroll.saturating_add_signed(delta);
    }

    /// Every answer waiting to be written, the backend choice included.
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    pub fn is_dirty(&self) -> bool {
        self.backend_changed() || !self.pending.is_empty()
    }

    /// How many separate lines applying would write.
    pub fn change_count(&self) -> usize {
        self.pending.len() + usize::from(self.backend_changed())
    }

    /// The pending answer for `row`: `Some(Some(value))` sets it,
    /// `Some(None)` removes it, `None` means the row is untouched.
    pub fn pending_for(&self, row: ProjectConfigRow) -> Option<Option<&str>> {
        self.pending
            .iter()
            .find(|change| change.row == row)
            .map(|change| change.value.as_deref())
    }

    /// The value on disk for `row`, from whichever file owns it.
    pub fn saved(&self, row: ProjectConfigRow) -> Option<String> {
        let (section, key) = row.slot()?;
        let text = match row.destination() {
            Destination::User => &self.user_text,
            _ => &self.text,
        };
        config::key_value(text, section, key)
    }

    /// What the row would read as once applied: the pending answer if there
    /// is one, else what is on disk.
    pub fn value(&self, row: ProjectConfigRow) -> Option<String> {
        match self.pending_for(row) {
            Some(pending) => pending.map(str::to_string),
            None => self.saved(row),
        }
    }

    /// The theme this window is previewing, or `None` when it has no
    /// opinion --- read by `App::previewed_theme`, which is how the preview
    /// happens without the session being changed to produce it.
    pub fn previewed_theme(&self) -> Option<ThemeChoice> {
        let value = self.value(ProjectConfigRow::Theme)?;
        ThemeChoice::from_slug(&value)
    }

    /// The icon set this window is previewing, on the same terms.
    pub fn previewed_icons(&self) -> Option<IconSet> {
        IconSet::from_slug(&self.value(ProjectConfigRow::Icons)?)
    }

    pub fn variants(&self) -> usize {
        config::parse_variants(&self.text).len()
    }

    fn first_selectable(&self) -> usize {
        self.rows
            .iter()
            .position(|row| row.kind() != RowKind::Heading)
            .unwrap_or(0)
    }

    fn clamp(&mut self) {
        let Cursor::Row(index) = self.cursor else {
            return;
        };
        if self.rows.is_empty() {
            self.cursor = Cursor::Cards;
            return;
        }
        let index = index.min(self.rows.len() - 1);
        self.cursor = Cursor::Row(index);
        if self.rows[index].kind() == RowKind::Heading {
            self.step(1);
        }
    }

    /// Moves the cursor by `delta`, skipping headings. The cards are the
    /// stop above the first row, and the ends hold rather than wrap --- the
    /// checklist's grammar, not a picker's.
    pub fn step(&mut self, delta: isize) {
        self.edit = None;
        // A new row is a new details document --- start from its top.
        self.details_scroll = 0;
        if self.rows.is_empty() {
            self.cursor = Cursor::Cards;
            return;
        }
        let last = self.rows.len() as isize - 1;
        let mut index = match self.cursor {
            Cursor::Cards if delta <= 0 => return,
            Cursor::Cards => -1,
            Cursor::Row(index) => index as isize,
        };
        loop {
            let next = index + delta;
            if next < 0 {
                self.cursor = Cursor::Cards;
                return;
            }
            if next > last {
                return;
            }
            index = next;
            if self.rows[index as usize].kind() != RowKind::Heading {
                self.cursor = Cursor::Row(index as usize);
                return;
            }
        }
    }

    /// Puts the cursor on the cards (a click on the strip, or `Home`).
    pub fn select_cards(&mut self) {
        self.edit = None;
        self.details_scroll = 0;
        self.cursor = Cursor::Cards;
    }

    /// Selects a list row, if it is selectable (a click's grammar: select,
    /// never activate).
    pub fn select(&mut self, index: usize) {
        if self
            .rows
            .get(index)
            .is_some_and(|row| row.kind() != RowKind::Heading)
        {
            self.edit = None;
            self.details_scroll = 0;
            self.cursor = Cursor::Row(index);
        }
    }

    /// Picks a backend card.
    ///
    /// The sections belong to the backend that owns them, so leaving one
    /// takes its unapplied answers with it --- and says how many, because a
    /// pending change the window cannot show is a change the user cannot
    /// review before applying it.
    pub fn choose(&mut self, kind: BackendKind, caps_for: impl Fn(BackendKind) -> Capabilities) {
        if self.chosen == Some(kind) {
            return;
        }
        self.edit = None;
        self.error = None;
        self.details_scroll = 0;
        let before = self.pending.len();
        let general = |row: ProjectConfigRow| {
            matches!(row.destination(), Destination::User | Destination::Registry)
        };
        self.pending.retain(|change| general(change.row));
        let dropped = before - self.pending.len();
        self.notice = (dropped > 0).then(|| {
            Notice::Lost(format!(
                "{dropped} unapplied {} discarded with the previous backend",
                if dropped == 1 { "answer" } else { "answers" }
            ))
        });
        self.chosen = Some(kind);
        self.rebuild(caps_for(kind));
    }

    /// Steps the card strip by `delta`, clamped.
    pub fn step_card(&mut self, delta: isize, caps_for: impl Fn(BackendKind) -> Capabilities) {
        let all = BackendKind::ALL;
        let at = self
            .chosen
            .and_then(|kind| all.iter().position(|candidate| *candidate == kind));
        let next = match at {
            Some(at) => (at as isize + delta).clamp(0, all.len() as isize - 1) as usize,
            // No choice yet: either end of the strip is one step away.
            None if delta < 0 => all.len() - 1,
            None => 0,
        };
        self.choose(all[next], caps_for);
    }

    pub fn begin_edit(&mut self) {
        let Some(row) = self.selected() else { return };
        if row.kind() != RowKind::Text {
            return;
        }
        self.error = None;
        self.notice = None;
        self.edit = Some(Edit {
            row,
            input: self.value(row).unwrap_or_default(),
        });
    }

    pub fn cancel_edit(&mut self) {
        self.edit = None;
    }

    pub fn push_char(&mut self, ch: char) {
        if let Some(edit) = &mut self.edit {
            edit.input.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(edit) = &mut self.edit {
            edit.input.pop();
        }
    }

    /// `Del` inside an edit: empties the field.
    ///
    /// The field opens seeded with the current answer, which is right for
    /// correcting a path and wrong for replacing one --- without this,
    /// swapping a long workspace path for another means holding backspace
    /// through the old one. `Del` clears the whole key outside an edit, so
    /// it means the same thing in both places: get rid of what is there.
    pub fn clear_input(&mut self) {
        if let Some(edit) = &mut self.edit {
            edit.input.clear();
        }
    }

    /// Records what was typed. An emptied field clears the key: absent is
    /// how the file says "no answer", and blank is not.
    pub fn commit_edit(&mut self) {
        let Some(edit) = self.edit.take() else { return };
        let value = edit.input.trim().to_string();
        self.record(edit.row, (!value.is_empty()).then_some(value));
    }

    /// Steps the selected choice row by `delta`, the empty answer being the
    /// last stop of the cycle.
    pub fn cycle(&mut self, delta: isize) {
        let Some(row) = self.selected() else { return };
        let RowKind::Choice(ids) = row.kind() else {
            return;
        };
        let current = self.value(row);
        let len = ids.len() as isize + 1;
        let at = current
            .as_deref()
            .and_then(|value| ids.iter().position(|id| id == value))
            .map_or(len - 1, |index| index as isize);
        let next = (at + delta).rem_euclid(len);
        let value = (next < ids.len() as isize).then(|| ids[next as usize].clone());
        self.record(row, value);
    }

    /// `Del`: clears the selected row's key.
    pub fn clear_selected(&mut self) {
        let Some(row) = self.selected() else { return };
        if matches!(row.kind(), RowKind::Heading | RowKind::Report) {
            return;
        }
        self.record(row, None);
    }

    /// Records one answer, dropping it again when it matches what is
    /// already on disk --- a change back to the saved value is not a change,
    /// and counting it would put a line in the review dialog that writes
    /// nothing.
    fn record(&mut self, row: ProjectConfigRow, value: Option<String>) {
        self.error = None;
        self.notice = None;
        self.pending.retain(|change| change.row != row);
        if value.as_deref() != self.saved(row).as_deref() {
            self.pending.push(Pending { row, value });
        }
    }

    /// Drops every unapplied answer, the backend choice included.
    pub fn discard(&mut self, caps_for: impl Fn(Option<BackendKind>) -> Capabilities) {
        self.pending.clear();
        self.edit = None;
        self.error = None;
        self.notice = None;
        if self.chosen != self.backend {
            self.chosen = self.backend;
            self.rebuild(caps_for(self.backend));
        }
    }

    /// Writes every pending answer that belongs to a file this panel owns
    /// (the project's `chiptui.toml` and the user config's `[ui]`), and
    /// forgets them.
    ///
    /// The registry entry and the backend choice are the caller's half:
    /// both need the session rebuilt around them, which is not something a
    /// panel can do. Errors stop at the first one and leave the rest
    /// pending, so a failed apply can be read and retried rather than
    /// half-forgotten.
    pub fn write_files(&mut self) -> Result<(), String> {
        let changes: Vec<Pending> = self
            .pending
            .iter()
            .filter(|change| {
                matches!(
                    change.row.destination(),
                    Destination::Project | Destination::User
                )
            })
            .cloned()
            .collect();
        for change in changes {
            let Some((section, key)) = change.row.slot() else {
                continue;
            };
            let target = match change.row.destination() {
                Destination::User => &self.user_config,
                _ => &self.path,
            };
            let wrote = match &change.value {
                Some(value) => config::set_key(target, section, key, value),
                None => config::clear_key(target, section, key),
            };
            if let Err(err) = wrote {
                let message = format!("cannot write {}: {err}", target.display());
                self.error = Some(message.clone());
                return Err(message);
            }
            self.pending.retain(|other| other.row != change.row);
        }
        Ok(())
    }

    /// Re-reads both files and forgets what was applied.
    pub fn settle(&mut self, backend: Option<BackendKind>) {
        self.text = std::fs::read_to_string(&self.path).unwrap_or_default();
        self.user_text = std::fs::read_to_string(&self.user_config).unwrap_or_default();
        self.backend = backend;
        self.chosen = backend;
        self.pending.clear();
        self.edit = None;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    /// A line for the header, cleared by the next answer --- what just
    /// happened, said where the user is looking rather than only in the log
    /// behind the window.
    pub fn set_notice(&mut self, notice: Notice) {
        self.error = None;
        self.notice = Some(notice);
    }
}

/// The spelling a choice row's value is shown with. Ids are what the file
/// carries; a person reads the word the rest of the application uses for
/// the same thing.
pub fn choice_label(row: ProjectConfigRow, id: &str) -> String {
    match row {
        ProjectConfigRow::Theme => ThemeChoice::from_slug(id)
            .map(|choice| choice.display_name().to_string())
            .unwrap_or_else(|| id.to_string()),
        ProjectConfigRow::Mouse => match id {
            "true" => "on".to_string(),
            "false" => "off".to_string(),
            other => other.to_string(),
        },
        ProjectConfigRow::OtaMethod => OtaMethod::from_id(id)
            .map(|method| method.label().to_string())
            .unwrap_or_else(|| id.to_string()),
        ProjectConfigRow::OtaTransport => Transport::from_id(id)
            .map(|transport| transport.label().to_string())
            .unwrap_or_else(|| id.to_string()),
        _ => id.to_string(),
    }
}

/// The one-line description under a backend's name on its card. Written for
/// someone deciding, so it names the tooling they will meet rather than the
/// architecture: what runs on the board, and what runs on the host.
pub const fn backend_summary(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::MicroPython => "a REPL and files on the board",
        BackendKind::Zephyr => "an image built with west",
    }
}
