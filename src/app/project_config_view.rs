//! The project configuration screen's glue: opening it, driving its keys,
//! and applying the transaction it collects.
//!
//! `SPEC.md` §7, §13. The window itself ([`crate::project_config`]) is pure
//! --- it knows two files and nothing about the app. What lives here is the
//! context it cannot have: which backend the session resolved, what already
//! answers a key the file leaves empty, and everything that has to happen
//! *around* a backend answer once it is applied.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, DevicePaneTab, Overlay};
use crate::backend::{BackendKind, Capabilities};
use crate::project::DetectionOutcome;
use crate::project_config::{Destination, Pending, ProjectConfigPanel, ProjectConfigRow, RowKind};

impl App {
    /// The directory the configuration file belongs to: the detected
    /// project root when there is one, else where the session started ---
    /// the same answer [`crate::project::ProjectManager::scaffold_dir`]
    /// gives, and it has to be, or the file would be written somewhere
    /// detection never reads it back from.
    pub(super) fn project_config_root(&self) -> std::path::PathBuf {
        self.manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf)
    }

    /// The capabilities a backend would bring, asked of the registry rather
    /// than of the session --- the window shows a *chosen* backend's
    /// sections before anything is applied, so it cannot read them off a
    /// `ProjectManager` that still holds the old answer.
    pub(super) fn capabilities_of(&self, kind: Option<BackendKind>) -> Capabilities {
        kind.map_or_else(Capabilities::empty, |kind| {
            self.manager.registry().capabilities(Some(kind))
        })
    }

    /// Opens the screen (`ctrl+,` or `,`, or by itself at startup).
    ///
    /// The panel is rebuilt every time rather than reused: every value it
    /// shows is read off the two files it edits, so a fresh one costs two
    /// `read_to_string`s and can never disagree with what an editor did to
    /// them meanwhile.
    pub fn open_project_config(&mut self, from_startup: bool) {
        let root = self.project_config_root();
        let user = self.user_config_path();
        let backend = self.manager.selected_kind();
        self.project_config = Some(ProjectConfigPanel::new(
            &root,
            &user,
            backend,
            self.capabilities_of(backend),
            from_startup,
        ));
        self.overlay = Some(Overlay::ProjectConfig);
    }

    /// Opens the screen when detection has nothing to go on: `Unknown` or
    /// `Ambiguous`, with no session override and no answer already
    /// recorded.
    ///
    /// This replaced the empty-project prompt, which asked the same question
    /// and recorded the answer only in this machine's registry. The window
    /// asks it by writing the project's own `chiptui.toml`, which is the
    /// answer that travels with the directory --- and it is why a Zephyr
    /// repository whose root is a *module* rather than an application (no
    /// `find_package(Zephyr)` to score, so 0.25 confidence and `Unknown`)
    /// can be opened at all.
    ///
    /// Deliberately **not** called from inside [`App::detect`], for the
    /// reason the prompt was not either: `detect()`/`bootstrap()` are called
    /// directly by many tests that send key events straight afterwards, and
    /// auto-opening a modal there would silently redirect their next press
    /// into `on_overlay_key`. Only the two real "detection just ran and the
    /// user might act on it" call sites opt in --- the binary's startup
    /// sequence and the `r` re-detect key.
    pub fn maybe_open_project_config(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(detection) = self.manager.detection() else {
            return;
        };
        if self.manager.override_kind().is_some()
            || matches!(
                detection.source,
                crate::project::DetectionSource::Config
                    | crate::project::DetectionSource::Registered
            )
        {
            return;
        }
        if matches!(
            detection.outcome,
            DetectionOutcome::Unknown | DetectionOutcome::Ambiguous(_)
        ) {
            self.open_project_config(true);
        }
    }

    pub(super) fn on_project_config_key(&mut self, key: KeyEvent) {
        // Read before the panel borrow: paging the details pane moves by
        // the rows the last frame actually drew.
        let page = self.config_details_viewport.max(1) as isize;
        let Some(panel) = &mut self.project_config else {
            self.overlay = None;
            return;
        };

        // While a row is being typed into, every printable key belongs to
        // it and `Esc` cancels the edit rather than the window.
        if panel.editing().is_some() {
            match key.code {
                KeyCode::Esc => panel.cancel_edit(),
                KeyCode::Backspace => panel.backspace(),
                KeyCode::Delete => panel.clear_input(),
                KeyCode::Char(ch) => panel.push_char(ch),
                KeyCode::Enter => panel.commit_edit(),
                _ => {}
            }
            return;
        }

        // `ctrl+s` applies from anywhere in the window --- the row under the
        // cursor has nothing to do with a transaction over the whole of it.
        if key
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('s'))
        {
            self.request_apply_config();
            return;
        }

        match key.code {
            KeyCode::Esc => self.leave_project_config(),
            // The pickers' two-pane grammar: `Tab` hands the keyboard to
            // the details pane, whose arrows then scroll it --- the whole
            // options list of a choice row lives there, and the themes'
            // thirty-odd are reachable no other way. `k`/`j` keep walking
            // rows either way; the pane answers the arrows only.
            KeyCode::Tab => panel.toggle_details_focus(),
            KeyCode::Up | KeyCode::Down
                if panel.details_focus() == crate::app::DocsFocus::Details =>
            {
                panel.scroll_details(if key.code == KeyCode::Up { -1 } else { 1 });
            }
            KeyCode::PageUp => panel.scroll_details(-page),
            KeyCode::PageDown => panel.scroll_details(page),
            KeyCode::Up | KeyCode::Char('k') => panel.step(-1),
            KeyCode::Down | KeyCode::Char('j') => panel.step(1),
            KeyCode::Home => panel.select_cards(),
            KeyCode::End => panel.step(panel.rows().len() as isize),
            KeyCode::Delete => panel.clear_selected(),
            KeyCode::Left | KeyCode::Right => {
                let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                match panel.selected() {
                    None => self.step_config_card(delta),
                    Some(row) if matches!(row.kind(), RowKind::Choice(_)) => panel.cycle(delta),
                    Some(_) => {}
                }
            }
            KeyCode::Enter => match panel.selected() {
                // On the cards, `Enter` accepts the choice and moves into
                // the settings it just revealed. Stepping the strip instead
                // would be a no-op on the last card --- and "next" is what
                // the arrows already mean here.
                None => panel.step(1),
                Some(row) => match row.kind() {
                    RowKind::Text => panel.begin_edit(),
                    RowKind::Choice(_) => panel.cycle(1),
                    _ => {}
                },
            },
            _ => {}
        }
    }

    fn step_config_card(&mut self, delta: isize) {
        let caps: Vec<(BackendKind, Capabilities)> = BackendKind::ALL
            .iter()
            .map(|kind| (*kind, self.capabilities_of(Some(*kind))))
            .collect();
        if let Some(panel) = &mut self.project_config {
            panel.step_card(delta, |kind| {
                caps.iter()
                    .find(|(candidate, _)| *candidate == kind)
                    .map_or_else(Capabilities::empty, |(_, caps)| *caps)
            });
        }
    }

    /// A click on a backend card.
    pub(super) fn choose_config_backend(&mut self, kind: BackendKind) {
        let caps = self.capabilities_of(Some(kind));
        if let Some(panel) = &mut self.project_config {
            panel.select_cards();
            panel.choose(kind, |_| caps);
        }
    }

    /// Selects a clicked list row without activating it (the picker grammar
    /// every overlay list follows).
    pub(super) fn project_config_select(&mut self, index: usize) {
        if let Some(panel) = &mut self.project_config {
            panel.select(index);
        }
    }

    /// `ctrl+s`: opens the review dialog, which is the window's one
    /// confirmation. Nothing has reached either file before this point.
    pub(super) fn request_apply_config(&mut self) {
        let Some(panel) = &self.project_config else {
            return;
        };
        if !panel.is_dirty() {
            return;
        }
        self.overlay = Some(Overlay::ConfirmApplyConfig { confirm: true });
    }

    /// `Esc`: leaves, asking first when there is something to lose.
    pub(super) fn leave_project_config(&mut self) {
        let dirty = self
            .project_config
            .as_ref()
            .is_some_and(ProjectConfigPanel::is_dirty);
        if dirty {
            self.overlay = Some(Overlay::ConfirmDiscardConfig { selected: 0 });
            return;
        }
        self.close_project_config();
    }

    /// The discard dialog's third answer: write the transaction, then
    /// leave. A failed write hands the window back with the error instead
    /// --- closing then would lose both the changes and the reason --- and
    /// a successful one that opened something of its own (a device picker
    /// from the rescan) leaves that up, since it outranks the request to
    /// close.
    pub(super) fn apply_and_close_project_config(&mut self) {
        self.apply_project_config();
        let failed = self
            .project_config
            .as_ref()
            .is_some_and(|panel| panel.error().is_some());
        if failed {
            return;
        }
        if matches!(self.overlay, Some(Overlay::ProjectConfig)) {
            self.close_project_config();
        }
    }

    /// Drops every unapplied answer and leaves --- the discard dialog's
    /// yes, and the second half of the `Esc` that raised it. Discarding
    /// without leaving is not an act the window offers: the way to undo one
    /// answer is to give it again.
    pub(super) fn discard_project_config(&mut self) {
        let backend = self.manager.selected_kind();
        let caps = self.capabilities_of(backend);
        if let Some(panel) = &mut self.project_config {
            panel.discard(|_| caps);
        }
        self.close_project_config();
    }

    /// The one way out, shared by `Esc` and by a click outside the box so
    /// the two cannot drift (the installer's `close_installer` rule).
    ///
    /// Where it goes is the question the window inherited from the routing
    /// change. A session that opened *because* the directory named no
    /// project has nothing behind this window --- the dashboard under it is
    /// a project that does not exist --- so leaving without answering goes
    /// back to the home screen, which is the screen that directory used to
    /// route to. A window the user opened deliberately simply closes.
    ///
    /// The environment's own first question rides the way out of a startup
    /// window rather than the answer to it: `maybe_open_workspace_picker`
    /// does nothing while an overlay is open, so this is the only moment it
    /// can land --- while a window the user opened deliberately closes onto
    /// the dashboard and nothing else, since throwing a directory picker
    /// over a screen they just dismissed answers nothing they asked.
    pub fn close_project_config(&mut self) {
        let from_startup = self
            .project_config
            .as_ref()
            .is_some_and(ProjectConfigPanel::from_startup);
        self.overlay = None;
        self.project_config = None;
        if !from_startup {
            return;
        }
        if self.manager.selected_kind().is_none() {
            self.request_home_screen();
            return;
        }
        self.maybe_open_workspace_picker();
        // The project question's entry form follows the same rule: it lands
        // only when the workspace question did not (the guard is inside),
        // so a repository entered from its module root is asked about its
        // one application here rather than at the first build press.
        self.maybe_open_entry_project();
    }

    /// Writes the transaction: the two files the panel owns, then the
    /// registry entry, the backend answer and everything the session has to
    /// rebuild around it.
    pub(super) fn apply_project_config(&mut self) {
        let Some(panel) = &mut self.project_config else {
            return;
        };
        let chosen = panel.chosen();
        let backend_changed = panel.backend_changed();
        let name = panel
            .pending_for(ProjectConfigRow::Name)
            .map(|value| value.map(str::to_string));
        let count = panel.change_count();
        // Asked before a single byte is written: `chiptui.toml` is not a
        // hidden entry, so the transaction's own first write is what would
        // end the emptiness the scaffold is gated on --- and the starting
        // layout the review dialog just promised would never be created.
        let was_empty = crate::startup::is_empty_dir(panel.root());

        if let Err(message) = panel.write_files() {
            self.logs.error(message);
            self.overlay = Some(Overlay::ProjectConfig);
            return;
        }

        if backend_changed {
            self.apply_project_type(chosen, was_empty);
        }
        if let Some(name) = name {
            self.rename_project_entry(name);
        }
        // Every other pending answer is a file this session reads back
        // rather than holds, so re-resolving is all that is left.
        self.refresh_workspace_resolution();
        self.reload_mpy_projects();
        self.report_tools();

        let backend = self.manager.selected_kind();
        let caps = self.capabilities_of(backend);
        let applied = format!(
            "{count} {} applied",
            if count == 1 { "change" } else { "changes" }
        );
        if let Some(panel) = &mut self.project_config {
            panel.settle(backend);
            panel.rebuild(caps);
            panel.set_notice(crate::project_config::Notice::Done(applied.clone()));
        }
        self.logs.success(applied);
        // The window comes back --- unless applying opened something of its
        // own. Scanning for devices after a backend answer can raise the
        // device picker, and restoring unconditionally would drop it on the
        // floor the moment it appeared: the overlay slot is one deep, and
        // whatever is in it now got there during this apply.
        if self.overlay.is_none() {
            self.overlay = Some(Overlay::ProjectConfig);
        }
    }

    /// Renames the project's registry entry --- the name the home screen
    /// lists it under. An emptied name falls back to the folder's own,
    /// which is what [`crate::settings::ProjectEntry::new`] would have
    /// given it.
    fn rename_project_entry(&mut self, name: Option<String>) {
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let root = self.project_config_root();
        let mut entry = match self.manager.known_projects().entry_for(&root) {
            Some(known) => known.clone(),
            None => crate::settings::ProjectEntry::new(&root, kind),
        };
        entry.name = name.unwrap_or_else(|| crate::settings::ProjectEntry::new(&root, kind).name);
        let config = self.user_config_path();
        if let Err(err) = crate::settings::record_project(&config, entry) {
            self.logs
                .warn(format!("could not save the project name: {err}"));
        }
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
    }

    /// Writes `project_type` and rebuilds the session around it.
    ///
    /// The scaffold is written **only into an empty directory**. Scaffolding
    /// is what makes `mkdir x && cd x && chiptui` produce a usable project;
    /// a directory that already holds a project is not missing a
    /// `CMakeLists.txt`, and dropping files into someone's repository
    /// because they named its backend would be the passive write
    /// `SPEC.md` §7 forbids.
    /// `was_empty` is the caller's, not this function's: by the time the
    /// transaction reaches here it has already written the keys that made
    /// the directory non-empty, so asking now would answer about a
    /// directory this very apply had just changed.
    pub(super) fn apply_project_type(&mut self, kind: Option<BackendKind>, was_empty: bool) {
        let root = self.project_config_root();
        let path = root.join(crate::project::config::FILE_NAME);
        let wrote = match kind {
            Some(kind) => crate::project::config::set_project_type(&path, kind),
            None => crate::project::config::clear_project_type(&path),
        };
        if let Err(err) = wrote {
            self.logs
                .error(format!("cannot write {}: {err}", path.display()));
            return;
        }

        self.manager.set_override(kind);
        // The pane the old backend showed is not this backend's pane: the
        // actions tab belongs to a device pane that may not even exist yet,
        // so the tab starts over with it.
        self.device_pane_tab = DevicePaneTab::Files;

        match kind {
            Some(kind) => {
                if was_empty {
                    self.report_scaffold(kind);
                } else {
                    self.logs
                        .success(format!("{kind} recorded in {}", path.display()));
                }
                self.record_open_project();
            }
            None => self.logs.info(format!(
                "project type cleared in {} --- detection decides again",
                path.display()
            )),
        }

        self.ensure_workspace_panel();
        self.report_tools();
        self.maybe_scan_devices();
        // The answer is this backend's first entry: place focus the way the
        // startup route does rather than merely clamping --- the user has
        // not navigated anywhere yet to keep.
        self.place_startup_focus();
    }

    fn report_scaffold(&mut self, kind: BackendKind) {
        match self.manager.create_scaffold(kind) {
            Ok(created) if created.written.is_empty() => {
                self.logs.success(format!("{kind} selected"));
            }
            Ok(created) => {
                let names: Vec<String> = created
                    .written
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                self.logs
                    .success(format!("{kind} selected --- created {}", names.join(", ")));
            }
            Err(err) => self.logs.warn(format!(
                "{kind} selected, but the project layout could not be created: {err}"
            )),
        }
    }

    /// The files a pending set of answers would touch, named once each ---
    /// the review dialog's "where this lands" line.
    pub fn config_targets(&self) -> Vec<String> {
        let Some(panel) = &self.project_config else {
            return Vec::new();
        };
        let mut targets: Vec<String> = Vec::new();
        let mut push = |value: String| {
            if !targets.contains(&value) {
                targets.push(value);
            }
        };
        if panel.backend_changed() {
            push(panel.path().display().to_string());
        }
        for change in panel.pending() {
            match change.row.destination() {
                Destination::Project => push(panel.path().display().to_string()),
                Destination::User | Destination::Registry => {
                    push(panel.user_config().display().to_string());
                }
                Destination::ReadOnly => {}
            }
        }
        targets
    }

    /// The starting layout applying would create, when it would --- the
    /// review dialog's most consequential line, and the reason the whole
    /// window became a transaction.
    pub fn config_scaffold(&self) -> Vec<String> {
        let Some(panel) = &self.project_config else {
            return Vec::new();
        };
        if !panel.backend_changed() || !crate::startup::is_empty_dir(panel.root()) {
            return Vec::new();
        }
        let Some(kind) = panel.chosen() else {
            return Vec::new();
        };
        let name = panel
            .root()
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().to_string());
        self.manager
            .registry()
            .get(kind)
            .map(|backend| {
                backend
                    .scaffold(&name)
                    .files
                    .iter()
                    .map(|file| file.path.display().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What answers a row the files leave empty, and where that answer comes
    /// from --- the muted half of every line.
    ///
    /// This is the window's real teaching: a key absent from the file is not
    /// an unanswered question, it is a question answered somewhere less
    /// specific. Showing where turns "why is my board wrong" into a fact on
    /// the screen.
    pub fn project_config_fallback(&self, row: ProjectConfigRow) -> Option<(String, &'static str)> {
        match row {
            ProjectConfigRow::Root => Some((
                self.project_config_root().display().to_string(),
                "this session",
            )),
            ProjectConfigRow::Name => {
                let root = self.project_config_root();
                match self.manager.known_projects().entry_for(&root) {
                    Some(entry) => Some((entry.name.clone(), "project registry")),
                    // Not recorded yet: the name it *would* get, which is
                    // the folder's own --- an empty row here would read as
                    // "this project has no name", and it has one.
                    None => Some((
                        root.file_name()?.to_string_lossy().to_string(),
                        "the folder",
                    )),
                }
            }
            ProjectConfigRow::Variants => {
                let count = self.project_config.as_ref()?.variants();
                Some((format!("{count} declared"), "chiptui.toml"))
            }
            ProjectConfigRow::Theme => {
                Some((self.theme_choice().display_name().to_string(), "default"))
            }
            ProjectConfigRow::Icons => Some((self.icon_set().slug().to_string(), "default")),
            ProjectConfigRow::Mouse => Some(("off".to_string(), "default")),
            ProjectConfigRow::ZephyrWorkspace
            | ProjectConfigRow::ZephyrProjects
            | ProjectConfigRow::ZephyrSdk
            | ProjectConfigRow::ZephyrWest => {
                let user = crate::settings::load_user(&self.config_dir)?;
                let value = match row {
                    ProjectConfigRow::ZephyrWorkspace => user.workspace,
                    ProjectConfigRow::ZephyrProjects => user.projects,
                    ProjectConfigRow::ZephyrSdk => user.sdk,
                    _ => user.west,
                }?;
                Some((value, "user config"))
            }
            ProjectConfigRow::MpyProjects => Some((
                crate::settings::mpy_projects_raw(&self.config_dir)?,
                "user config",
            )),
            // Not a less-specific *level*, but the same statement the
            // stack makes everywhere else: the key is absent and something
            // else is answering. Here it is the discovery, and naming it
            // is what says the answer would move the day the repository
            // grows a second application.
            ProjectConfigRow::ZephyrApp => {
                let root = self.project_config_root();
                match crate::backend::zephyr::projects::resolve_app(&root)? {
                    crate::backend::zephyr::projects::AppSource::Root => {
                        Some(("the project folder itself".to_string(), "resolved"))
                    }
                    crate::backend::zephyr::projects::AppSource::Dir(app) => Some((
                        app.strip_prefix(&root)
                            .unwrap_or(&app)
                            .display()
                            .to_string(),
                        "resolved: the only application inside",
                    )),
                }
            }
            ProjectConfigRow::ZephyrBuildArgs => {
                let panel = self.build.as_ref()?;
                let derived = panel.cmake_args();
                (!derived.is_empty()).then(|| (derived.join(" "), "the project's board module"))
            }
            ProjectConfigRow::ZephyrBoard | ProjectConfigRow::ZephyrShield => {
                let panel = self.build.as_ref()?;
                let choice = panel.board.as_ref()?;
                let value = if row == ProjectConfigRow::ZephyrBoard {
                    choice.name.clone()
                } else {
                    panel.shield.clone()?
                };
                Some((value, choice.origin.label()))
            }
            _ => None,
        }
    }
}

impl App {
    /// The review dialog's body: the lines about to be written, then the
    /// starting layout that would be created, then the files each lands in.
    pub fn config_review_lines(&self) -> Vec<String> {
        let Some(panel) = &self.project_config else {
            return Vec::new();
        };
        let backend_line = panel.backend_changed().then(|| match panel.chosen() {
            Some(kind) => format!("project_type = \"{}\"", kind.id()),
            None => "remove project_type".to_string(),
        });
        let mut lines = review_lines(panel.pending(), backend_line);
        let scaffold = self.config_scaffold();
        if !scaffold.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "creates {} starting {}",
                scaffold.len(),
                if scaffold.len() == 1 { "file" } else { "files" }
            ));
            for file in scaffold {
                lines.push(format!("  {file}"));
            }
        }
        lines
    }
}

/// The lines a pending set of answers writes, grouped by their section, for
/// the review dialog. Free of `App` so the shape is testable on its own.
pub fn review_lines(pending: &[Pending], backend_line: Option<String>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(line) = backend_line {
        lines.push(line);
    }
    let mut section: Option<&'static str> = None;
    for change in pending {
        if change.row.destination() == Destination::ReadOnly {
            continue;
        }
        let next = change.section();
        if next != section {
            if let Some(name) = next {
                lines.push(format!("[{name}]"));
            }
            section = next;
        }
        lines.push(format!("  {}", change.line()));
    }
    lines
}
