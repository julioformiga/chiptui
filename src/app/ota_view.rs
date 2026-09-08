//! The OTA modal's app side: opening it with the project's answers,
//! driving its keys, and landing its results.
//!
//! The panel itself ([`crate::ota::update::OtaPanel`]) is a pure state
//! machine --- it spawns through the [`ProcessManager`] handle it is given
//! and returns updates, never logging or touching the UI (the installer's
//! split, [`super::install_view`]). Everything that needs the rest of the
//! app lives here: which project and board the modal serves, where the
//! `[ota]` answers are read from, and what each confirm leads to.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::ota::update::{OtaAction, OtaConfirm, OtaPanel};
use crate::process::ProcessEvent;
use crate::project::config;

use super::{App, Overlay};

impl App {
    /// Opens the OTA modal for the project the build panel serves.
    ///
    /// Every fact the panel needs is answered here from the project's own
    /// state: the root and build directory from the build panel, the board
    /// from its answer (a project without one cannot name the Kconfig
    /// fragment the prepare writes, so the modal refuses by name rather
    /// than opening empty), and the `[ota]` answers from the project's
    /// `chiptui.toml` --- absent, the defaults every Zephyr project starts
    /// with.
    pub fn open_ota(&mut self) {
        let Some(build) = &self.build else {
            self.logs
                .warn("OTA: no project resolved --- pick one first");
            return;
        };
        let root = build.root.clone();
        // `sysbuild.conf`, `VERSION` and the `boards/` fragment are the
        // *application's* files --- Zephyr reads them out of the source
        // directory `west build` is pointed at. The build directory below
        // stays the root's.
        let app_root = build.app_dir.clone();
        // The *board's* build directory, never the last build's: a host
        // build produces an executable and no bootloader swaps one, which
        // is `BuildPanel::flash_build_dir`'s whole reason and applies here
        // word for word. Following `build_dir` left a project with a
        // `native_sim` target reporting `Build first` forever whenever the
        // session's last build was the simulator --- `build_sim/` has no
        // `domains.yaml`, so no signed image resolves out of it, no matter
        // how many times the board is rebuilt.
        let build_dir = Some(build.flash_build_dir());
        let Some(board) = build.board_name().map(str::to_string) else {
            self.logs
                .warn("OTA: no board selected --- the Environment pane's Board row answers that");
            return;
        };
        // A panel that already describes this project is *kept*, not
        // rebuilt. `OtaPanel::new` starts with `awaiting_confirm: false`,
        // so rebuilding it on every open threw away the one state the
        // whole mechanism exists to surface: a user who read "updated ---
        // unconfirmed: the next reset reverts", pressed `Esc` and came
        // back was shown a panel offering to update again, with nothing
        // anywhere saying the running image was still unconfirmed.
        if self.ota.as_ref().is_some_and(|panel| {
            panel.serves(&root, app_root.as_deref(), &board, build_dir.as_deref())
        }) {
            if let Some(panel) = &mut self.ota {
                // Only the cheap synchronous facts: a build may well have
                // landed while the modal was closed. The requirement probe
                // is deliberately *not* re-run --- it is a subprocess, so
                // until it answered the button would read a dim `Blocked`,
                // and a reopened modal flashing "unanswered" over a halt it
                // is there to report is the opposite of the point. `r` is
                // the key that asks again.
                panel.prepare.recheck_slots();
                panel.refresh_image();
            }
            self.overlay = Some(Overlay::Ota);
            return;
        }
        let config = std::fs::read_to_string(root.join(config::FILE_NAME))
            .ok()
            .and_then(|text| config::parse_ota(&text))
            .unwrap_or_default();
        let Some(mut panel) = OtaPanel::new(&root, app_root, board, build_dir, config) else {
            self.logs
                .error("OTA: the configured method has no driver registered");
            return;
        };
        if let Some(tool) = self.ota_tool_path.clone() {
            panel.set_tool(tool);
        }
        panel.probe_requirements(&mut self.processes);
        // A fresh panel is a fresh project/board/build directory, so the
        // automatic capture is owed again.
        self.address_capture_tried = false;
        self.logs
            .info(format!("OTA: {}", panel.prepare.root.display()));
        self.ota = Some(panel);
        self.overlay = Some(Overlay::Ota);
    }

    /// The address question, asked of the board first and of the user
    /// second: starts the live capture, and opens the text entry when it
    /// cannot run (no resolved workspace, a board with no platform
    /// monitor, a spawn that failed).
    ///
    /// The fall-through is the whole reason this is one function and not
    /// two keys: a capture that cannot start must not leave the modal
    /// having done nothing, and a project whose Kconfig has no address
    /// log yet is exactly that case --- so typing it stays one keypress
    /// away rather than being replaced.
    pub(super) fn capture_or_ask_address(&mut self) {
        if !self.address_capture_tried {
            self.address_capture_tried = true;
            if self.start_address_capture() {
                return;
            }
        }
        let input = self
            .ota
            .as_ref()
            .and_then(|panel| panel.config().address.clone())
            .unwrap_or_default();
        self.overlay = Some(Overlay::OtaAddress { input });
    }

    /// Points the OTA client at a specific program, for tests (the seam
    /// [`Self::set_installer_tool_path`] is for the installer's tools).
    pub fn set_ota_tool(&mut self, program: impl Into<String>) {
        let program = program.into();
        if let Some(panel) = &mut self.ota {
            panel.set_tool(program.clone());
        }
        self.ota_tool_path = Some(program);
    }

    pub(super) fn on_ota_key(&mut self, key: KeyEvent) {
        let Some(panel) = &mut self.ota else {
            self.overlay = None;
            return;
        };
        let viewport = self.ota_viewport.max(1);
        match key.code {
            // Scrolling stays live while a stage runs --- watching the
            // upload is the whole reason the modal carries output.
            KeyCode::Char('k') | KeyCode::Up => panel.scroll_output(1, viewport),
            KeyCode::Char('j') | KeyCode::Down => panel.scroll_output(-1, viewport),
            KeyCode::PageUp => panel.scroll_output(viewport as isize, viewport),
            KeyCode::PageDown => panel.scroll_output(-(viewport as isize), viewport),
            KeyCode::Char('s') if !panel.is_busy() => panel.prepare.toggle_netshell(),
            // The net shell's cheap sibling: both answer "how does the
            // board name its own address", and both are opt-in blocks
            // whose cost rides their row.
            KeyCode::Char('l') if !panel.is_busy() => {
                panel
                    .prepare
                    .toggle_optional(crate::ota::prepare::Step::AddressLog);
            }
            // Reading the address again, at any time. The automatic one
            // runs once, when the address is first needed; after that a
            // DHCP lease that moved is the user's to notice, and this is
            // the key that acts on it.
            KeyCode::Char('a') if !panel.is_busy() => {
                self.address_capture_tried = false;
                self.capture_or_ask_address();
            }
            KeyCode::Char('r') if !panel.is_busy() => {
                panel.prepare.recheck_slots();
                panel.refresh_image();
                panel.prepare.probe_requirements(&mut self.processes);
            }
            // The address is typed by hand and nothing validates it, so the
            // cycle's own first stage is offered on its own: `os echo` is a
            // read, it needs no confirm, and a wrong address becomes a
            // one-second answer instead of a timeout partway through an
            // upload the user has already agreed to.
            KeyCode::Char('p') if !panel.is_busy() => {
                panel.probe_board(&mut self.processes);
            }
            // The transport decides the Kconfig block the prepare writes,
            // and until now it could only be answered by hand-editing
            // `chiptui.toml` --- before preparing, since the block is
            // already written afterwards.
            KeyCode::Char('t') if !panel.is_busy() => {
                let selected = crate::ota::Transport::ALL
                    .iter()
                    .position(|transport| *transport == panel.config().transport)
                    .unwrap_or(0);
                self.overlay = Some(Overlay::OtaTransport { selected });
            }
            // The other half of the transport's precedent: an answer that
            // decides what the runner does, persisted the moment it is
            // given. It is a *safety* setting, so the hint beside the
            // Update heading names the direction `c` would move it.
            KeyCode::Char('c') if !panel.is_busy() => {
                let auto = panel.auto_confirm();
                if let Err(err) = panel.set_auto_confirm(!auto) {
                    self.logs.error(format!("OTA: {err}"));
                }
            }
            // One decision, shared with the renderer: the button says what
            // it does because both read `OtaPanel::action`.
            KeyCode::Enter => match panel.action() {
                OtaAction::Stop => {
                    panel.stop(&mut self.processes);
                }
                // The board says this at every boot, so the first press
                // asks *it* rather than the user --- and falls through to
                // the text entry when the capture cannot run or finds
                // nothing, which is every project whose Kconfig does not
                // carry the address log yet.
                OtaAction::SetAddress | OtaAction::RecaptureAddress => {
                    self.capture_or_ask_address();
                }
                // The image the modal needs is the pristine sysbuild
                // build's, which belongs to the *build* panel's one process
                // slot --- so the modal closes and the run streams into the
                // Monitor with `Stop` reachable, the `ZephyrActions`
                // /`Dashboard` rule. The tick's `refresh_image` turns the
                // button back into `Update` when the image lands.
                OtaAction::Rebuild => {
                    self.overlay = None;
                    self.run_build_action(crate::build::BuildAction::Build(
                        crate::backend::BuildKind::Rebuild,
                    ));
                }
                // The confirmed update is over: close, and end the cycle
                // so reopening starts a new one rather than showing `Done`
                // for the rest of the session.
                OtaAction::Done => {
                    panel.end_cycle();
                    self.overlay = None;
                }
                // The one resumption that asks nothing: everything the
                // cycle writes has been written, and what is left is the
                // read that judges it (`OtaPanel::resume_writes`).
                OtaAction::ResumeVerify => {
                    panel.start_update(&mut self.processes);
                }
                // Every question-asking action reads its question off the
                // action itself, so the button's word and the dialog that
                // follows it cannot disagree. A retry re-asks --- the
                // installer's does too: consent for a write covers the
                // write, and a fresh look at the command is never wrong.
                // `Blocked` is the one that asks nothing: its explanation
                // is the checklist above it.
                other => {
                    if let Some(what) = other.confirm() {
                        self.ask_ota_confirm(what);
                    }
                }
            },
            // A running update is not something to leave by reflex: `Stop`
            // is the way out, and it is on screen (`SPEC.md` §12).
            // The overlay closes; the panel stays. See `open_ota`: dropping
            // it here is what lost the unconfirmed halt.
            KeyCode::Esc | KeyCode::Char('q') if !panel.is_busy() => {
                self.overlay = None;
            }
            _ => {}
        }
    }

    /// The one-deep slot's discipline: the confirm *replaces* the modal and
    /// hands it back on either answer, exactly as `Overlay::Packages`'s
    /// removal confirm does.
    fn ask_ota_confirm(&mut self, what: OtaConfirm) {
        self.overlay = Some(Overlay::ConfirmOta {
            what,
            confirm: false,
        });
    }

    /// The confirm's accept path: back to the modal, then the action.
    pub(super) fn accept_ota_confirm(&mut self, what: OtaConfirm) {
        self.overlay = Some(Overlay::Ota);
        let Some(panel) = &mut self.ota else {
            return;
        };
        match what {
            OtaConfirm::Prepare => {
                let update = panel.start_prepare();
                if let Some(notice) = update.notice {
                    self.logs.error(notice);
                }
                if update.finished {
                    self.logs.info(format!(
                        "OTA: {} is instrumented --- rebuild (sysbuild) to produce the signed image",
                        panel.prepare.root.display()
                    ));
                }
            }
            OtaConfirm::Update => {
                panel.start_update(&mut self.processes);
            }
            OtaConfirm::ConfirmImage => {
                panel.confirm_image(&mut self.processes);
            }
        }
    }

    /// The confirm's decline path: back to the modal, not to the dashboard
    /// --- declining one question is not leaving the flow.
    pub(super) fn decline_ota_confirm(&mut self) {
        self.overlay = Some(Overlay::Ota);
    }

    /// The transport picker's keys --- `ZephyrActions`' stacked-menu
    /// grammar, and the same replace-and-hand-back discipline the address
    /// entry keeps.
    pub(super) fn on_ota_transport_key(&mut self, key: KeyEvent, selected: usize) {
        let count = crate::ota::Transport::ALL.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Some(Overlay::Ota),
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Some(Overlay::OtaTransport {
                    selected: (selected + count - 1) % count,
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Some(Overlay::OtaTransport {
                    selected: (selected + 1) % count,
                });
            }
            KeyCode::Enter => {
                if let Some(transport) = crate::ota::Transport::ALL.get(selected).copied()
                    && let Some(panel) = &mut self.ota
                    && let Err(err) = panel.set_transport(transport)
                {
                    self.logs
                        .error(format!("could not record the transport: {err}"));
                }
                self.overlay = Some(Overlay::Ota);
            }
            _ => {}
        }
    }

    pub(super) fn on_ota_address_key(&mut self, key: KeyEvent, mut input: String) {
        match key.code {
            KeyCode::Esc => self.overlay = Some(Overlay::Ota),
            KeyCode::Enter => {
                let address = input.trim().to_string();
                if !address.is_empty()
                    && let Some(panel) = &mut self.ota
                    && let Err(err) = panel.set_address(address)
                {
                    self.logs
                        .error(format!("could not record the address: {err}"));
                }
                self.overlay = Some(Overlay::Ota);
            }
            KeyCode::Backspace => {
                input.pop();
                self.overlay = Some(Overlay::OtaAddress { input });
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                self.overlay = Some(Overlay::OtaAddress { input });
            }
            _ => {}
        }
    }

    /// Feeds a process event to the OTA panel, if one exists. Called from
    /// [`App::on_process`] beside the other panels: the panel guards on its
    /// own process ids and ignores what is not its own.
    pub(super) fn ota_on_process(&mut self, event: &ProcessEvent) {
        let Some(panel) = &mut self.ota else {
            return;
        };
        let update = panel.on_process(event, &mut self.processes);
        if let Some(notice) = update.notice {
            if update.finished || update.halted_unconfirmed {
                self.logs.info(notice);
            } else {
                self.logs.error(notice);
            }
        }
    }

    /// The tick's half: the post-reset settle ends on a deadline, and a
    /// build landing while the modal is open turns `BuildFirst` into
    /// `Update`.
    pub(super) fn drive_ota(&mut self) {
        let Some(panel) = &mut self.ota else {
            return;
        };
        panel.tick(&mut self.processes);
    }
}
