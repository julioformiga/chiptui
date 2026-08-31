//! Project-pane driving: row 1's checklist rows (the environment questions
//! that moved out of the workspace pane), the shortcuts overlay's way in
//! (`ctrl+k`, then the pane's `e` letter), and the
//! MicroPython half of the project state machine --- the projects folder
//! (`[micropython] projects`, user config) and the session-only project
//! pick that re-roots the file browser's local pane. Split out of `app.rs`
//! alongside the other one-subsystem files.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::Capability;
use crate::project::{DetectionOutcome, DetectionSource};
use crate::workspace::{DirPurpose, WorkspaceAction};

use super::{App, Overlay, ProjectRow};

impl App {
    /// The Project pane's rows: whichever questions the selected backend
    /// asks, in the order they are answered. A workspace-maintaining backend
    /// (Zephyr) asks through [`crate::workspace::WorkspacePanel::actions`];
    /// a project-selecting filesystem backend (MicroPython) asks its four
    /// own. Capability-gated, never backend-kind-gated (`AGENTS.md` §3). A
    /// backend that asks nothing gets no rows --- the pane falls back to
    /// plain detection info.
    pub fn project_rows(&self) -> Vec<ProjectRow> {
        let caps = self.manager.capabilities();
        if let Some(panel) = &self.workspace
            && caps.contains(Capability::WorkspaceSync)
        {
            return panel
                .actions(&caps)
                .into_iter()
                .map(|action| match action {
                    WorkspaceAction::Choose => ProjectRow::ZephyrPath,
                    WorkspaceAction::Projects => ProjectRow::ProjectsBase,
                    WorkspaceAction::Project => ProjectRow::ProjectPath,
                    WorkspaceAction::BoardShield => ProjectRow::BoardShield,
                })
                .collect();
        }
        if caps.contains(Capability::ProjectSelect) && caps.contains(Capability::Filesystem) {
            return vec![
                ProjectRow::MpyProjectsBase,
                ProjectRow::MpyProjectPath,
                ProjectRow::MpyDependencies,
                ProjectRow::MpyBoot,
            ];
        }
        Vec::new()
    }

    /// Whether a row's question is still open --- the state the Project
    /// pane's entry cursor lands on (the first one). An *invalid* configured answer
    /// counts as open too: it needs fixing, not celebrating.
    pub(super) fn project_row_open(&self, row: ProjectRow) -> bool {
        match row {
            ProjectRow::ZephyrPath => self
                .workspace
                .as_ref()
                .is_some_and(|panel| panel.resolved.is_none()),
            ProjectRow::ProjectsBase => self
                .workspace
                .as_ref()
                .is_some_and(|panel| panel.projects.is_none()),
            ProjectRow::ProjectPath => !self.project_gate_ok(),
            ProjectRow::BoardShield => self
                .build
                .as_ref()
                .is_some_and(|panel| panel.board.is_none()),
            ProjectRow::MpyProjectsBase => self.mpy_projects.is_none(),
            // The working directory already answers this until a pick
            // overrides it; the dependencies and boot-file rows are reports,
            // not questions.
            ProjectRow::MpyProjectPath | ProjectRow::MpyDependencies | ProjectRow::MpyBoot => false,
        }
    }

    pub(super) fn first_open_project_row(&self) -> usize {
        self.project_rows()
            .iter()
            .position(|row| self.project_row_open(*row))
            .unwrap_or(0)
    }

    /// Handles a key while the Project pane holds focus: the usual list
    /// grammar over the checklist rows, `Enter` answering the row under the
    /// cursor, and --- on the merged `Board · Shield` row --- `←`/`→`
    /// switching which half the `Enter` acts on (positional: the board is
    /// the left half, the shield the right).
    pub(super) fn on_project_key(&mut self, key: KeyEvent) {
        let rows = self.project_rows();
        let len = rows.len().max(1);
        let current = rows.get(self.project_cursor).copied();
        let mut run: Option<ProjectRow> = None;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.project_cursor = self.project_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.project_cursor = (self.project_cursor + 1).min(len - 1);
            }
            KeyCode::PageUp => self.project_cursor = self.project_cursor.saturating_sub(5),
            KeyCode::PageDown => self.project_cursor = (self.project_cursor + 5).min(len - 1),
            KeyCode::Home => self.project_cursor = 0,
            KeyCode::End => self.project_cursor = len - 1,
            KeyCode::Left if current == Some(ProjectRow::BoardShield) => self.board_segment = true,
            KeyCode::Right if current == Some(ProjectRow::BoardShield) => {
                self.board_segment = false;
            }
            // The Dependencies row's other door: search the micropython-lib
            // index without first committing to installing everything the
            // file lists.
            KeyCode::Char('s') if current == Some(ProjectRow::MpyDependencies) => {
                self.open_package_manager();
                return;
            }
            KeyCode::Enter => run = current,
            _ => {}
        }
        if let Some(row) = run {
            self.run_project_row(row);
        }
    }

    /// Answers one checklist row: the Zephyr rows run through the workspace
    /// actions they always did, the MicroPython questions open their own
    /// flows, the dependencies row runs its `mip install`, and the boot-file
    /// row is a report nothing answers.
    fn run_project_row(&mut self, row: ProjectRow) {
        let action = match row {
            ProjectRow::ZephyrPath => Some(WorkspaceAction::Choose),
            ProjectRow::ProjectsBase => Some(WorkspaceAction::Projects),
            ProjectRow::ProjectPath => Some(WorkspaceAction::Project),
            ProjectRow::BoardShield => Some(WorkspaceAction::BoardShield),
            ProjectRow::MpyProjectsBase => {
                self.open_mpy_projects_dir_picker();
                return;
            }
            ProjectRow::MpyProjectPath => {
                self.open_mpy_project_flow();
                return;
            }
            ProjectRow::MpyDependencies => {
                self.open_dependencies();
                return;
            }
            ProjectRow::MpyBoot => return,
        };
        if let Some(action) = action {
            self.run_workspace_action(action);
        }
    }

    // ---- MicroPython projects -------------------------------------------

    /// Resolves `[micropython] projects` (user config only --- a
    /// MicroPython project pins no environment of its own) once per
    /// session. The pickers refresh the answer afterwards; this is only the
    /// startup read.
    pub(super) fn ensure_mpy_projects(&mut self) {
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::ProjectSelect)
            || !caps.contains(Capability::Filesystem)
            || self.mpy_projects_loaded
        {
            return;
        }
        self.mpy_projects_loaded = true;
        if let Some(raw) = crate::settings::mpy_projects_raw(&self.config_dir) {
            let dir = crate::settings::expand_home(&raw, &self.home_dir);
            if dir.is_dir() {
                self.mpy_projects = Some(dir);
            } else {
                let message = format!(
                    "{} does not exist (or is not a directory) — fix [micropython] projects or choose again",
                    dir.display()
                );
                self.logs.error(message.clone());
                self.mpy_projects_invalid = Some(message);
            }
        }
    }

    /// The projects-folder flavor of the directory picker for MicroPython:
    /// same navigation, existence-only validation, persisted to
    /// `[micropython] projects`.
    pub(super) fn open_mpy_projects_dir_picker(&mut self) {
        self.open_purpose_picker(DirPurpose::MpyProjects);
    }

    /// Validates the folder the picker accepted (it only has to exist),
    /// persists it to the user config, and moves straight to the project
    /// picker --- the folder alone browses nothing; the specific project
    /// inside it is the other half of the answer.
    pub(super) fn accept_mpy_projects_dir(&mut self, dir: PathBuf) {
        if !dir.is_dir() {
            self.overlay = Some(Overlay::DirPicker {
                purpose: DirPurpose::MpyProjects,
                path: dir,
                selected: 0,
                error: Some("the folder vanished — it existed when it was accepted".to_string()),
            });
            return;
        }
        let target = self.user_config_path();
        match crate::settings::save_mpy_projects(&target, &dir) {
            Ok(()) => {
                self.logs
                    .info(format!("projects folder saved to {}", target.display()));
            }
            Err(err) => self
                .logs
                .error(format!("could not save {}: {err}", target.display())),
        }
        self.overlay = None;
        self.mpy_projects = Some(dir);
        self.mpy_projects_invalid = None;
        self.open_mpy_project_picker();
    }

    /// Opens whichever picker the MicroPython project question needs next:
    /// the projects folder when none is configured, the project list when
    /// one is.
    pub(super) fn open_mpy_project_flow(&mut self) {
        if self.mpy_projects.is_some() {
            self.open_mpy_project_picker();
        } else {
            self.logs
                .warn("no projects folder configured — where do your MicroPython projects live?");
            self.open_mpy_projects_dir_picker();
        }
    }

    pub(super) fn open_mpy_project_picker(&mut self) {
        self.overlay = Some(Overlay::ProjectPicker {
            mpy: true,
            selected: 0,
            error: None,
        });
    }

    /// Applies the MicroPython project chosen in the picker: session-only,
    /// re-rooting the browser's local pane. Every subdirectory is a
    /// project (no build step to gate on), so nothing is refused.
    pub(super) fn apply_mpy_project_picker(&mut self, selected: usize) {
        let Some(dir) = self.mpy_projects.clone() else {
            self.open_mpy_project_flow();
            return;
        };
        let (rows, read_error) = crate::backend::micropython::projects::project_rows(&dir);
        let Some(row) = rows.get(selected) else {
            let reason = read_error.unwrap_or_else(|| "nothing to pick".to_string());
            self.overlay = Some(Overlay::ProjectPicker {
                mpy: true,
                selected,
                error: Some(reason),
            });
            return;
        };
        self.set_mpy_project(row.path.clone());
        self.logs.info(format!(
            "project set to {} for this session (nothing written)",
            row.path.display()
        ));
        self.overlay = None;
    }

    /// Re-roots the session's MicroPython project: the browser's local pane
    /// follows (both the path and the root its title names), the same
    /// one-fact-two-views rule the Zephyr panes' re-rooting follows.
    pub(super) fn set_mpy_project(&mut self, dir: PathBuf) {
        self.mpy_root = Some(dir.clone());
        if let Some(browser) = &mut self.browser {
            browser.set_local_root(dir);
        }
        // The declarations belong to the project, so they are re-read now
        // rather than up to a tick later.
        self.reload_requirements();
    }

    /// The MicroPython project root in effect: the session's pick, or the
    /// detection root when none was made. The Dependencies row reads this
    /// --- `requirements.txt` lives at the project root, which the scaffold
    /// is what puts it there.
    pub fn mpy_effective_root(&self) -> PathBuf {
        self.mpy_root.clone().unwrap_or_else(|| {
            self.manager.root().map_or_else(
                || self.manager.start_dir().to_path_buf(),
                |root| root.to_path_buf(),
            )
        })
    }

    /// The directory the *device's own root* is compared against: the
    /// project's `src/` when it has one, else the project root
    /// ([`crate::files::sync_root`], the same rule the Files pane opens on).
    ///
    /// Deliberately not [`Self::mpy_effective_root`]: the scaffold writes
    /// `boot.py`/`main.py` into `src/` and `requirements.txt` into the root,
    /// so the two report rows ask about two different directories. Reading
    /// the root for both is what pinned the Boot files row at `⚠`.
    pub fn mpy_sync_root(&self) -> PathBuf {
        crate::files::sync_root(&self.mpy_effective_root())
    }

    /// The board's MicroPython version, from whatever REPL/monitor lines
    /// were just seen --- the probe before the first listing, the monitor
    /// afterwards. A banner that says nothing changes nothing.
    pub(super) fn refresh_mpy_version(&mut self, lines: &[String]) {
        if let Some(version) = crate::device::micropython_version(lines)
            && self.mpy_version.as_deref() != Some(version.as_str())
        {
            self.mpy_version = Some(version);
        }
    }

    /// The Dependencies row's `Enter`: opens the package manager, always.
    ///
    /// It used to install the whole file outright, and only open the search
    /// when no file existed --- so the one gesture on the row did two
    /// unrelated things depending on state, and there was no way at all to
    /// *look* at what was declared. Installing everything is now a row
    /// inside the manager ([`Self::install_project_dependencies`]).
    ///
    /// A missing file is created from the shared template first, so the
    /// manager always opens onto something. A `manifest.py`-only project is
    /// refused with the reason --- manifests are a firmware-build format,
    /// not something mip can read.
    pub(super) fn open_dependencies(&mut self) {
        let root = self.mpy_effective_root();
        let requirements = root.join("requirements.txt");
        if !requirements.is_file() {
            if root.join("manifest.py").is_file() {
                self.logs.warn(
                    "mip installs from requirements.txt — manifest.py is a firmware-build format",
                );
                return;
            }
            if self.create_requirements_file().is_none() {
                return;
            }
        }
        self.open_package_manager();
    }

    /// Installs every specification `requirements.txt` declares, in one
    /// `mip install` (mpremote 1.28 has no `-r`; mip itself skips files
    /// that are already installed). The manager's `Install all` row.
    pub(super) fn install_project_dependencies(&mut self) {
        let requirements = self.requirements_path();
        let Ok(text) = std::fs::read_to_string(&requirements) else {
            self.logs
                .error(format!("{}: could not read it", requirements.display()));
            return;
        };
        let specs = crate::backend::micropython::deps::parse_requirements(&text);
        if specs.is_empty() {
            self.logs
                .warn("requirements.txt lists no packages — nothing to install");
            return;
        }
        self.dispatch_browser(|browser, processes, port| {
            browser.request_mip_install(&specs, processes, port)
        });
    }
}

impl App {
    /// Runs the first detection and reports it to the log pane.
    ///
    /// A detection failure is surfaced, not fatal: the UI still starts so the
    /// user can read the error and override the backend.
    pub fn bootstrap(&mut self) {
        self.logs.info(format!(
            "working directory {}",
            self.manager.start_dir().display()
        ));
        self.detect();
        // Seeded here so the very first frame's Dependencies row is right:
        // the tick that keeps it fresh has not fired yet.
        self.reload_requirements();
    }

    /// Re-runs detection from the starting directory.
    pub fn detect(&mut self) {
        match self.manager.detect() {
            Ok(detection) => {
                let root = detection.root.display().to_string();
                let searched = detection.searched.len();
                let outcome = detection.outcome.clone();
                let source = detection.source;
                let confidence = detection.confidence();

                match &outcome {
                    DetectionOutcome::Detected(kind) => {
                        let confidence = confidence.unwrap_or(0.0);
                        if source == DetectionSource::Manual {
                            self.logs
                                .success(format!("{kind} selected manually at {root}"));
                        } else {
                            self.logs.success(format!(
                                "{kind} detected at {root} (confidence {confidence:.2})"
                            ));
                        }
                    }
                    DetectionOutcome::Ambiguous(kinds) => {
                        let names = kinds
                            .iter()
                            .map(|kind| kind.display_name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.logs
                            .warn(format!("ambiguous project at {root}: {names}"));
                    }
                    DetectionOutcome::Unknown => {
                        self.logs.warn(format!(
                            "no known project found in {searched} director{} from {root}",
                            if searched == 1 { "y" } else { "ies" }
                        ));
                    }
                }

                self.ensure_workspace_panel();
                self.report_tools();
            }
            Err(err) => self.logs.error(err.to_string()),
        }
        self.clamp_focus();
    }

    /// Opens [`Overlay::ProjectSetup`] when detection has nothing to go on:
    /// `Unknown` or `Ambiguous`, with no session override and no scaffold
    /// file already deciding it (`SPEC.md` §7's empty-project prompt).
    ///
    /// Deliberately **not** called from inside [`App::detect`] itself.
    /// `detect()`/`bootstrap()` are called directly by many existing tests
    /// that assert on `overlay`/send key events right afterwards (every
    /// `tests/flash_view.rs` case via its `app_with_flash` helper, which
    /// starts from a bare temp directory). Auto-opening a modal from inside
    /// `detect()` would silently redirect their next key press into
    /// `on_overlay_key`. Instead only the two real "detection just ran and
    /// the user might act on it" call sites opt in explicitly: the binary's
    /// startup sequence, and the `r` re-detect key.
    pub fn maybe_open_project_setup(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(detection) = self.manager.detection() else {
            return;
        };
        if self.manager.override_kind().is_some()
            || matches!(
                detection.source,
                DetectionSource::Config | DetectionSource::Registered
            )
        {
            return;
        }
        if matches!(
            detection.outcome,
            DetectionOutcome::Unknown | DetectionOutcome::Ambiguous(_)
        ) {
            self.overlay = Some(Overlay::ProjectSetup { selected: 0 });
        }
    }

    /// Records the open project in the user config's registry (`SPEC.md`
    /// §7), stamping it as the most recently opened.
    ///
    /// This is the single place a project becomes "known": every way of
    /// arriving at a dashboard --- a `chiptui.toml` in the tree, evidence
    /// alone, the empty-project prompt, a project just created on the home
    /// screen --- passes through here, which is what keeps the home screen's
    /// list complete without any of them writing into the project directory.
    /// A directory whose backend is still unknown is not recorded; there
    /// would be nothing to record about it.
    pub fn record_open_project(&mut self) {
        let (Some(kind), Some(root)) = (self.manager.selected_kind(), self.manager.root()) else {
            return;
        };
        let root = root.to_path_buf();
        let mut entry = crate::settings::ProjectEntry::new(&root, kind).opened_now();
        // A name the user edited into the config is theirs, not ours to
        // reset on every open --- and so are the saved board/shield answers:
        // recording the open must not be what forgets them.
        if let Some(known) = self.manager.known_projects().entry_for(&root) {
            entry.name = known.name.clone();
            entry.board = known.board.clone();
            entry.shield = known.shield.clone();
        }

        let config = self.user_config_path();
        if let Err(err) = crate::settings::record_project(&config, entry) {
            self.logs.warn(format!(
                "could not record this project in {}: {err}",
                config.display()
            ));
            return;
        }
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
    }

    /// `P`: back to the home screen to open another project. Leaving means
    /// dropping this project's session, so anything still running is named
    /// in a confirmation first (`SPEC.md` §15's rule applied to losing work
    /// rather than to the device) --- with nothing running there is nothing
    /// to warn about, and the screen simply changes.
    pub fn request_home_screen(&mut self) {
        if self.running_commands() == 0 {
            self.request_project_switch();
            return;
        }
        self.overlay = Some(Overlay::ConfirmSwitchProject { confirm: false });
    }

    /// `q` / `ctrl+c`: end the session. Quitting drops the whole
    /// `ProcessManager`, so it loses strictly more than
    /// [`Self::request_home_screen`] does --- and that one always asked.
    /// The same rule applies here: with nothing running the session simply
    /// ends, otherwise [`Overlay::ConfirmQuit`] names what dies first
    /// (`SPEC.md` §15 applied to losing work). `ctrl+c` pressed again over
    /// the dialog quits outright (`App::on_key`), so raw mode's escape
    /// hatch survives the question.
    pub fn request_quit(&mut self) {
        if self.running_commands() == 0 {
            self.quit();
            return;
        }
        self.overlay = Some(Overlay::ConfirmQuit { confirm: false });
    }

    /// External commands this session would lose by leaving it --- builds,
    /// device operations and the PTY sessions alike, since they all run
    /// through the one [`crate::process::ProcessManager`].
    pub fn running_commands(&self) -> usize {
        self.processes.running_count()
    }

    /// Asks the binary to close this project and go back to the home screen.
    /// The App is dropped by the caller, which is what cancels every running
    /// command and releases the serial port --- see
    /// [`crate::process::ProcessManager`]'s `Drop`.
    pub fn request_project_switch(&mut self) {
        self.switch_requested = true;
    }

    /// Whether the event loop should give the terminal back so the home
    /// screen can take over --- the same standing as [`Self::should_quit`].
    pub fn switch_requested(&self) -> bool {
        self.switch_requested
    }

    /// Consumed by the binary's loop, like [`Self::take_pending_command`].
    pub fn take_switch_request(&mut self) -> bool {
        std::mem::take(&mut self.switch_requested)
    }

    /// The header's `project` field. A backend that makes the project a
    /// question ([`Capability::ProjectSelect`]) answers it with the picked
    /// root --- the build panel's for Zephyr (and only once that root is a
    /// buildable application: picked in the panel, or a launch directory
    /// that already is one), the session's MicroPython pick otherwise.
    /// Until then the field stays empty, because the cwd is not a project
    /// just because ChipTUI was started in it. Other backends keep the
    /// detection root's directory name as before.
    pub fn header_project(&self) -> String {
        if self
            .manager
            .capabilities()
            .contains(Capability::ProjectSelect)
        {
            if let Some(panel) = &self.build {
                return if self.project_gate_ok() {
                    panel
                        .root
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
                } else {
                    String::new()
                };
            }
            if let Some(picked) = &self.mpy_root {
                return picked
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            }
        }
        self.manager.name().unwrap_or("--").to_string()
    }

    /// The project half of the panel's checklist: whether the current root
    /// is a buildable application. Backends without
    /// [`Capability::ProjectSelect`] have no such question --- the root is
    /// theirs by definition.
    pub fn project_gate_ok(&self) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        !self
            .manager
            .capabilities()
            .contains(Capability::ProjectSelect)
            || crate::backend::zephyr::projects::is_buildable(&panel.root)
    }
}
