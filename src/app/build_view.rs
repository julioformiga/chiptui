//! Build-panel driving: the action list's key handling, spawning commands,
//! and the confirm gate in front of [`crate::backend::Capability::Clean`]
//! (`SPEC.md` §15 --- destructive actions ask first, showing the literal
//! command). Split out of `app.rs` alongside the other one-subsystem files.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BuildKind;
use crate::build::{BuildAction, BuildPanel};

use super::{App, DocsFocus, Focus, LogTab, MonitorSource, Overlay};

impl App {
    /// Handles a key while [`Focus::Build`] holds focus. The list is
    /// navigated like every other list here (`j`/`k`, arrows, page, home/
    /// end); `Enter` runs the action under the cursor --- `Stop` while a
    /// command is running, otherwise the build lifecycle entry.
    pub(super) fn on_build_key(&mut self, key: KeyEvent) {
        // Two phases: cursor movement borrows only the panel, `Enter` needs
        // the whole app (starting a command flips focus and the monitor
        // source), so the decision is computed first and acted on after the
        // panel borrow ends.
        let caps = self.manager.capabilities();
        let mut action = None;
        if let Some(panel) = self.build.as_mut() {
            let len = panel.actions(&caps).len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    panel.cursor = panel.cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    panel.cursor = (panel.cursor + 1).min(len - 1);
                }
                KeyCode::PageUp => panel.cursor = panel.cursor.saturating_sub(5),
                KeyCode::PageDown => panel.cursor = (panel.cursor + 5).min(len - 1),
                KeyCode::Home => panel.cursor = 0,
                KeyCode::End => panel.cursor = len - 1,
                KeyCode::Enter => action = panel.action_at(&caps, panel.cursor),
                _ => {}
            }
        }
        if let Some(action) = action {
            self.run_build_action(action);
        }
    }

    /// Opens the board picker, kicking off the background `west boards`
    /// fetch on first open (the list is slow to produce and useless until
    /// asked for), plus the docs index that enriches it.
    pub fn open_board_picker(&mut self) {
        self.overlay = Some(Overlay::BoardPicker {
            input: String::new(),
            selected: 0,
            scroll: 0,
            focus: DocsFocus::default(),
        });
        self.docs_list_offset = 0;
        self.docs.ensure_index(&self.docs_label());
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if let Some(command) = panel.boards_command(backend) {
            let label = command.to_string();
            panel.start_boards_fetch(command, &mut self.processes);
            self.logs.info(format!("fetching the board list ({label})"));
        }
    }

    /// The docs release the pickers enrich their lists from: the resolved
    /// workspace's own version when it names one, `latest` otherwise --- the
    /// board/shield *list* stays `west boards` (what this tree can build);
    /// only the enrichment is online, and it should match the tree.
    pub fn docs_label(&self) -> String {
        self.workspace
            .as_ref()
            .and_then(|panel| panel.zephyr_version())
            .unwrap_or_else(|| crate::board_docs::LATEST.to_string())
    }

    /// The tick's watch over an open picker: the selected row's docs id
    /// arms (and then fires) the debounced picture/details fetch. Runs every
    /// tick so a late-arriving index still enriches the row already under
    /// the cursor without a special case.
    pub(crate) fn drive_docs_selection(&mut self) {
        let id = self.picker_selection_doc_id();
        let ticks = self.ticks;
        self.docs.note_selection(id.as_deref(), ticks);
        self.docs.drive(ticks);
    }

    /// The docs entry id the open picker's cursor rests on: a board's
    /// qualified west name reduced to its docs id, a shield's name as-is,
    /// `None` without a picker (or on the shield picker's `(none)` row).
    fn picker_selection_doc_id(&self) -> Option<String> {
        match &self.overlay {
            Some(Overlay::BoardPicker {
                input, selected, ..
            }) => {
                let panel = self.build.as_ref()?;
                let boards = panel.filtered_boards(input);
                let board = boards.get(*selected)?;
                Some(crate::board_docs::board_doc_id(&board.name).to_string())
            }
            Some(Overlay::ShieldPicker {
                input, selected, ..
            }) => {
                if *selected == 0 {
                    return None;
                }
                let panel = self.build.as_ref()?;
                let shields = panel.filtered_shields(input);
                let shield = shields.get(*selected - 1)?;
                Some(shield.name.clone())
            }
            _ => None,
        }
    }

    /// Applies the board chosen in the picker and persists it in the
    /// project's registry entry (`SPEC.md` §13): the answer reloads on
    /// every later open, outranking the build cache. Nothing lands in the
    /// project directory --- the registry is the one place a session
    /// answer outlives the session.
    pub(super) fn apply_board_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        let filtered = panel.filtered_boards(filter);
        let Some(name) = filtered.get(selected).map(|board| board.name.clone()) else {
            return;
        };
        panel.set_picked(name.clone());
        self.logs.info(format!("board set to {name}"));
        self.persist_target();
    }

    /// Writes the panel's current target answers --- board, shield and the
    /// selected variant's name --- into the registry entry for the project
    /// the panel is rooted at (creating the entry when the project is not
    /// recorded yet --- opening it would do the same). Everything else
    /// already recorded --- backend, name, last-opened stamp --- survives
    /// untouched.
    fn persist_target(&mut self) {
        let Some(panel) = &self.build else {
            return;
        };
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let root = panel.root.clone();
        let mut entry = match self.manager.known_projects().entry_for(&root) {
            Some(known) => known.clone(),
            None => crate::settings::ProjectEntry::new(&root, kind),
        };
        entry.board = panel.board_name().map(str::to_string);
        entry.shield = panel.shield_name().map(str::to_string);
        // The *answer*, not the live target: a session starts on the board
        // whatever was built last, so recording the target here would
        // forget the answer the moment the project reopened.
        entry.variant = panel
            .variant_index_for(panel.remembered_simulator)
            .and_then(|index| panel.variants.get(index))
            .map(|variant| variant.name.clone());
        let config = self.user_config_path();
        match crate::settings::record_project(&config, entry) {
            Ok(()) => self
                .logs
                .info(format!("target answer saved to {}", config.display())),
            Err(err) => self.logs.warn(format!(
                "could not save the target answer in {}: {err}",
                config.display()
            )),
        }
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
    }

    /// Opens the build question: on the board, or on the host simulator?
    ///
    /// Only when the project has both ([`BuildPanel::offers_build_choice`])
    /// --- with a single target there is nothing to ask and `kind` starts
    /// outright. The cursor opens on the *last* answer, so repeating a
    /// target is one `Enter` and changing it is one arrow: the question is
    /// asked every time (nothing on the pane says where the next build
    /// goes, so remembering silently would hide it), but it never costs
    /// more than a keypress.
    pub fn ask_build_target(&mut self, kind: BuildKind) {
        self.overlay = Some(Overlay::BuildTarget {
            kind,
            selected: usize::from(
                self.build
                    .as_ref()
                    .is_some_and(|panel| panel.remembered_simulator),
            ),
        });
    }

    /// Applies the build question's answer and starts the command: the
    /// chosen variant becomes the panel's target --- its board, its shield
    /// and its build directory together --- and `kind` runs against it.
    ///
    /// That target then outlives the command: `Clean`, `Menuconfig` and the
    /// dashboard follow the last build, because that is the artifact the
    /// user was just looking at. Only `Flash` is pinned to the board
    /// ([`BuildPanel::flash_build_dir`]).
    pub(super) fn apply_build_target(&mut self, kind: BuildKind, selected: usize) {
        let simulator = selected == 1;
        if let Some(panel) = &mut self.build {
            panel.remembered_simulator = simulator;
            if let Some(index) = panel.variant_index_for(simulator) {
                panel.select_variant(index);
            }
            self.persist_target();
        }
        self.start_build(kind);
    }

    /// Opens the shield picker, kicking off the background `west shields`
    /// fetch on first open, like the board picker does for `west boards`.
    pub(super) fn open_shield_picker(&mut self) {
        self.overlay = Some(Overlay::ShieldPicker {
            input: String::new(),
            selected: 0,
            scroll: 0,
            focus: DocsFocus::default(),
        });
        self.docs_list_offset = 0;
        self.docs.ensure_index(&self.docs_label());
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if let Some(command) = panel.shields_command(backend) {
            let label = command.to_string();
            panel.start_shields_fetch(command, &mut self.processes);
            self.logs
                .info(format!("fetching the shield list ({label})"));
        }
    }

    /// Applies the shield picker's answer and persists it beside the board:
    /// row 0 --- the `(none)` row --- clears it (the shield is optional, so
    /// no pick is a valid answer, unlike the board).
    pub(super) fn apply_shield_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        if selected == 0 {
            panel.set_shield(None);
            self.logs.info("shield cleared");
            self.persist_target();
            return;
        }
        let filtered = panel.filtered_shields(filter);
        let Some(name) = filtered.get(selected - 1).map(|shield| shield.name.clone()) else {
            return;
        };
        panel.set_shield(Some(name.clone()));
        self.logs.info(format!("shield set to {name}"));
        self.persist_target();
    }

    /// Runs a panel action: destructive ones (`Clean`, `Flash`, and
    /// `UpdateZephyr` --- it rewrites the shared workspace, `SPEC.md` §15)
    /// route through a confirm overlay quoting the literal command;
    /// `menuconfig` hands the terminal to the child; the rest act
    /// immediately. Disabled rows (the lifecycle before the checklist is
    /// complete, or `UpdateZephyr` before a workspace is resolved ---
    /// [`Self::build_action_enabled`]) do nothing: the dimmed row is the
    /// explanation. This is also the confirm overlay's accept path, which
    /// is why it must not itself confirm again. Every action that runs a
    /// *project* command passes through
    /// [`Self::require_buildable_project`] first --- no command runs in a
    /// directory that is not a buildable application; `UpdateZephyr` skips
    /// that gate, since it runs in the workspace, not the project.
    pub(super) fn run_build_action(&mut self, action: BuildAction) {
        if !self.build_action_enabled(action) {
            // Two reasons a row is dimmed, and only one of them explains
            // itself. The checklist gate does: the Environment pane is
            // showing the unanswered question, so silence is the right
            // answer and the dimmed row is the whole message. The *busy*
            // gate does not --- it arrived with the one-process-slot rule
            // and dims every row under a live `Stop`, so a user pressing
            // Enter on a familiar button gets nothing at all, and the
            // warning that used to say why is now unreachable from here
            // (`start_build_command`'s guard, which the gate short-circuits).
            if action != BuildAction::Stop
                && self
                    .build
                    .as_ref()
                    .is_some_and(crate::build::BuildPanel::is_busy)
            {
                self.logs
                    .warn("a build command is already running — stop it first");
            }
            return;
        }
        if matches!(
            action,
            BuildAction::Build(_)
                | BuildAction::Flash
                | BuildAction::Menuconfig
                | BuildAction::Dashboard
                | BuildAction::SizeReport
        ) && !self.require_buildable_project(&action)
        {
            return;
        }
        match action {
            BuildAction::Stop => self.stop_build(),
            BuildAction::Build(kind) => {
                if kind == BuildKind::Clean {
                    self.overlay = Some(Overlay::ConfirmBuild {
                        action,
                        confirm: false,
                    });
                } else if self
                    .build
                    .as_ref()
                    .is_some_and(BuildPanel::offers_build_choice)
                {
                    // The project keeps a host target beside the board, so
                    // where this build runs is a question, not a default.
                    self.ask_build_target(kind);
                } else {
                    self.start_build(kind);
                }
            }
            BuildAction::Flash => {
                self.overlay = Some(Overlay::ConfirmBuild {
                    action,
                    confirm: false,
                });
            }
            BuildAction::Menuconfig => self.start_menuconfig(),
            BuildAction::UpdateZephyr => {
                self.overlay = Some(Overlay::ZephyrActions { selected: 0 });
            }
            // Nothing runs from this row: it asks where to install, and
            // the installer's own confirm asks whether to.
            BuildAction::InstallZephyr => self.open_install_picker(),
            BuildAction::Dashboard => self.start_dashboard(),
            BuildAction::SizeReport => self.start_size_report(),
            // Rowless, and not started by a press either: a finished
            // simulator build launches it ([`App::start_run`]).
            BuildAction::Run => self.start_run(),
        }
    }

    /// The project gate (`Capability::ProjectSelect`): every command that
    /// runs *in the project* (build, clean, rebuild, flash, menuconfig)
    /// needs a working directory with build elements. A root that has them
    /// --- the directory ChipTUI was started in, when that is a project ---
    /// passes without ceremony; one that does not is refused, never
    /// silently built around: the refusal explains itself and opens the
    /// flow that answers it (projects folder, then the project itself).
    fn require_buildable_project(&mut self, action: &BuildAction) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        if !self
            .manager
            .capabilities()
            .contains(crate::backend::Capability::ProjectSelect)
            || crate::backend::zephyr::projects::is_buildable(&panel.root)
        {
            return true;
        }
        let what = match action {
            BuildAction::Build(kind) => kind.label(),
            BuildAction::Flash => "flash",
            BuildAction::Menuconfig => "menuconfig",
            BuildAction::Dashboard => "dashboard",
            BuildAction::SizeReport => "memory report",
            _ => "this command",
        };
        self.logs.warn(format!(
            "{what}: {} is not a Zephyr application (no CMakeLists.txt) — pick a project first",
            panel.root.display()
        ));
        self.open_project_flow();
        false
    }

    /// Re-derives the extra board search roots for the build panel's
    /// current project and pushes them in.
    ///
    /// The walk stops at the configured projects folder when there is one:
    /// a module *above* the projects base is somebody else's, and climbing
    /// to `$HOME` looking for one would be a search, not a resolution.
    pub fn refresh_board_roots(&mut self) {
        let Some(panel) = &self.build else {
            return;
        };
        let stop_at = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.projects.clone());
        let roots = crate::backend::zephyr::variants::board_roots(&panel.root, stop_at.as_deref());
        if let Some(panel) = &mut self.build {
            panel.set_board_roots(roots);
        }
    }

    /// Re-derives the build panel's variant list for its current project.
    ///
    /// Called whenever one of its two inputs can have changed: the project
    /// (a switch, a panel just created) and the board catalogue (the
    /// `west boards` fetch landing). The catalogue is what turns a
    /// `boards/<stem>.conf` into a real target, so a project that has never
    /// been built shows its variants only once the list has been fetched
    /// --- which is when the user opens the board picker. A project with
    /// build directories answers immediately from their CMake caches, needing
    /// no catalogue at all, and that is the case in the field: `west boards`
    /// walks every board root in the workspace, so running it eagerly on
    /// every project open would spend seconds of CPU to sharpen a list that
    /// is usually already right.
    pub fn refresh_variants(&mut self) {
        let Some(panel) = &self.build else {
            return;
        };
        let declared = std::fs::read_to_string(panel.root.join(crate::project::config::FILE_NAME))
            .map(|text| crate::project::config::parse_variants(&text))
            .unwrap_or_default();
        let catalogue: Vec<String> = match &panel.boards.state {
            crate::build::ListState::Loaded(boards) => {
                boards.iter().map(|board| board.name.clone()).collect()
            }
            _ => Vec::new(),
        };
        let variants =
            crate::backend::zephyr::variants::variants(&panel.root, &declared, &catalogue);
        let saved = self
            .manager
            .known_projects()
            .entry_for(&panel.root)
            .and_then(|entry| entry.variant.clone());
        let Some(panel) = &mut self.build else {
            return;
        };
        panel.set_variants(variants);
        // The registry's answer seeds the *question's cursor*, not the
        // target: the session starts on the board (`set_variants` lands
        // there), so a `Clean` pressed before any build cannot erase a
        // directory this session never mentioned. Only a name the project
        // still has counts.
        if let Some(name) = saved {
            panel.remembered_simulator = panel
                .variants
                .iter()
                .find(|v| v.name == name)
                .is_some_and(|v| v.is_simulator());
        }
    }

    /// Opens whichever picker the project question needs next: the projects
    /// folder when none is configured, the project list when one is.
    pub(super) fn open_project_flow(&mut self) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|panel| panel.projects.is_some())
        {
            self.open_project_picker();
        } else {
            self.logs
                .warn("no projects folder configured — where do your Zephyr applications live?");
            self.open_projects_dir_picker();
        }
    }

    /// Opens the project picker over the configured projects folder. The
    /// rows (and each one's build-element mark) are read at draw time like
    /// every other overlay's derived state.
    pub(super) fn open_project_picker(&mut self) {
        self.overlay = Some(Overlay::ProjectPicker {
            mpy: false,
            selected: 0,
            error: None,
        });
    }

    /// Applies the project chosen in the picker: session-only, re-rooting
    /// every build command (`west` runs there; nothing is written --- the
    /// folder is the persisted half of the answer, the project is not).
    /// Accepting a directory without build elements keeps the picker open
    /// with the reason: the verification is the point.
    pub(super) fn apply_project_picker(&mut self, selected: usize) {
        let Some(dir) = self
            .workspace
            .as_ref()
            .and_then(|panel| panel.projects.clone())
        else {
            self.open_project_flow();
            return;
        };
        let (rows, read_error) = crate::backend::zephyr::projects::project_rows(&dir);
        let Some(row) = rows.get(selected) else {
            let reason = read_error.unwrap_or_else(|| "nothing to pick".to_string());
            self.overlay = Some(Overlay::ProjectPicker {
                mpy: false,
                selected,
                error: Some(reason),
            });
            return;
        };
        if !row.buildable {
            self.overlay = Some(Overlay::ProjectPicker {
                mpy: false,
                selected,
                error: Some(format!(
                    "{} has no CMakeLists.txt — west build cannot run there",
                    row.name
                )),
            });
            return;
        }
        if self.build.is_none() {
            return;
        }
        self.set_project_root(row.path.clone());
        self.logs.info(format!(
            "project set to {} for this session (nothing written)",
            row.path.display()
        ));
        self.overlay = None;
    }

    /// Re-roots the build lifecycle and the workspace pane's embedded file
    /// list to `dir` together: picking a project answers both "what does
    /// `west` build" and "what files does the Workspace pane show" at once,
    /// and letting them diverge would leave the file list showing a
    /// directory the build panel no longer targets. The picked project's
    /// own registry answers (board, shield) are re-applied after the
    /// re-root: a saved answer belongs to the project, not the session.
    pub(super) fn set_project_root(&mut self, dir: PathBuf) {
        if let Some(panel) = &mut self.build {
            panel.set_project(dir.clone());
        }
        // The board roots belong to the *project*, so they are re-derived
        // with it: a switch from a plain application to one carrying its
        // own board module changes what `west boards` can even see.
        self.refresh_board_roots();
        self.refresh_variants();
        if let Some(entry) = self.manager.known_projects().entry_for(&dir) {
            let board = entry.board.clone();
            let shield = entry.shield.clone();
            if let Some(panel) = &mut self.build {
                if let Some(board) = board {
                    panel.set_config_board(board);
                }
                if let Some(shield) = shield {
                    panel.set_shield(Some(shield));
                }
            }
        }
        if let Some(workspace) = &mut self.workspace {
            workspace.set_files_root(dir);
        }
    }

    /// Starts `kind`'s command and shows its output streaming in the
    /// Monitor tab --- without pulling focus off the panel: the lifecycle's
    /// next step is right here (Stop while it runs, then Flash on success).
    /// A failure to even compose or start the command is a log notice
    /// instead: the panel stays usable.
    pub(super) fn start_build(&mut self, kind: BuildKind) {
        let updates_board = matches!(kind, BuildKind::Build | BuildKind::Rebuild);
        let caps = self.manager.capabilities();
        self.start_build_command(
            kind.label(),
            updates_board,
            BuildAction::Build(kind),
            Focus::Build,
            |panel, backend| panel.command(kind, backend),
        );
        // Clean parks the cursor on Build: the build is the step a clean
        // exists to clear the way for (a build/rebuild already sits on
        // Stop, the row `start` lands on).
        if kind == BuildKind::Clean
            && let Some(panel) = self.build.as_mut()
        {
            panel.focus_action(&caps, BuildAction::Build(BuildKind::Build));
        }
    }

    /// Starts the flash command, same hand-off as the build kinds except
    /// that focus follows the output (the device is the thing changing).
    /// Reached only through the confirm overlay (flash is destructive).
    pub(super) fn start_flash(&mut self) {
        self.start_build_command(
            "Flash",
            false,
            BuildAction::Flash,
            Focus::Logs,
            |panel, backend| panel.flash_command(backend),
        );
    }

    /// Hands the terminal to `west build -t menuconfig` (`SPEC.md` §11's
    /// terminal-native rule: the Kconfig editor *is* a full-screen TUI, and
    /// nesting it in a pane would break both). The command is parked for
    /// the event loop, which owns the terminal guard needed to suspend the
    /// alternate screen --- the same hand-off as `$EDITOR`.
    pub(super) fn start_menuconfig(&mut self) {
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &self.build else {
            return;
        };
        let Some(command) = panel.menuconfig_command(backend) else {
            self.logs
                .warn("menuconfig: this backend offers no such action");
            return;
        };
        self.logs
            .info(format!("running {command} (the TUI is suspended)"));
        self.pending_command = Some(command);
    }

    /// Starts the build dashboard (`west build -t dashboard`, reached
    /// through the Zephyr Actions menu): the build pane's own process slot,
    /// output streaming in the Monitor tab, focus staying on the panel with
    /// the cursor on `Stop` --- the build rule, because this is a build
    /// target like any other. The target itself opens the HTML report in
    /// the browser; what streams here is the generation. A board answer is
    /// deliberately not required: the report reads an already-configured
    /// build directory, and one that is missing is `west`'s error to
    /// explain.
    /// Starts the memory report (`size_report`), the one page of the build
    /// dashboard that has to be produced rather than read.
    ///
    /// The window is already closed by the time this runs (the caller closes
    /// it): a run of minutes belongs in the Monitor with `Stop` reachable,
    /// not behind a modal. `App::on_process` re-opens the window on the
    /// Memory tab when the run *succeeds* --- a failure leaves the Monitor
    /// showing why, which a modal over it would hide.
    pub(super) fn start_size_report(&mut self) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|panel| panel.resolved.clone())
        else {
            self.logs
                .warn("no Zephyr installation resolved --- the memory report needs one");
            return;
        };
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let command = match self
            .build
            .as_ref()
            .map(|panel| panel.size_report_command(backend, &workspace))
        {
            Some(Ok(command)) => command,
            Some(Err(why)) => {
                self.logs.warn(format!("memory report: {why}"));
                return;
            }
            None => return,
        };
        self.start_build_command(
            "Memory report",
            false,
            BuildAction::SizeReport,
            Focus::Build,
            move |_, _| Some(command),
        );
    }

    /// Runs what a simulator build just produced, streaming into the
    /// Monitor tab with the cursor parked on `Stop` --- the build rule.
    ///
    /// Reached only from the finished build itself, never from a row: a
    /// host build that succeeds and then waits for a second keypress is a
    /// step with no decision in it. The program is not `west`, so it is
    /// composed by the panel rather than the backend
    /// ([`crate::build::BuildPanel::run_command`]); a build that produced
    /// no executable simply logs and starts nothing.
    pub(super) fn start_run(&mut self) {
        self.start_build_command("Run", false, BuildAction::Run, Focus::Build, |panel, _| {
            panel.run_command()
        });
    }

    pub(super) fn start_dashboard(&mut self) {
        self.start_build_command(
            "Dashboard",
            false,
            BuildAction::Dashboard,
            Focus::Build,
            |panel, backend| panel.dashboard_command(backend),
        );
    }

    /// The shared body of [`Self::start_build`]/[`Self::start_flash`]:
    /// compose through the panel, run, and point the Monitor tab at the
    /// output. `focus` is where the user sits while it runs --- the panel
    /// for the build lifecycle, the Monitor tab for a flash --- and
    /// `action` is the row that was started, so the panel's cursor can
    /// return there (or its lifecycle successor) when it finishes.
    fn start_build_command(
        &mut self,
        label: &'static str,
        updates_board: bool,
        action: BuildAction,
        focus: Focus,
        command: impl FnOnce(
            &mut crate::build::BuildPanel,
            &dyn crate::backend::Backend,
        ) -> Option<crate::process::Command>,
    ) {
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if panel.is_busy() {
            // Depth, not the user-facing path: `run_build_action`'s gate
            // catches this first for every panel row. What still reaches
            // here is a confirm overlay's accept path, which calls the
            // `start_*` helpers directly.
            self.logs
                .warn("a build command is already running — stop it first");
            return;
        }
        let Some(command) = command(panel, backend) else {
            self.logs
                .warn(format!("{label}: this backend offers no such action"));
            return;
        };
        let full_label = command.to_string();
        let caps = self.manager.capabilities();
        if !panel.start(
            label,
            updates_board,
            action,
            command,
            &mut self.processes,
            &caps,
        ) {
            return;
        }
        self.logs.info(format!("running {full_label}"));
        // The command's home while it runs is the Monitor tab (`SPEC.md`
        // §11) --- showing it, without moving the user off the panel.
        self.view = super::View::Dashboard;
        self.focus = focus;
        self.log_tab = LogTab::Monitor;
        self.set_monitor_source(MonitorSource::Build);
    }

    /// Cancels the running build command at the user's request.
    pub(super) fn stop_build(&mut self) {
        let Some(panel) = &mut self.build else {
            return;
        };
        if panel.stop(&mut self.processes) {
            self.logs.warn("stopping the build command");
        }
    }
}
