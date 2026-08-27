//! Dashboard file-browser dispatch: navigation, the per-file action menu,
//! the local/device viewer, and the transfers they trigger. Split out of
//! `app.rs` since it is the one subsystem `App` drives almost entirely
//! through [`crate::browser::Browser`] and never touches [`crate::flash`].

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::micropython::commands;
use crate::backend::{Capabilities, Capability};
use crate::browser::{Browser, DeviceView, Notice, Side, Transfer, TransferKind};
use crate::device::DevicePath;
use crate::files::{self, SyncStatus};
use crate::flash::FlashPanel;
use crate::process::ProcessManager;

use super::{App, Overlay, PendingEdit, RunState};

impl App {
    /// Handles a key while [`super::Focus::FilesLocal`]/[`super::Focus::FilesDevice`] holds
    /// focus. `Tab`/`BackTab`, `x`, `?` and `d` are dashboard-wide and
    /// already handled by [`App::on_dashboard_key`] before this is reached,
    /// so only the file browser's own navigation remains here.
    ///
    /// Keys that can reach the device are routed through
    /// [`Self::dispatch_browser`]: it keeps the serial port's users from
    /// racing each other (a run session, the probe), and opens the interrupt
    /// confirmation should the gate hold the resulting request.
    pub(super) fn on_files_key(&mut self, key: KeyEvent) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        // The two columns are separate `Focus` stops now, so the browser's
        // own notion of which side is active just follows it.
        browser.focus = match self.focus {
            super::Focus::FilesDevice => Side::Device,
            _ => Side::Local,
        };

        // `→`/`←` are side-aware: `Browser::enter`/`ascend` navigate the
        // local pane without touching the device, so they dispatch under any
        // backend. Comparison, sync and a device-side reload are Filesystem
        // operations --- without the capability there is nothing on the other
        // end, and the device column cannot hold focus in the first place
        // (focus_order/clamp_focus).
        let has_filesystem = self.manager.capabilities().contains(Capability::Filesystem);
        // A device-pane reload is the firmware gate's recovery path: after a
        // re-flash (or a refused read) it re-identifies before listing, so
        // it cannot go straight to the browser the way the local reload
        // below does.
        if has_filesystem && key.code == KeyCode::Char('r') && browser.focus == Side::Device {
            self.browser = Some(browser);
            self.reload_device_pane();
            return;
        }
        let reaches_device = match key.code {
            KeyCode::Right | KeyCode::Left | KeyCode::Backspace => true,
            KeyCode::Char('c' | 'S') => has_filesystem,
            _ => false,
        };
        if reaches_device {
            self.browser = Some(browser);
            let code = key.code;
            self.dispatch_browser(move |browser, processes, port| match code {
                KeyCode::Right => browser.enter(processes, port),
                KeyCode::Left | KeyCode::Backspace => browser.ascend(processes, port),
                KeyCode::Char('c') => browser.verify_selected(processes, port),
                KeyCode::Char('S') => browser.request_sync(processes, port),
                KeyCode::Char('r') => browser.load_device(processes, port, true),
                _ => Vec::new(),
            });
            return;
        }

        let notices: Vec<Notice> = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                browser.move_cursor(-1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                browser.move_cursor(1);
                Vec::new()
            }
            KeyCode::PageUp => {
                browser.move_cursor(-10);
                Vec::new()
            }
            KeyCode::PageDown => {
                browser.move_cursor(10);
                Vec::new()
            }
            KeyCode::Home => {
                browser.cursor_to(0);
                Vec::new()
            }
            KeyCode::End => {
                browser.cursor_to(usize::MAX);
                Vec::new()
            }
            KeyCode::Enter => {
                if let Some(name) = browser.selected_name(browser.focus) {
                    let status = browser.statuses().get(&name).copied();
                    self.overlay = Some(Overlay::FileActions {
                        side: browser.focus,
                        is_dir: browser.selected_is_dir(browser.focus),
                        name,
                        status,
                        selected: 0,
                    });
                }
                Vec::new()
            }
            KeyCode::Char('r') => {
                browser.reload_local();
                Vec::new()
            }
            KeyCode::Char('a') => {
                self.overlay = Some(Overlay::CreateEntry {
                    side: browser.focus,
                    input: String::new(),
                });
                Vec::new()
            }
            KeyCode::Char('h') => {
                browser.toggle_hidden();
                Vec::new()
            }
            KeyCode::Char('i')
                if browser.focus == Side::Device
                    && self
                        .manager
                        .capabilities()
                        .contains(Capability::PackageInstall) =>
            {
                self.open_package_manager();
                Vec::new()
            }
            _ => Vec::new(),
        };

        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// The directory whichever local source currently has focus is showing:
    /// MicroPython's file browser (`browser.local_path`) or the Zephyr
    /// workspace pane's embedded file list (`workspace.files_path`). The two
    /// are mutually exclusive by capability --- `ensure_browser_scanning`
    /// never creates a browser for a backend with a build panel
    /// (`build_pane_visible_precondition`), and the workspace pane's file
    /// section only exists for exactly that kind of backend --- so checking
    /// the browser first is never ambiguous.
    fn local_dir(&self) -> Option<PathBuf> {
        self.browser
            .as_ref()
            .map(|browser| browser.local_path.clone())
            .or_else(|| {
                self.workspace
                    .as_ref()
                    .map(|panel| panel.files_path.clone())
            })
    }

    fn local_entry_path(&self, name: &str) -> Option<PathBuf> {
        self.local_dir().map(|dir| dir.join(name))
    }

    /// Reloads whichever local source is active, mirroring [`Self::local_dir`].
    fn reload_local_pane(&mut self) {
        if let Some(browser) = &mut self.browser {
            browser.reload_local();
        } else if let Some(panel) = &mut self.workspace {
            panel.reload_files();
        }
    }

    /// Refreshes whichever local source is active when its directory
    /// changed *outside the program* (another editor, a build, a sync
    /// tool). Polled once a second (the cadence of
    /// [`Self::check_device_hotplug`]): a `readdir` over an
    /// embedded-project directory is cheap, and an unchanged listing is
    /// dropped without a reload, so there is no cursor or redraw churn.
    /// The swap itself is silent --- the pane just starts telling the
    /// truth a second later, with no log line per external save.
    pub(super) fn refresh_local_listings(&mut self) {
        // only check every 4 ticks (1 second)
        if !self.ticks.is_multiple_of(4) {
            return;
        }
        if let Some(browser) = self
            .browser
            .as_mut()
            .filter(|browser| browser.local_listing_changed())
        {
            browser.reload_local();
        } else if let Some(panel) = self
            .workspace
            .as_mut()
            .filter(|panel| panel.files_listing_changed())
        {
            panel.reload_files();
        }
    }

    /// Runs the action chosen from [`Overlay::FileActions`]. `name` is the
    /// entry's name in whichever directory `side` currently shows --- stable
    /// for the duration of the menu, since an open overlay routes every key
    /// to [`App::on_overlay_key`] instead of the browser's own navigation.
    /// `is_dir` is carried alongside it since [`FileAction::for_entry`] can
    /// offer the same variant (`Delete`, or a directory's `SendToDevice`/
    /// `Download`) for either kind of entry, and only the browser call
    /// underneath differs.
    ///
    /// The workspace pane's embedded file list reaches this too, without a
    /// menu: its `Enter`/`v`/`Del` resolve to `Edit`/`View`/`Delete` on
    /// [`Side::Local`] directly, so both surfaces share one path per action
    /// (including the delete confirmation's default-No).
    pub(super) fn run_file_action(
        &mut self,
        side: Side,
        name: &str,
        is_dir: bool,
        action: FileAction,
    ) {
        match (side, action) {
            // A directory's menu always starts on `Open` (`for_entry`'s
            // default selection): re-run the same descend `Enter` used to
            // do directly, now one keypress later. `self.browser` and the
            // workspace pane's file section are mutually exclusive (see
            // `Self::local_dir`), so exactly one of these branches applies.
            (_, FileAction::Open) if self.browser.is_some() => {
                self.dispatch_browser(|browser, processes, port| browser.enter(processes, port));
            }
            (_, FileAction::Open) => {
                if let Some(panel) = &mut self.workspace {
                    panel.enter_files();
                }
            }
            (_, FileAction::Diff) => self.open_diff(name),
            (Side::Local, FileAction::View) => {
                let Some(path) = self.local_entry_path(name) else {
                    return;
                };
                self.open_local_file_viewer(path);
            }
            (Side::Local, FileAction::Edit) => {
                let Some(path) = self.local_entry_path(name) else {
                    return;
                };
                self.pending_edit = Some(PendingEdit {
                    path,
                    device_target: None,
                });
            }
            (Side::Local, FileAction::SendToDevice) => {
                self.overlay = Some(Overlay::ConfirmUpload {
                    name: name.to_string(),
                    is_dir,
                    confirm: false,
                });
            }
            (Side::Local, FileAction::Run) => self.open_run_session(name),
            (Side::Local, FileAction::Delete) => {
                self.overlay = Some(Overlay::ConfirmDelete {
                    side: Side::Local,
                    name: name.to_string(),
                    is_dir,
                    confirm: false,
                });
            }
            (Side::Device, FileAction::View) => self.open_device_file_viewer(name),
            (Side::Device, FileAction::Download) if is_dir => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_download_dir(name, processes, port)
                });
            }
            (Side::Device, FileAction::Download) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_download(name, processes, port)
                });
            }
            (Side::Device, FileAction::Edit) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_edit_download(name, processes, port)
                });
            }
            (Side::Device, FileAction::Delete) => {
                self.overlay = Some(Overlay::ConfirmDelete {
                    side: Side::Device,
                    name: name.to_string(),
                    is_dir,
                    confirm: false,
                });
            }
            // `FileAction::for_entry` never offers `Download`/`Run` on `Local`
            // and `Device` respectively, nor `SendToDevice` on `Device`.
            (Side::Local, FileAction::Download)
            | (Side::Device, FileAction::SendToDevice)
            | (Side::Device, FileAction::Run) => {}
        }
    }

    pub(super) fn delete_file(&mut self, side: Side, name: &str, is_dir: bool) {
        match (side, is_dir) {
            (Side::Local, false) => {
                let Some(path) = self.local_entry_path(name) else {
                    return;
                };
                match std::fs::remove_file(&path) {
                    Ok(_) => {
                        self.logs.success(format!("{} removed", path.display()));
                        self.reload_local_pane();
                    }
                    Err(e) => {
                        self.logs
                            .error(format!("{}: remove failed: {e}", path.display()));
                    }
                }
            }
            (Side::Local, true) => {
                let Some(path) = self.local_entry_path(name) else {
                    return;
                };
                match std::fs::remove_dir_all(&path) {
                    Ok(_) => {
                        self.logs.success(format!("{} removed", path.display()));
                        self.reload_local_pane();
                    }
                    Err(e) => {
                        self.logs
                            .error(format!("{}: remove failed: {e}", path.display()));
                    }
                }
            }
            (Side::Device, false) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_remove_device(name, processes, port)
                });
            }
            (Side::Device, true) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_remove_device_dir(name, processes, port)
                });
            }
        }
    }

    /// Runs the create-entry action (`a`): a trailing `/` on the typed name
    /// means "create a directory" (`SPEC.md` §9), otherwise an empty file.
    /// Local creation is synchronous, like [`Self::delete_file`]'s local
    /// arm; the device side queues through [`Browser`] like everything else
    /// that touches the port.
    pub(super) fn create_entry(&mut self, side: Side, input: &str) {
        let input = input.trim();
        let is_dir = input.ends_with('/');
        let name = input.trim_end_matches('/').trim();
        if name.is_empty() {
            self.logs.warn("type a name first");
            return;
        }

        match side {
            Side::Local => {
                let Some(dir) = self.local_dir() else {
                    return;
                };
                let path = dir.join(name);
                let result = if is_dir {
                    std::fs::create_dir(&path)
                } else {
                    std::fs::File::create_new(&path).map(|_| ())
                };
                match result {
                    Ok(()) => {
                        self.logs.success(format!("{} created", path.display()));
                        self.reload_local_pane();
                    }
                    Err(e) => self
                        .logs
                        .error(format!("{}: create failed: {e}", path.display())),
                }
            }
            Side::Device => {
                self.dispatch_browser(|browser, processes, port| {
                    if is_dir {
                        browser.request_mkdir(name, processes, port)
                    } else {
                        browser.request_touch(name, processes, port)
                    }
                });
            }
        }
    }

    /// Runs the rename prompt (`r` in the workspace file list): a new *name*
    /// for the listed entry, in the same directory. A `/` in the typed text
    /// would turn the rename into a move, so it is refused with the reason
    /// instead of acted on. Local and synchronous, like
    /// [`Self::create_entry`]'s local arm; a reload follows so the list
    /// shows the new name.
    pub(super) fn rename_entry(&mut self, old: &str, input: &str) {
        let name = input.trim().trim_end_matches('/');
        if name.is_empty() {
            self.logs.warn("type a name first");
            return;
        }
        // An unedited confirm is a quiet no-op, not an error.
        if name == old {
            return;
        }
        if name.contains('/') {
            self.logs
                .warn("rename stays in this directory (a name, not a path)");
            return;
        }
        let Some(dir) = self.local_dir() else {
            return;
        };
        let from = dir.join(old);
        let to = dir.join(name);
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                self.logs
                    .success(format!("{} renamed to {}", from.display(), to.display()));
                self.reload_local_pane();
            }
            Err(e) => self
                .logs
                .error(format!("{}: rename failed: {e}", from.display())),
        }
    }

    /// Takes `self.browser` for the duration of `f`, supplying the selected
    /// port, then puts it back and logs whatever `f` reports --- the same
    /// take/replace/log shape every browser-mutating key handler here
    /// already repeats, pulled out for the three new file-transfer actions.
    ///
    /// Refuses to dispatch while a `run` session holds the serial port: the
    /// PTY-based run bypasses the browser queue, so the browser's own
    /// `is_busy` would not catch the contention.
    pub(super) fn dispatch_browser(
        &mut self,
        f: impl FnOnce(&mut Browser, &mut ProcessManager, Option<&str>) -> Vec<Notice>,
    ) {
        if self.run_process.is_some() {
            self.logs
                .warn("a script is running — wait for it to finish first");
            return;
        }
        if self.probe.is_some() {
            self.logs
                .warn("still checking the device — try again in a moment");
            return;
        }
        // The background esptool query holds the port for a few seconds; an
        // mpremote request started now would fail to open it.
        if self.flash.as_ref().is_some_and(FlashPanel::is_busy) {
            self.logs
                .warn("reading device info — try again in a moment");
            return;
        }
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = f(&mut browser, &mut self.processes, port.as_deref());
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
        // The request may have hit the interrupt gate instead of starting.
        self.check_interrupt_gate();
    }

    /// Reads `path` and opens [`Overlay::FileViewer`] over it, synchronously
    /// --- a local read never has to wait. A file that cannot be shown
    /// (binary, too large, unreadable) still opens the viewer, with the
    /// reason in place of content, rather than doing nothing.
    fn open_local_file_viewer(&mut self, path: PathBuf) {
        let state = match files::read_text_file(&path) {
            Ok(content) => ViewerState::Ready {
                lines: content.lines().map(str::to_string).collect(),
            },
            Err(message) => ViewerState::Error(message),
        };
        self.viewer = Some(FileViewer {
            source: ViewerSource::Local(path),
            state,
            scroll: 0,
        });
        self.overlay = Some(Overlay::FileViewer);
    }

    /// Opens [`Overlay::FileViewer`] on `name`, in the current device
    /// directory, and queues the `cat` that will fill it in --- unlike the
    /// local case this cannot be synchronous, so the viewer opens straight
    /// into [`ViewerState::Loading`].
    fn open_device_file_viewer(&mut self, name: &str) {
        let Some(browser) = &self.browser else { return };
        let path = browser.device_path.join(name);

        if let Some(size) = browser.device_entry_size(name)
            && size > files::MAX_VIEW_BYTES
        {
            self.logs.warn(format!(
                "{path}: too large to preview ({} MiB) --- use 'Download' or 'Edit' instead",
                size / (1024 * 1024)
            ));
            return;
        }

        self.viewer = Some(FileViewer {
            source: ViewerSource::Device(path),
            state: ViewerState::Loading,
            scroll: 0,
        });
        self.overlay = Some(Overlay::FileViewer);
        self.dispatch_browser(|browser, processes, port| {
            browser.request_device_view(name, processes, port)
        });
    }

    /// Opens [`Overlay::FileViewer`] showing a unified diff of `name` between
    /// the local copy and the device copy. The local half is read now; the
    /// device half arrives later through the same `cat` the plain viewer uses
    /// --- [`Self::apply_device_view`] computes the diff once it lands, so the
    /// viewer opens straight into [`ViewerState::Loading`].
    fn open_diff(&mut self, name: &str) {
        let Some(browser) = &self.browser else {
            return;
        };
        let local = browser.local_path.join(name);
        let device = browser.device_path.join(name);

        if let Some(size) = browser.device_entry_size(name)
            && size > files::MAX_VIEW_BYTES
        {
            self.logs.warn(format!(
                "{device}: too large to diff ({} MiB) --- use 'Download' or 'Edit' instead",
                size / (1024 * 1024)
            ));
            return;
        }

        self.logs.info(format!("diffing {name} (local ↔ device)"));
        self.viewer = Some(FileViewer {
            source: ViewerSource::Diff { local, device },
            state: ViewerState::Loading,
            scroll: 0,
        });
        self.overlay = Some(Overlay::FileViewer);
        self.dispatch_browser(|browser, processes, port| {
            browser.request_device_view(name, processes, port)
        });
    }

    /// Starts a `mpremote run` session for `name` in a PTY, displayed in the
    /// Monitor tab under [`MonitorSource::Run`](super::MonitorSource::Run).
    /// The PTY lets Ctrl+C send a KeyboardInterrupt (0x03) to the running
    /// script, and the output streams line by line with timestamps instead of
    /// arriving all at once when the script finishes.
    ///
    /// Serial exclusivity: if the browser is busy, the run is refused rather
    /// than racing an in-flight `mpremote` for the port.
    pub(super) fn open_run_session(&mut self, name: &str) {
        if self.browser.as_ref().is_some_and(Browser::is_busy) {
            self.logs
                .warn("device is busy — wait for the current operation to finish");
            return;
        }
        if self
            .browser
            .as_ref()
            .is_some_and(Browser::held_for_interrupt)
        {
            self.logs
                .warn("a script is running — answer the interrupt prompt first");
            return;
        }
        if self.device_monitor_process.is_some() {
            self.logs
                .warn("close the monitor/REPL before running a script");
            return;
        }
        if self.version_capture.is_some() {
            self.logs
                .warn("reading the firmware version — wait for it to finish");
            return;
        }

        let Some(browser) = &self.browser else { return };
        let local_path = browser.local_path.join(name);
        let port = self.devices.selected_port().map(str::to_string);

        let mut command = commands::run(port.as_deref(), &local_path);
        if let Some(tool) = browser.tool_path() {
            command = command.with_program(tool);
        }

        self.run_script = Some(local_path.clone());
        self.run_output.clear();
        self.run_console.reset();
        self.run_state = RunState::Running;
        self.set_monitor_source(super::MonitorSource::Run);
        self.focus = super::Focus::Logs;
        self.log_tab = super::LogTab::Monitor;

        self.logs.info(format!("running {}", local_path.display()));

        let timeout = crate::browser::RUN_TIMEOUT;
        match self.processes.spawn_pty(command, timeout) {
            Ok(id) => self.run_process = Some(id),
            Err(e) => {
                self.logs.error(format!("could not start run: {e}"));
                self.run_state = RunState::Idle;
            }
        }
    }

    /// Saves the current run output (text only, no timestamps) to a file next
    /// to the script: `{stem}.output.txt`. The timestamps are a UI convenience;
    /// a saved file is raw script output for piping or comparison.
    pub(super) fn save_run_output(&mut self) {
        let Some(script) = &self.run_script else {
            return;
        };
        if self.run_output.is_empty() {
            self.logs.warn("no run output to save");
            return;
        }

        let stem = script
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "run".to_string());
        let dir = script.parent().unwrap_or_else(|| std::path::Path::new("."));
        let dest = dir.join(format!("{stem}.output.txt"));

        let content: String = self
            .run_output
            .iter()
            .map(|line| format!("{}\n", line.text))
            .collect();

        match std::fs::write(&dest, content) {
            Ok(()) => self
                .logs
                .success(format!("run output saved to {}", dest.display())),
            Err(e) => self.logs.error(format!("could not save run output: {e}")),
        }
    }

    /// Feeds a finished device `cat` into the open viewer, if it is still the
    /// one waiting on it --- matched by path, so a reply for a viewer the
    /// user already closed (or replaced by opening a different file) is
    /// dropped instead of overwriting the wrong content.
    ///
    /// For a plain device view the content becomes the viewer's lines
    /// directly; for a [`ViewerSource::Diff`] viewer it is diffed against the
    /// local copy (re-read here, since the file may have changed while the
    /// `cat` was in flight over serial).
    pub(super) fn apply_device_view(&mut self, view: DeviceView) {
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        // Resolve which source this reply is for, and --- for a diff --- the
        // local path to compare against, before mutably updating `state`.
        let diff_local = match &viewer.source {
            ViewerSource::Device(path) => {
                if *path != view.path {
                    return;
                }
                None
            }
            ViewerSource::Diff { local, device } => {
                if *device != view.path {
                    return;
                }
                Some(local.clone())
            }
            ViewerSource::Local(_) | ViewerSource::RunOutput(_) => return,
        };

        viewer.state = match (diff_local, view.content) {
            (None, Ok(content)) => ViewerState::Ready {
                lines: content.lines().map(str::to_string).collect(),
            },
            (None, Err(message)) | (Some(_), Err(message)) => ViewerState::Error(message),
            (Some(local_path), Ok(device_content)) => match files::read_text_file(&local_path) {
                Ok(local_content) => {
                    let lines = crate::diff::unified_diff(&local_content, &device_content);
                    // A `SameSize` entry can turn out byte-identical once both
                    // sides are fetched; an empty diff would render a blank
                    // viewer, so say so explicitly instead.
                    if lines.is_empty() {
                        ViewerState::Ready {
                            lines: vec!["(no differences — files are identical)".to_string()],
                        }
                    } else {
                        ViewerState::Ready { lines }
                    }
                }
                Err(message) => ViewerState::Error(message),
            },
        };
    }

    /// Reacts to a finished download or upload. A download queued by
    /// [`FileAction::Edit`] on a device file lands locally here --- queue
    /// `$EDITOR` on it. The re-upload that follows once `$EDITOR` closes
    /// (`App::request_device_reupload`) offers a restart on success. A plain
    /// download and an ordinary "Send to device" upload are fire-and-forget:
    /// their outcome is already in the log via `notices`, nothing further to
    /// do here.
    pub(super) fn apply_transfer(&mut self, transfer: Transfer) {
        let Transfer { kind, ok } = transfer;
        match kind {
            TransferKind::Download {
                local_path,
                then_edit: true,
                source,
            } if ok => {
                self.pending_edit = Some(PendingEdit {
                    path: local_path,
                    device_target: Some(source),
                });
            }
            TransferKind::Upload { after_edit: true } if ok => {
                self.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });
            }
            _ => {}
        }
    }

    /// Re-uploads `local_path` to `target` after `$EDITOR` closes
    /// successfully on a device file --- called by the binary, which is the
    /// only place that knows the editor actually ran and exited cleanly.
    pub fn request_device_reupload(&mut self, local_path: PathBuf, target: DevicePath) {
        self.dispatch_browser(|browser, processes, port| {
            browser.request_reupload_after_edit(local_path, target, processes, port)
        });
    }

    /// Restarts the device (`soft-reset`), once the user has explicitly
    /// confirmed it from [`Overlay::ConfirmRestartDevice`].
    pub(super) fn restart_device(&mut self) {
        self.dispatch_browser(|browser, processes, port| browser.request_reset(processes, port));
    }

    /// Runs the choice from [`Overlay::RestoreDeviceScript`]: bring the
    /// interrupted script back with a hard reset (clean state, board
    /// reboots), relaunch `main.py` without a reset (fast, but leftover
    /// state from the interrupted run survives), or leave the device
    /// stopped for the user to deal with.
    pub(super) fn apply_restore_device_script(&mut self, selected: usize) {
        match selected {
            0 => self.dispatch_browser(|browser, processes, port| {
                browser.request_hard_reset(processes, port)
            }),
            1 => self.dispatch_browser(|browser, processes, port| {
                browser.request_relaunch_main(processes, port)
            }),
            _ => {}
        }
    }

    /// Scrolls the open file viewer, clamped to the last position that still
    /// keeps the viewport full --- same shape as [`crate::logs::LogStore::scroll_up`], just
    /// counting down from the top instead of up from the tail. A no-op while
    /// [`ViewerState::Loading`] or [`ViewerState::Error`]: there is nothing to
    /// page through yet.
    pub(super) fn scroll_viewer(&mut self, delta: isize) {
        let viewport = self.viewer_viewport.max(1);
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        let ViewerState::Ready { lines } = &viewer.state else {
            return;
        };
        let max = lines.len().saturating_sub(viewport) as isize;
        let next = (viewer.scroll as isize + delta).clamp(0, max.max(0));
        viewer.scroll = next as usize;
    }

    /// Jumps the open file viewer straight to `target`, clamped the same way
    /// as [`Self::scroll_viewer`]. `usize::MAX` means "the end", mirroring
    /// [`Browser::cursor_to`]'s convention for the same key (`End`).
    pub(super) fn jump_viewer(&mut self, target: usize) {
        let viewport = self.viewer_viewport.max(1);
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        let ViewerState::Ready { lines } = &viewer.state else {
            return;
        };
        let max = lines.len().saturating_sub(viewport);
        viewer.scroll = target.min(max);
    }

    /// Takes the path queued by an `Edit` action, if any. The binary's event
    /// loop polls this once per iteration and, when it fires, suspends the
    /// terminal to run `$EDITOR` --- `App` has no terminal handle to do that
    /// itself.
    pub fn take_pending_edit(&mut self) -> Option<PendingEdit> {
        self.pending_edit.take()
    }

    /// The directory `$EDITOR` runs from: the project folder, so an editor
    /// whose file explorer follows its working directory opens straight on
    /// the project's files instead of wherever ChipTUI was launched. The
    /// Zephyr flow's project is the workspace pane's listed root (the build
    /// panel's root, which a pick can move mid-session); the browser's is
    /// the detection root --- the tree the local pane lists.
    pub fn editor_cwd(&self) -> PathBuf {
        if let Some(panel) = &self.workspace
            && !panel.files_root.as_os_str().is_empty()
        {
            return panel.files_root.clone();
        }
        self.manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf)
    }

    /// Takes the interactive command queued by the build panel's
    /// `menuconfig` action, if any: the event loop suspends the terminal and
    /// runs it attached to the real screen (the same contract as
    /// [`Self::take_pending_edit`], for a child that is itself a TUI).
    pub fn take_pending_command(&mut self) -> Option<crate::process::Command> {
        self.pending_command.take()
    }

    /// Re-reads the local pane after `$EDITOR` closes: size and contents may
    /// have changed under it while the terminal was suspended.
    pub fn reload_local_files(&mut self) {
        self.reload_local_pane();
    }
}

/// One action offered by [`Overlay::FileActions`] for the entry under the
/// cursor. The files pane is a sync tool, not a filesystem manager, so the
/// choices mirror that: move a copy across, or work with the copy already
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Descends into a directory --- the menu's default entry for one, so a
    /// reflex `Enter` twice still just browses in, one extra keypress from
    /// what a bare `Enter` used to do before directories gained the rest of
    /// this menu too.
    Open,
    SendToDevice,
    Download,
    /// Runs a local file on the device without copying it in
    /// (`mpremote run`) --- only ever offered on [`Side::Local`], since the
    /// underlying command takes a host path. See [`Capability::Run`].
    Run,
    View,
    Edit,
    /// Shows a unified diff of the local copy against the device copy in the
    /// file viewer --- only offered when both sides exist as text and the
    /// comparison verdict says they differ (or might, same size unchecked).
    Diff,
    Delete,
}

impl FileAction {
    /// Every label buffer-widths to the same 3 cells before its word: a
    /// genuinely wide emoji gets one space, a narrow glyph like `▶` gets two.
    /// Every emoji here is picked to be `Emoji_Presentation=Yes` on its own
    /// --- `🗑`/`👁` used to be forced wide with a trailing `\u{FE0F}`
    /// (VS16), the same trick `ui::files`'s file-listing icons used for
    /// `⚙️`, and for the same reason it was dropped there: `unicode-width`
    /// scores the VS16 sequence 2, but not every terminal's own font
    /// support agrees, so the two disagreeing about a glyph's width is a
    /// terminal-dependent bug no width math on this side can fix. A
    /// dedicated pictograph codepoint has no such disagreement to have.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "📂 Open",
            Self::SendToDevice => "📤 Send to device",
            Self::Download => "📥 Download",
            Self::Run => "▶  Run",
            Self::View => "🔍 View",
            Self::Edit => "📝 Edit",
            Self::Diff => "🔀 Diff",
            Self::Delete => "🚮 Delete",
        }
    }

    /// The actions offered for the entry under the cursor, in menu order.
    ///
    /// A directory gets `Open` first (descend), plus `Delete` and, when the
    /// backend can upload, `SendToDevice` --- never `View`/`Edit`/`Diff`,
    /// which need file contents. A file never offers `Open`; `View`/`Edit`
    /// appear only when `is_text` ([`crate::files::is_text_like`]) --- a binary
    /// file (e.g. a `.mpy`) can still be sent, downloaded and deleted, just
    /// not previewed or opened in `$EDITOR`.
    ///
    /// The transfer actions are capability-gated like `Run`, not just hidden
    /// by this menu's judgement: a backend without [`Capability::Upload`]
    /// offers no `SendToDevice`, and `Diff` needs
    /// [`Capability::Filesystem`] --- there is no second copy to compare
    /// against without one --- offered when `status` marks the entry as
    /// differing or as same-size but unchecked, since that is exactly when a
    /// content diff adds information the size markers cannot.
    pub fn for_entry(
        side: Side,
        is_dir: bool,
        is_text: bool,
        status: Option<SyncStatus>,
        capabilities: Capabilities,
    ) -> Vec<FileAction> {
        if is_dir {
            let mut actions = vec![Self::Open];
            match side {
                Side::Local if capabilities.contains(Capability::Upload) => {
                    actions.push(Self::SendToDevice);
                }
                Side::Device => actions.push(Self::Download),
                Side::Local => {}
            }
            actions.push(Self::Delete);
            actions
        } else {
            let mut actions = match side {
                Side::Local if capabilities.contains(Capability::Upload) => {
                    vec![Self::SendToDevice]
                }
                Side::Device => vec![Self::Download],
                Side::Local => Vec::new(),
            };
            if is_text {
                if side == Side::Local && capabilities.contains(Capability::Run) {
                    actions.push(Self::Run);
                }
                actions.push(Self::View);
                actions.push(Self::Edit);
                if capabilities.contains(Capability::Filesystem)
                    && matches!(
                        status,
                        Some(SyncStatus::Differs) | Some(SyncStatus::SameSize)
                    )
                {
                    actions.push(Self::Diff);
                }
            }
            actions.push(Self::Delete);
            actions
        }
    }
}

/// Contents behind [`Overlay::FileViewer`].
pub struct FileViewer {
    pub source: ViewerSource,
    pub state: ViewerState,
    /// Index of the first visible line, clamped in [`App::scroll_viewer`]
    /// against the viewport height the renderer publishes each frame
    /// (mirrors [`App::log_viewport`]).
    pub scroll: usize,
}

impl FileViewer {
    /// Name to detect a syntax-highlighting language from --- the file name
    /// alone either way, since a device file has no local path to draw one
    /// from.
    pub fn display_name(&self) -> String {
        match &self.source {
            ViewerSource::Local(path) => path.display().to_string(),
            ViewerSource::Device(path) => path.to_string(),
            // Deliberately not `{path}.output` or anything else ending the
            // string in the script's own extension: `highlight::Language::
            // from_filename` keys off the text after the last '.', and this
            // is plain captured output, not Python source, so it must not
            // look like a `.py` file to it.
            ViewerSource::RunOutput(path) => format!("{} — output", path.display()),
            ViewerSource::Diff { local, .. } => {
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("Diff: {name}  (local ↔ device)")
            }
        }
    }
}

/// Where a viewer's content came from. A local read is synchronous
/// ([`App::open_local_file_viewer`] fills in [`ViewerState`] immediately); a
/// device `cat` is not, so a device-sourced viewer starts in
/// [`ViewerState::Loading`] and is updated once [`crate::browser::DeviceView`]
/// arrives (`App::apply_device_view`, matched by path so a stale reply for a
/// viewer the user already closed and reopened elsewhere is dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerSource {
    Local(PathBuf),
    Device(DevicePath),
    /// Captured stdout of a local script run on the device
    /// (`FileAction::Run`), keyed by the local script's path.
    RunOutput(PathBuf),
    /// A unified diff of the local copy (`local`) against the device copy
    /// (`device`). Like [`Self::Device`], the device half arrives
    /// asynchronously via a `cat`: the viewer opens in
    /// [`ViewerState::Loading`] and [`App::apply_device_view`] computes the
    /// diff once the device content lands.
    Diff {
        local: PathBuf,
        device: DevicePath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerState {
    Loading,
    Ready {
        lines: Vec<String>,
    },
    /// The file could not be shown (binary, too large, unreadable, or the
    /// device `cat` failed) --- the reason is already in the log too.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::Side;

    /// Every `FileAction` label must buffer-width to 3 cells before its
    /// word --- the padding a wide (2-cell) icon needs is one space less than
    /// a narrow (1-cell) one. This is what a `🗑`/`👁` missing `\u{FE0F}`
    /// breaks: `unicode_width` scores them narrow while most emoji fonts
    /// draw them wide, so the single-space padding used for genuinely wide
    /// icons quietly desyncs the menu from the terminal.
    #[test]
    fn every_file_action_label_budgets_the_same_column() {
        use ratatui::text::Span;

        for action in [
            FileAction::Open,
            FileAction::SendToDevice,
            FileAction::Download,
            FileAction::Run,
            FileAction::View,
            FileAction::Edit,
            FileAction::Diff,
            FileAction::Delete,
        ] {
            let label = action.label();
            let word_start = label
                .char_indices()
                .find(|(_, c)| c.is_alphabetic())
                .map(|(i, _)| i)
                .expect("label has a word");
            let prefix_width = Span::raw(&label[..word_start]).width();
            assert_eq!(
                prefix_width, 3,
                "{label:?} budgets {prefix_width} cells before its word, want 3"
            );
        }
    }

    #[test]
    fn file_actions_without_upload_or_filesystem_are_purely_local() {
        use crate::backend::Backend as _;
        // Zephyr's real capability set: the local pane offers exactly
        // open/view/edit/delete, nothing device-bound.
        let caps = crate::backend::zephyr::ZephyrBackend.capabilities();
        assert_eq!(
            FileAction::for_entry(Side::Local, false, true, None, caps),
            vec![FileAction::View, FileAction::Edit, FileAction::Delete]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, false, false, None, caps),
            vec![FileAction::Delete]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, true, false, None, caps),
            vec![FileAction::Open, FileAction::Delete]
        );
        // Even a differing verdict cannot offer a diff without a filesystem:
        // there is no second copy to compare against.
        assert!(
            !FileAction::for_entry(Side::Local, false, true, Some(SyncStatus::Differs), caps)
                .contains(&FileAction::Diff)
        );
    }

    #[test]
    fn file_actions_keep_transfers_under_upload_and_filesystem() {
        use crate::backend::Backend as _;
        // MicroPython's real capability set: unchanged behavior.
        let caps = crate::backend::micropython::MicroPythonBackend.capabilities();
        assert_eq!(
            FileAction::for_entry(Side::Local, true, false, None, caps),
            vec![
                FileAction::Open,
                FileAction::SendToDevice,
                FileAction::Delete
            ]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, false, true, Some(SyncStatus::Differs), caps),
            vec![
                FileAction::SendToDevice,
                FileAction::Run,
                FileAction::View,
                FileAction::Edit,
                FileAction::Diff,
                FileAction::Delete
            ]
        );
    }
}
