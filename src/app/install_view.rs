//! The Zephyr installer's app side: opening it, driving its keys, and
//! landing its result in the config.
//!
//! The panel itself ([`crate::install::Installer`]) is a pure state machine
//! --- it spawns through the [`ProcessManager`](crate::process::ProcessManager)
//! handle it is given and returns updates, never logging or touching the UI.
//! Everything that needs the rest of the app lives here: which folder to
//! install into, what a finished installation writes, and where the user
//! goes next.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::install::{Action, Installer};
use crate::process::ProcessEvent;
use crate::workspace::DirPurpose;

use super::{App, Overlay};

impl App {
    /// Opens the installer over `dir`, the folder the picker accepted.
    ///
    /// The workspace goes into `zephyr/` *inside* it --- one folder holding
    /// the whole installation, the way the getting-started guide lays it
    /// out --- unless `dir` already carries a `.west/`, which means the
    /// user pointed at a half-finished workspace and wants that one
    /// resumed, not a second one nested inside it.
    pub fn open_installer(&mut self, dir: std::path::PathBuf) {
        let root = if dir.join(".west").is_dir() {
            dir
        } else {
            dir.join("zephyr")
        };
        let mut installer = Installer::new(root);
        for (tool, program) in self.installer_tool_paths.clone() {
            installer.set_tool_path(tool, program);
        }
        // A complete installation sitting where the user pointed is not
        // something to install over: the panel adopts it instead. Decided
        // here, once, from the same predicate the picker validates with.
        if crate::backend::zephyr::workspace::install_state(&installer.root)
            == crate::backend::zephyr::workspace::InstallState::Complete
        {
            installer.mark_already_installed();
        }
        installer.probe_prereqs(&mut self.processes);
        self.logs.info(format!(
            "Zephyr installer: target {}",
            installer.root.display()
        ));
        self.installer = Some(installer);
        self.overlay = Some(Overlay::ZephyrInstall);
    }

    /// Points one of the installer's tools at a specific program, for tests
    /// (the seam [`crate::build::BuildPanel::set_tool_path`] is for `west`).
    /// Recorded on the app rather than the panel because the panel is
    /// created much later, when the user picks a folder.
    pub fn set_installer_tool_path(&mut self, tool: &'static str, program: impl Into<String>) {
        let program = program.into();
        if let Some(entry) = self
            .installer_tool_paths
            .iter_mut()
            .find(|(name, _)| *name == tool)
        {
            entry.1 = program.clone();
        } else {
            self.installer_tool_paths.push((tool, program.clone()));
        }
        if let Some(installer) = &mut self.installer {
            installer.set_tool_path(tool, program);
        }
    }

    pub(super) fn on_install_key(&mut self, key: KeyEvent) {
        let Some(installer) = &mut self.installer else {
            self.overlay = None;
            return;
        };
        let viewport = self.install_viewport.max(1);
        match key.code {
            // Scrolling stays live while a step runs --- watching the
            // output is the whole reason the modal carries it.
            KeyCode::Char('k') | KeyCode::Up => installer.scroll_output(1, viewport),
            KeyCode::Char('j') | KeyCode::Down => installer.scroll_output(-1, viewport),
            KeyCode::PageUp => installer.scroll_output(viewport as isize, viewport),
            KeyCode::PageDown => installer.scroll_output(-(viewport as isize), viewport),
            KeyCode::Char('s') => installer.toggle_sdk(),
            KeyCode::Char('t') => self.open_sdk_toolchains(),
            KeyCode::Char('r') if !installer.is_busy() => {
                installer.probe_prereqs(&mut self.processes);
            }
            // One decision, shared with the renderer: the button says what
            // it does because both read `Installer::action`.
            KeyCode::Enter => match installer.action() {
                Action::Stop => {
                    installer.stop(&mut self.processes);
                }
                Action::PickToolchains => self.open_sdk_toolchains(),
                Action::Adopt => self.adopt_installation(),
                Action::InstallSdk | Action::AddToolchains => self.install_missing_sdk(),
                Action::Install | Action::Retry => self.confirm_install(),
                // Their explanation is already on screen: the prerequisite
                // checklist above, or a sequence with nothing left.
                Action::Blocked | Action::Done => {}
            },
            // A running installation is not something to leave by reflex:
            // `Stop` is the way out, and it is on screen (`SPEC.md` §12).
            KeyCode::Esc | KeyCode::Char('q') if !installer.is_busy() => self.overlay = None,
            _ => {}
        }
    }

    /// Asks before the sequence starts. One confirm for the whole run, not
    /// one per step: the user asked for an installation, and every command
    /// after this is that installation. It still names the target and the
    /// cost the way `SPEC.md` §15 requires --- several GB into a folder is
    /// worth stating even though nothing is overwritten.
    fn confirm_install(&mut self) {
        let Some(installer) = &self.installer else {
            return;
        };
        if !installer.can_start() {
            if !installer.prereqs_ready() {
                self.logs
                    .warn("install the missing prerequisites first — the checklist names them");
            }
            return;
        }
        // The literal first command, not a summary of the sequence: what
        // the dialog quotes is what `Enter` actually runs next.
        let command = installer
            .next_step()
            .and_then(|index| installer.step_command(index))
            .map_or_else(
                || format!("west init -m {} .", crate::install::steps::MANIFEST_URL),
                |command| command.to_string(),
            );
        self.overlay = Some(Overlay::Confirm {
            message: command,
            confirm: false,
        });
        self.install_confirm_pending = true;
    }

    /// Whether the open shared confirm belongs to the installer.
    pub(super) fn install_confirm_pending(&self) -> bool {
        self.install_confirm_pending
    }

    /// The confirm's accept path.
    pub(super) fn start_install(&mut self) {
        self.install_confirm_pending = false;
        let Some(installer) = &mut self.installer else {
            return;
        };
        if let Err(err) = std::fs::create_dir_all(&installer.root) {
            self.logs.error(format!(
                "could not create {}: {err}",
                installer.root.display()
            ));
            self.overlay = Some(Overlay::ZephyrInstall);
            return;
        }
        installer.start(&mut self.processes);
        self.overlay = Some(Overlay::ZephyrInstall);
    }

    /// The confirm's decline path: back to the installer, not to the
    /// dashboard --- declining one question is not leaving the flow.
    pub(super) fn cancel_install(&mut self) {
        self.install_confirm_pending = false;
        self.overlay = Some(Overlay::ZephyrInstall);
    }

    fn open_sdk_toolchains(&mut self) {
        if self.installer.is_none() {
            return;
        }
        self.overlay = Some(Overlay::SdkToolchains { selected: 0 });
    }

    pub(super) fn on_sdk_toolchains_key(&mut self, key: KeyEvent, selected: usize) {
        let Some(installer) = &mut self.installer else {
            self.overlay = None;
            return;
        };
        let toolchains = crate::install::steps::TOOLCHAINS;
        let count = toolchains.len();
        match key.code {
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay = Some(Overlay::SdkToolchains {
                    selected: (selected + count - 1) % count,
                });
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay = Some(Overlay::SdkToolchains {
                    selected: (selected + 1) % count,
                });
            }
            KeyCode::Char(' ') => {
                let Some(name) = toolchains.get(selected).map(|name| (*name).to_string()) else {
                    return;
                };
                match installer
                    .picked_toolchains
                    .iter()
                    .position(|picked| *picked == name)
                {
                    Some(index) => {
                        installer.picked_toolchains.remove(index);
                    }
                    None => installer.picked_toolchains.push(name),
                }
                // The checklist has to agree with the button: picking a
                // toolchain the installed bundle lacks makes the SDK step
                // something to run again.
                installer.refresh_sdk_step();
            }
            // Both doors lead back to the installer: the pick is applied as
            // it is made, so there is nothing to confirm or discard.
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Some(Overlay::ZephyrInstall);
            }
            _ => {}
        }
    }

    /// Feeds a process event to the installer, if one exists. Called from
    /// [`App::on_process`] beside the other panels: each guards on its own
    /// process ids and ignores what is not its own.
    pub(super) fn install_on_process(&mut self, event: &ProcessEvent) {
        let Some(mut installer) = self.installer.take() else {
            return;
        };
        let update = installer.on_process(event, &mut self.processes);
        self.installer = Some(installer);
        if let Some(notice) = update.notice {
            self.logs.error(notice);
        }
        if update.stopped {
            self.salvage_installation();
        }
        if update.finished {
            self.finish_install();
        }
    }

    /// A step failed and stopped the sequence --- but what already ran may
    /// have left a perfectly usable workspace behind (`west init` and
    /// `west update` both succeeding is enough). Record it rather than lose
    /// it: before this, stopping anywhere meant `[zephyr] workspace` was
    /// never written, and a complete checkout sat on disk that ChipTUI did
    /// not know about.
    ///
    /// The modal stays open and the panel alive: the failed step is still
    /// ✗ and `Retry` is still the way on. Nothing chains anywhere.
    fn salvage_installation(&mut self) {
        let Some(installer) = &self.installer else {
            return;
        };
        let root = installer.root.clone();
        let step = installer
            .failed_step()
            .map_or("the remaining step", crate::install::Step::label);
        if crate::backend::zephyr::workspace::install_state(&root)
            != crate::backend::zephyr::workspace::InstallState::Complete
        {
            return;
        }
        // Already recorded --- a retry that fails again must not repeat the
        // line.
        if self
            .workspace
            .as_ref()
            .and_then(crate::workspace::WorkspacePanel::dir)
            == Some(&root)
        {
            return;
        }
        let sdk = installer.installed_sdk();
        self.persist_installation(&root, sdk);
        self.logs.info(format!(
            "the workspace at {} is usable — recorded even though {step} still needs to run",
            root.display()
        ));
    }

    /// The adopt path for a workspace whose SDK step still has something to
    /// do --- no bundle at all, or a bundle missing a toolchain the user
    /// just picked. Records the installation *first* (that answer is
    /// already correct, and closing the modal must not lose it), then runs
    /// the SDK step, which asks west only for what is absent.
    fn install_missing_sdk(&mut self) {
        let Some(installer) = &self.installer else {
            return;
        };
        let root = installer.root.clone();
        if self
            .workspace
            .as_ref()
            .and_then(crate::workspace::WorkspacePanel::dir)
            != Some(&root)
        {
            self.persist_installation(&root, None);
        }
        if let Some(installer) = &mut self.installer {
            installer.start_sdk_only(&mut self.processes);
        }
    }

    /// Adopts the installation the panel found already in place: validate
    /// it once more through the picker's own check, record it, and leave.
    /// Nothing is spawned --- this path exists precisely because there is
    /// nothing left to run.
    fn adopt_installation(&mut self) {
        let Some(installer) = &self.installer else {
            return;
        };
        let root = installer.root.clone();
        let sdk = installer.installed_sdk();
        self.persist_installation(&root, sdk);
        self.overlay = None;
        self.installer = None;
        self.offer_projects_folder();
    }

    /// A finished installation: persist it the way every other environment
    /// answer is persisted (config first --- a pick only counts once
    /// written, see [`crate::workspace::WorkspacePanel::apply_resolution`]),
    /// then move on to the next question the checklist still has open.
    fn finish_install(&mut self) {
        let Some(installer) = &self.installer else {
            return;
        };
        let root = installer.root.clone();
        let sdk = installer.installed_sdk();
        self.persist_installation(&root, sdk);
        self.overlay = None;
        self.installer = None;
        self.offer_projects_folder();
    }

    /// Writes the installation into the config and re-resolves from it.
    /// Shared by the two ways an installation becomes the answer: running
    /// the sequence to the end, and adopting one that was already there.
    ///
    /// A *second* installation replaces the first --- someone who just
    /// installed Zephyr somewhere means to use it --- so the switch is
    /// named in the log rather than left to be discovered in the pane.
    fn persist_installation(&mut self, root: &std::path::Path, sdk: Option<std::path::PathBuf>) {
        let previous = self
            .workspace
            .as_ref()
            .and_then(crate::workspace::WorkspacePanel::dir)
            .cloned();
        let (project_root, project_settings, _user) = self.zephyr_settings();
        let target = self.settings_target(&project_root, project_settings.as_ref());

        match crate::settings::save_workspace(&target, root) {
            Ok(()) => match previous {
                Some(previous) if previous != root => self.logs.info(format!(
                    "Zephyr installation switched from {} to {}",
                    previous.display(),
                    root.display()
                )),
                _ => self
                    .logs
                    .info(format!("Zephyr installation saved to {}", target.display())),
            },
            Err(err) => self
                .logs
                .error(format!("could not save {}: {err}", target.display())),
        }
        // The SDK's directory carries its version in its name, so the key
        // is worth writing: it is what makes `ZEPHYR_SDK_INSTALL_DIR` and
        // the Project pane's version badge answerable.
        if let Some(sdk) = sdk
            && let Err(err) = crate::settings::save_sdk(&target, &sdk)
        {
            self.logs
                .error(format!("could not save the SDK location: {err}"));
        }

        self.refresh_workspace_resolution();
    }

    /// The environment's next open question, once the installation answered
    /// its first. Only when it *is* still open: a second installation on a
    /// machine that already has a projects folder must not re-ask something
    /// already answered.
    fn offer_projects_folder(&mut self) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|panel| panel.projects.is_none())
        {
            self.open_projects_dir_picker();
        }
    }

    /// The dashboard's `s`: open the SDK toolchain picker over the
    /// installation that is already configured.
    ///
    /// The installer's own flow reaches this state too, but only after
    /// re-answering questions the config already holds (the path picker,
    /// then its offer). This is the same destination by the short way, for
    /// the errand it exists for: unpacking one more toolchain into an SDK
    /// that is otherwise fine.
    ///
    /// Without a resolved installation there is nothing to add to, and the
    /// key deliberately invents no path --- `Zephyr path` is where that
    /// question lives.
    pub(super) fn open_sdk_toolchains_shortcut(&mut self) {
        let Some(root) = self
            .workspace
            .as_ref()
            .and_then(crate::workspace::WorkspacePanel::dir)
            .cloned()
        else {
            self.logs
                .warn("no Zephyr installation configured — answer Zephyr path first (ctrl+k, e)");
            return;
        };
        // The resolved directory *is* the workspace root (it carries
        // `.west/`, which is what validated it), so this opens the panel on
        // it rather than on a `zephyr/` inside it.
        self.open_installer(root);
        self.overlay = Some(Overlay::SdkToolchains { selected: 0 });
    }

    /// The `Install Zephyr` button: asks *where* first. The picker's
    /// accepted folder is the installer's target's parent --- see
    /// [`Self::open_installer`].
    pub(super) fn open_install_picker(&mut self) {
        self.open_purpose_picker(DirPurpose::Install);
    }

    /// The installer's accept path from the directory picker.
    pub(super) fn accept_install_dir(&mut self, dir: std::path::PathBuf) {
        self.open_installer(dir);
    }

    /// Offers the installer over a directory the installation picker just
    /// refused. The offer names what is actually at the target --- nothing,
    /// a half-built workspace, or a complete installation to adopt --- so
    /// the answer is not always the word "install"
    /// (`ui::overlay::install_offer` does the wording).
    pub(super) fn offer_install(&mut self, dir: std::path::PathBuf, reason: String) {
        self.overlay = Some(Overlay::ConfirmInstallHere {
            dir,
            reason,
            // Nothing is destroyed and nothing runs yet: the modal this
            // opens confirms again before its first command. Declining is
            // the unusual answer here, so Yes leads.
            confirm: true,
        });
    }

    /// The offer's decline path: back to the picker, showing the refusal
    /// that opened the offer --- the user is where they were, with the
    /// reason still on screen.
    pub(super) fn decline_install_offer(&mut self, dir: std::path::PathBuf, reason: String) {
        self.overlay = Some(Overlay::DirPicker {
            purpose: DirPurpose::Installation,
            path: dir,
            selected: 0,
            error: Some(reason),
        });
    }

    /// The step list, for tests and for the overlay's rows.
    pub fn install_steps(&self) -> Option<&[crate::install::StepState]> {
        self.installer
            .as_ref()
            .map(|installer| installer.steps.as_slice())
    }
}
