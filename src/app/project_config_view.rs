//! The project configuration screen's glue: opening it, driving its keys,
//! and applying the transaction it collects.
//!
//! `SPEC.md` §7, §13. The window itself ([`crate::project_config`]) is pure
//! --- it knows two files and nothing about the app. What lives here is the
//! context it cannot have: which backend the session resolved, what already
//! answers a key the file leaves empty, and everything that has to happen
//! *around* a backend answer once it is applied.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, DevicePaneTab, Overlay};
use crate::backend::zephyr::simulator;
use crate::backend::{BackendKind, Capabilities};
use crate::project::DetectionOutcome;
use crate::project_config::{
    Destination, Pending, ProjectConfigPanel, ProjectConfigRow, RowKind, SimulatorChange,
};

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
        self.cancel_simulator_probe();
        let root = self.project_config_root();
        let user = self.user_config_path();
        let backend = self.manager.selected_kind();
        // The row list is the file's shape, but a project that has only
        // ever *built* its variants owns them just the same --- this
        // snapshot decides the row's presence, while the drawn list reads
        // the session live.
        let session_variants = self.build.as_ref().map_or(0, |build| build.variants.len());
        self.project_config = Some(ProjectConfigPanel::new(
            &root,
            &user,
            backend,
            self.capabilities_of(backend),
            session_variants,
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

    /// Offers the starting-layout picker when detection already knows an
    /// empty directory is Zephyr --- most importantly, when the project
    /// registry supplied the backend before this session began. That path
    /// never applies a backend change in the configuration screen, so the
    /// apply-time hook alone cannot reach it.
    ///
    /// Returns whether the picker opened, so a preceding startup question can
    /// hand off to it without opening another overlay. A configuration window
    /// already on screen owns the question instead and blocks this second
    /// door.
    pub fn maybe_offer_starting_layout(&mut self) -> bool {
        if self.overlay.is_some()
            || self.manager.selected_kind() != Some(BackendKind::Zephyr)
            || !crate::startup::is_empty_dir(&self.project_config_root())
        {
            return false;
        }
        self.maybe_offer_samples(BackendKind::Zephyr)
    }

    pub(super) fn on_project_config_key(&mut self, key: KeyEvent) {
        // Read before the panel borrow: paging the details pane moves by
        // the rows the last frame actually drew.
        let page = self.config_details_viewport.max(1) as isize;
        if self
            .project_config
            .as_ref()
            .is_some_and(|panel| panel.simulator_edit.is_some())
        {
            self.on_simulator_config_key(key);
            return;
        }
        let Some(panel) = &mut self.project_config else {
            self.overlay = None;
            return;
        };

        if panel.choosing_backend() {
            match key.code {
                KeyCode::BackTab | KeyCode::Left | KeyCode::Up => panel.step_card(-1),
                KeyCode::Tab | KeyCode::Right | KeyCode::Down => panel.step_card(1),
                KeyCode::Enter => {
                    let kind = panel.highlighted_backend();
                    self.choose_config_backend(kind);
                }
                KeyCode::Esc if panel.chosen().is_some() => panel.show_settings(),
                KeyCode::Esc => self.leave_project_config(),
                _ => {}
            }
            return;
        }

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
            KeyCode::Char('e')
                if key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
            {
                panel.begin_edit()
            }
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
            KeyCode::Home => panel.show_settings(),
            KeyCode::Char('b') => panel.select_cards(),
            KeyCode::End => panel.step(panel.rows().len() as isize),
            KeyCode::Delete => panel.clear_selected(),
            KeyCode::Left | KeyCode::Right => {
                let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                match panel.selected() {
                    None => {}
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
                    RowKind::Action if row == ProjectConfigRow::Simulator => {
                        self.open_simulator_config()
                    }
                    _ if row.uses_target_picker() => self.open_config_target_picker(row),
                    RowKind::Text if row.picker_kind().is_some() => self.open_config_path(row),
                    RowKind::Text => panel.begin_edit(),
                    RowKind::Choice(_) => panel.cycle(1),
                    _ => {}
                },
            },
            _ => {}
        }
    }

    fn open_simulator_config(&mut self) {
        let Some(panel) = &mut self.project_config else {
            return;
        };
        let declared = panel.variant_list();
        let declared_empty = declared.is_empty();
        let root = panel.root();
        let app = panel
            .value(ProjectConfigRow::ZephyrApp)
            .map(|relative| root.join(relative))
            .or_else(|| {
                self.build
                    .as_ref()
                    .filter(|b| b.root == root)
                    .and_then(|b| b.app_dir.clone())
            })
            .or_else(|| crate::backend::zephyr::projects::entry_child(root))
            .unwrap_or_else(|| root.to_path_buf());
        let variants = if declared_empty {
            let catalogue: Vec<_> = self
                .build
                .as_ref()
                .and_then(|b| match &b.boards.state {
                    crate::build::ListState::Loaded(boards) => {
                        Some(boards.iter().map(|b| b.name.clone()).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            crate::backend::zephyr::variants::discover_all(root, Some(&app), &catalogue)
        } else {
            declared
        };
        panel.open_simulator(&variants);
        if panel.sdl_probe.is_none() {
            panel.sdl_status = "Checking SDL2 development files…".into();
            panel.sdl_probe = Some(
                self.processes.spawn(
                    crate::process::Command::new(&panel.sdl_program)
                        .args(["--exists", "sdl2"])
                        .current_dir(panel.root()),
                    std::time::Duration::from_secs(5),
                ),
            );
        }
        // Preserve fragment-only device variants too. The existing catalogue
        // fetch is asynchronous; preparing waits for it instead of guessing
        // qualified board names from filenames.
        let has_fragments = app.join("boards").is_dir();
        if declared_empty && has_fragments {
            let backend = self.manager.backend();
            if let (Some(backend), Some(build)) = (backend, &mut self.build)
                && let Some(command) = build.boards_command(backend)
            {
                build.start_boards_fetch(command, &mut self.processes);
            }
        }
    }

    pub(super) fn on_simulator_config_key(&mut self, key: KeyEvent) {
        let Some(editor) = self
            .project_config
            .as_mut()
            .and_then(|p| p.simulator_edit.as_mut())
        else {
            return;
        };
        if editor.editing {
            let value = &mut editor.values[editor.selected - 1];
            match key.code {
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                {
                    value.push(ch)
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Delete => value.clear(),
                KeyCode::Enter => editor.editing = false,
                KeyCode::Esc => editor.cancel_edit(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel_simulator_form(),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                editor.selected = editor.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                editor.selected = (editor.selected + 1).min(6)
            }
            KeyCode::Left | KeyCode::Right if editor.selected == 0 => {
                editor.step_choice(if key.code == KeyCode::Left { -1 } else { 1 })
            }
            KeyCode::Left | KeyCode::Right if editor.selected >= 4 => {
                // Prepare / Remove / Cancel, cycling within the button row.
                let step = if key.code == KeyCode::Left { 2 } else { 1 };
                editor.selected = 4 + (editor.selected - 4 + step) % 3;
            }
            KeyCode::Enter => match editor.selected {
                0 => editor.step_choice(1),
                1..=3 => editor.begin_edit(),
                4 => self.prepare_simulator_config(),
                5 => self.remove_simulator_config(),
                _ => self.cancel_simulator_form(),
            },
            _ => {}
        }
    }

    fn cancel_simulator_form(&mut self) {
        self.cancel_simulator_probe();
        if let Some(panel) = &mut self.project_config {
            panel.simulator_edit = None;
        }
    }

    fn prepare_simulator_config(&mut self) {
        let result = self.simulator_preparation();
        if let Some(panel) = &mut self.project_config {
            match result {
                Ok(plan) => {
                    panel.stage_simulator(SimulatorChange::Prepare(plan));
                    self.request_apply_config();
                }
                Err(error) => {
                    if let Some(editor) = &mut panel.simulator_edit {
                        editor.error = Some(error);
                    }
                }
            }
        }
    }

    fn remove_simulator_config(&mut self) {
        let result = (|| {
            let panel = self
                .project_config
                .as_ref()
                .ok_or("Project configuration is closed")?;
            let editor = panel
                .simulator_edit
                .as_ref()
                .ok_or("Simulator settings are closed")?;
            if let Some(reason) = editor.remove_refusal() {
                return Err(reason);
            }
            let variant = editor
                .choices
                .get(editor.choice)
                .cloned()
                .expect("the refusal covers a missing choice");
            let declared = panel.variant_list();
            let app = self.simulator_app_dir()?;
            simulator::Removal::new(panel.root(), &app, &variant, &declared)
        })();
        match result {
            Ok(removal) => {
                if let Some(panel) = &mut self.project_config {
                    panel.stage_simulator(SimulatorChange::Remove(removal));
                }
                self.request_apply_config();
            }
            Err(error) => {
                if let Some(editor) = self
                    .project_config
                    .as_mut()
                    .and_then(|panel| panel.simulator_edit.as_mut())
                {
                    editor.error = Some(error);
                }
            }
        }
    }

    /// The application the simulator's fragments belong to --- shared by
    /// preparation and removal, which must agree on where the variant's
    /// files live.
    fn simulator_app_dir(&self) -> Result<PathBuf, String> {
        use crate::backend::zephyr::projects;
        let panel = self
            .project_config
            .as_ref()
            .ok_or("Project configuration is closed")?;
        let root = panel.root();
        if let Some(relative) = panel.value(ProjectConfigRow::ZephyrApp) {
            let path = crate::project::scaffold::resolve(root, Path::new(&relative))
                .map_err(|err| err.to_string())?;
            if !projects::is_buildable(&path) {
                return Err("Application folder is not a Zephyr application; correct it before preparing the simulator.".into());
            }
            Ok(path)
        } else if crate::startup::is_empty_dir(root) || projects::is_buildable(root) {
            Ok(root.to_path_buf())
        } else if let Some(path) = self
            .build
            .as_ref()
            .filter(|b| b.root == root)
            .and_then(|b| b.app_dir.clone())
            .or_else(|| projects::entry_child(root))
        {
            Ok(path)
        } else {
            Err("Choose the Application folder before preparing the simulator; existing project files will not be replaced.".into())
        }
    }

    fn simulator_preparation(
        &self,
    ) -> Result<crate::backend::zephyr::simulator::Preparation, String> {
        use crate::backend::zephyr::{simulator, variants, workspace};
        let panel = self
            .project_config
            .as_ref()
            .ok_or("Project configuration is closed")?;
        let editor = panel
            .simulator_edit
            .as_ref()
            .ok_or("Simulator settings are closed")?;
        let project_settings = crate::settings::ZephyrSettings {
            workspace: panel.value(ProjectConfigRow::ZephyrWorkspace),
            sdk: panel.value(ProjectConfigRow::ZephyrSdk),
            ..Default::default()
        };
        let user_settings = crate::settings::load_user(&self.config_dir);
        let workspace = match workspace::resolve(&workspace::ResolveInput {
            project_settings: Some(&project_settings), user_settings: user_settings.as_ref(), home: &self.home_dir,
        }) {
            workspace::Resolution::Single(workspace) => workspace,
            _ => return Err("Choose a valid Workspace path in project configuration before preparing LVGL + SDL.".into()),
        };
        simulator::check_workspace(&workspace.zephyr_base)?;
        let root = panel.root();
        let app = self.simulator_app_dir()?;
        let declared = panel.variant_list();
        let catalogue: Vec<_> = self
            .build
            .as_ref()
            .and_then(|b| match &b.boards.state {
                crate::build::ListState::Loaded(boards) => {
                    Some(boards.iter().map(|b| b.name.clone()).collect())
                }
                _ => None,
            })
            .unwrap_or_default();
        if declared.is_empty() && app.join("boards").is_dir() && catalogue.is_empty() {
            return Err("The board catalogue is needed to retain existing variants. Wait for the board scan, or apply the Zephyr backend and open the board picker, then prepare again.".into());
        }
        let discovered = variants::discover_all(root, Some(&app), &catalogue);
        simulator::Preparation::new(
            root,
            &app,
            editor.variant(),
            editor.original_name(),
            &declared,
            discovered,
        )
    }

    /// A click on a backend card.
    pub(super) fn choose_config_backend(&mut self, kind: BackendKind) {
        self.cancel_simulator_probe();
        let caps = self.capabilities_of(Some(kind));
        if let Some(panel) = &mut self.project_config {
            panel.choose(kind, |_| caps);
            panel.show_settings();
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
        let validation = panel.validate();
        if let Err(error) = validation {
            if let Some(panel) = &mut self.project_config {
                panel.set_error(error);
            }
            return;
        }
        self.overlay = Some(Overlay::ConfirmApplyConfig { confirm: true });
        self.project_config.as_mut().unwrap().review_scroll = 0;
    }

    pub(super) fn scroll_config_review(&mut self, code: KeyCode) -> bool {
        let delta = match code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -8,
            KeyCode::PageDown => 8,
            _ => return false,
        };
        if let Some(panel) = &mut self.project_config {
            panel.review_scroll = panel
                .review_scroll
                .saturating_add_signed(delta)
                .min(panel.review_max_scroll);
        }
        true
    }

    pub(super) fn on_simulator_probe(&mut self, event: &crate::process::ProcessEvent) -> bool {
        use crate::process::ProcessEvent;
        let Some(panel) = &mut self.project_config else {
            return false;
        };
        if panel.sdl_probe != Some(event.id()) {
            return false;
        }
        match event {
            ProcessEvent::Line { text, .. } => self.logs.info(text.clone()),
            ProcessEvent::Finished { outcome, .. } => {
                panel.sdl_probe = None;
                panel.sdl_status = if outcome.is_success() {
                    "SDL2 development files available".into()
                } else {
                    format!(
                        "SDL2 check failed ({}); install pkg-config and SDL2 development files before Build.",
                        outcome.summary()
                    )
                };
                if outcome.is_success() {
                    self.logs.success(&panel.sdl_status);
                } else {
                    self.logs.warn(&panel.sdl_status);
                }
            }
            _ => {}
        }
        true
    }

    fn cancel_simulator_probe(&mut self) {
        if let Some(id) = self
            .project_config
            .as_mut()
            .and_then(|panel| panel.sdl_probe.take())
        {
            self.processes.cancel(id);
        }
    }

    /// `Esc`: leaves, asking first when there is something to lose.
    pub(super) fn leave_project_config(&mut self) {
        let dirty = self
            .project_config
            .as_ref()
            .is_some_and(ProjectConfigPanel::is_dirty);
        if dirty {
            self.project_config.as_mut().unwrap().review_scroll = 0;
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
        self.cancel_simulator_probe();
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
        if self.maybe_offer_starting_layout() {
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
        let target_changed = panel.pending_for(ProjectConfigRow::ZephyrBoard).is_some()
            || panel.pending_for(ProjectConfigRow::ZephyrShield).is_some();
        let simulator_changed = panel.simulator_pending().is_some();
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
            self.apply_project_type(chosen, was_empty && !simulator_changed);
        }
        if let Some(name) = name {
            self.rename_project_entry(name);
        }
        // Every other pending answer is a file this session reads back
        // rather than holds, so re-resolving is all that is left.
        self.refresh_workspace_resolution();
        self.reload_mpy_projects();
        if target_changed || simulator_changed {
            self.refresh_config_target();
        }
        if simulator_changed {
            self.refresh_variants();
        }
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

    /// A target chosen in project configuration was deliberately not sent
    /// through the build picker's immediate persistence path. Once its file
    /// transaction lands, reflect the new, more-specific answer in a live
    /// build panel too.
    fn refresh_config_target(&mut self) {
        let root = self.project_config_root();
        let (board, shield) = self.target_answers(&root);
        let Some(panel) = &mut self.build else {
            return;
        };
        if let Some(board) = board {
            match board.origin {
                crate::build::BoardOrigin::ProjectFile => panel.set_project_file_board(board.name),
                _ => panel.set_config_board(board.name),
            }
        }
        panel.set_shield(shield);
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
                    // Zephyr's empty directory may start from one of the
                    // workspace's own samples instead of the minimal layout:
                    // when the tree lists any, that question owns the
                    // layout --- its answer writes the files and then runs
                    // the tail below, so the device scan never contends
                    // with the picker for the one-deep overlay slot.
                    if self.maybe_offer_samples(kind) {
                        self.record_open_project();
                        return;
                    }
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

        self.finish_backend_apply();
    }

    /// The tail every applied backend answer runs: resolve the environment
    /// around the new backend, refresh the tool report, start device
    /// discovery and place the startup focus. Shared with the sample
    /// picker, whose answer completes the apply the overlay deferred.
    pub(super) fn finish_backend_apply(&mut self) {
        self.ensure_workspace_panel();
        self.report_tools();
        self.maybe_scan_devices();
        // The answer is this backend's first entry: place focus the way the
        // startup route does rather than merely clamping --- the user has
        // not navigated anywhere yet to keep.
        self.place_startup_focus();
    }

    /// Offers the workspace's `samples/` tree as the new project's starting
    /// layout (`SPEC.md` §7), when there are any to offer. `true` means the
    /// picker opened and owns what happens next; `false` means the caller
    /// falls back to the minimal scaffold --- the answer for a backend
    /// without samples (MicroPython), and for a workspace that names none,
    /// with a log line saying why so the fallback never reads as a miss.
    fn maybe_offer_samples(&mut self, kind: BackendKind) -> bool {
        use crate::backend::zephyr::workspace::Resolution;

        if kind != BackendKind::Zephyr {
            return false;
        }
        // Resolved fresh from the files this apply just wrote, so a
        // `Zephyr path` answered in this very transaction already counts.
        let workspace = match self.resolve_workspace() {
            Resolution::Single(workspace) => workspace,
            Resolution::Invalid(_) => {
                self.logs
                    .info("Zephyr samples unavailable (the workspace is not a valid installation)");
                return false;
            }
            Resolution::NotConfigured => {
                self.logs
                    .info("Zephyr samples unavailable (no workspace configured yet)");
                return false;
            }
        };
        let dir = workspace.zephyr_base.join("samples");
        let samples = crate::backend::zephyr::samples::list_samples(&dir);
        if samples.is_empty() {
            self.logs
                .info(format!("no buildable samples under {}", dir.display()));
            return false;
        }
        self.overlay = Some(Overlay::SamplePicker {
            input: String::new(),
            selected: 0,
            scroll: 0,
            focus: crate::app::DocsFocus::List,
            samples,
        });
        self.docs_list_offset = 0;
        true
    }

    /// The sample picker's explicit answer: the minimal scaffold for row 0,
    /// or a copy of the picked sample's tree. Then the deferred tail of the
    /// apply runs, and the configuration window comes back with its applied
    /// notice --- unless the device scan opened a question of its own, the
    /// one-deep slot's standing rule.
    pub(super) fn apply_sample_pick(
        &mut self,
        sample: Option<crate::backend::zephyr::samples::Sample>,
    ) {
        match sample {
            None => self.report_scaffold(BackendKind::Zephyr),
            Some(sample) => {
                let root = self.manager.scaffold_dir().to_path_buf();
                match crate::backend::zephyr::samples::copy_sample(&sample.dir, &root) {
                    Ok(created) if created.written.is_empty() => {
                        self.logs
                            .success(format!("sample {} --- nothing new to copy", sample.rel));
                    }
                    Ok(created) => {
                        self.logs.success(format!(
                            "sample {} --- copied {}",
                            sample.rel,
                            created
                                .written
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    Err(err) => self.logs.warn(format!(
                        "sample {} could not be copied: {err} --- the directory keeps what was written",
                        sample.rel
                    )),
                }
            }
        }
        self.finish_backend_apply();
        if self.overlay.is_none() && self.project_config.is_some() {
            self.overlay = Some(Overlay::ProjectConfig);
        }
    }

    /// Cancelling the sample question is not an answer to its starting-layout
    /// question. Complete the already explicit backend selection, but leave
    /// the project tree untouched.
    pub(super) fn cancel_sample_picker(&mut self) {
        self.finish_backend_apply();
        if self.overlay.is_none() && self.project_config.is_some() {
            self.overlay = Some(Overlay::ProjectConfig);
        }
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
        if panel.simulator_pending().is_some() {
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
        if panel.simulator_pending().is_some() {
            return Vec::new(); // The simulator's own review names the actual layout.
        }
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
                let panel = self.project_config.as_ref()?;
                // Declared variants are the file's own answer; a project
                // that declares none but has built some reports where its
                // list really came from.
                let declared = panel.variants();
                (declared > 0)
                    .then(|| (format!("{declared} declared"), "chiptui.toml"))
                    .or_else(|| {
                        let session = panel.session_variants();
                        (session > 0).then(|| (format!("{session} discovered"), "this session"))
                    })
            }
            ProjectConfigRow::Simulator => {
                let panel = self.project_config.as_ref()?;
                self.build
                    .as_ref()
                    .and_then(|b| {
                        b.variants
                            .iter()
                            .find(|v| crate::backend::zephyr::simulator::is_graphical_target(v))
                    })
                    .map(|v| (format!("{} · LVGL + SDL", v.name), "discovered"))
                    .or_else(|| panel.saved(row).map(|value| (value, "chiptui.toml")))
                    .or_else(|| Some(("Not configured — Enter to add".into(), "default")))
            }
            ProjectConfigRow::Theme => {
                Some((self.theme_choice().display_name().to_string(), "default"))
            }
            ProjectConfigRow::Icons => Some((self.icon_set().slug().to_string(), "default")),
            ProjectConfigRow::Mouse => Some(("off".to_string(), "default")),
            ProjectConfigRow::ZephyrWorkspace
            | ProjectConfigRow::ZephyrProjects
            | ProjectConfigRow::ZephyrSdk => {
                let user = crate::settings::load_user(&self.config_dir)?;
                let value = match row {
                    ProjectConfigRow::ZephyrWorkspace => user.workspace,
                    ProjectConfigRow::ZephyrProjects => user.projects,
                    ProjectConfigRow::ZephyrSdk => user.sdk,
                    _ => unreachable!("only Zephyr path rows reach this branch"),
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
        match panel.simulator_pending() {
            Some(SimulatorChange::Prepare(plan)) => {
                lines.push("simulator variant in chiptui.toml".into());
                if let Some(name) = &plan.original_name {
                    lines.push(format!("  update {name}"));
                }
                for variant in plan.retained.iter().chain(std::iter::once(&plan.variant)) {
                    lines.push("  [[variant]]".into());
                    lines.push(format!("  name = \"{}\"", variant.name));
                    if let Some(board) = &variant.board {
                        lines.push(format!("  board = \"{board}\""));
                    }
                    if let Some(shield) = &variant.shield {
                        lines.push(format!("  shield = \"{shield}\""));
                    }
                    lines.push(format!("  build_dir = \"{}\"", variant.build_dir));
                }
                if !plan.retained.is_empty() {
                    lines.push("device variants retained (declarations replace discovery)".into());
                }
                lines.push("LVGL + SDL scaffold".into());
                for file in &plan.scaffold.files {
                    let action = if panel.root().join(&file.path).exists() {
                        "keep existing"
                    } else {
                        "create"
                    };
                    lines.push(format!("  {action}: {}", file.path.display()));
                }
                lines.push(crate::backend::zephyr::simulator::REQUIREMENTS.into());
                lines.push(panel.sdl_status.clone());
                lines.push(crate::backend::zephyr::simulator::GUIDANCE.into());
            }
            Some(SimulatorChange::Remove(removal)) => {
                lines.push("remove simulator variant from chiptui.toml".into());
                lines.push(format!("  name = \"{}\"", removal.name));
                lines.push("kept on disk (nothing else is touched):".into());
                for file in &removal.retained_files {
                    lines.push(format!("  {}", file.display()));
                }
                if panel.root().join(&removal.build_dir).is_dir() {
                    lines.push(format!("  {}/", removal.build_dir));
                }
                if removal.rediscovered {
                    lines.push(
                    "With no variants left declared, this target may reappear as discovered while its files remain."
                        .into(),
                );
                }
            }
            None => {}
        }
        let scaffold = self.config_scaffold();
        if !scaffold.is_empty() {
            lines.push(String::new());
            // When the workspace offers samples, the layout is a question
            // the apply itself asks next --- the review names the question
            // rather than the minimal answer it may not take.
            if self.samples_offer(panel) {
                lines.push(
                    "starting layout: your pick next — the workspace's samples, or the minimal application"
                        .to_string(),
                );
            } else {
                lines.push(format!(
                    "creates {} starting {}",
                    scaffold.len(),
                    if scaffold.len() == 1 { "file" } else { "files" }
                ));
                for file in scaffold {
                    lines.push(format!("  {file}"));
                }
            }
        }
        lines
    }

    /// Whether the apply would offer the workspace's samples: the chosen
    /// backend is Zephyr, the directory is empty and a workspace --- the
    /// one already resolved, or the one this very transaction is about to
    /// answer --- has a samples tree. `false` keeps the review on the
    /// minimal layout's file list, which is then exactly what applying
    /// writes.
    ///
    /// This runs once per frame while the review is open, so it is an
    /// existence check, never the tree walk --- a checkout that ships a
    /// `samples/` directory ships samples, and the picker does the real
    /// listing (with the real count) the moment the question opens.
    fn samples_offer(&self, panel: &ProjectConfigPanel) -> bool {
        use crate::backend::zephyr::workspace::{Resolution, ResolveInput};

        if panel.chosen() != Some(BackendKind::Zephyr)
            || !panel.backend_changed()
            || !crate::startup::is_empty_dir(panel.root())
        {
            return false;
        }
        let workspace = match self.resolve_workspace() {
            Resolution::Single(workspace) => Some(workspace),
            _ => {
                // A `Zephyr path` answered in this transaction is not on
                // disk yet; the preview still owes the user its
                // consequences, so resolve the pending answer as if
                // written. Either file would do --- resolution reads both
                // levels the same way.
                let pending = panel
                    .pending_for(ProjectConfigRow::ZephyrWorkspace)
                    .and_then(|value| value.map(str::to_string));
                pending.and_then(|pending| {
                    let settings = crate::settings::ZephyrSettings {
                        workspace: Some(pending),
                        ..crate::settings::ZephyrSettings::default()
                    };
                    let input = ResolveInput {
                        project_settings: Some(&settings),
                        user_settings: None,
                        home: &self.home_dir,
                    };
                    match crate::backend::zephyr::workspace::resolve(&input) {
                        Resolution::Single(workspace) => Some(workspace),
                        _ => None,
                    }
                })
            }
        };
        workspace.is_some_and(|workspace| workspace.zephyr_base.join("samples").is_dir())
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
