//! Dashboard file-browser dispatch: navigation, the per-file action menu,
//! the local/device viewer, and the transfers they trigger. Split out of
//! `app.rs` since it is the one subsystem `App` drives almost entirely
//! through [`crate::browser::Browser`] and never touches [`crate::flash`].

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::Capability;
use crate::backend::micropython::commands;
use crate::browser::{Browser, DeviceView, Notice, Side, Transfer, TransferKind};
use crate::device::DevicePath;
use crate::files;
use crate::process::ProcessManager;

use super::{
    App, FileAction, FileViewer, Overlay, PendingEdit, PendingMonitor, RunState, ViewerSource,
    ViewerState,
};

impl App {
    /// Handles a key while [`super::Focus::FilesLocal`]/[`super::Focus::FilesDevice`] holds
    /// focus. `Tab`/`BackTab`, `o`, `x`, `?` and `d` are dashboard-wide and
    /// already handled by [`App::on_dashboard_key`] before this is reached,
    /// so only the file browser's own navigation remains here.
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
        let port = self.devices.selected_port().map(str::to_string);
        let port = port.as_deref();

        let notices = match key.code {
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
            // Left/right stay pure navigation, independent of the action
            // menu: `→` descends into a directory directly (a no-op on a
            // file, same as `Browser::enter` already guards), `←`/Backspace
            // goes back up.
            KeyCode::Right => browser.enter(&mut self.processes, port),
            KeyCode::Backspace | KeyCode::Left => browser.ascend(&mut self.processes, port),
            KeyCode::Char('r') => {
                if browser.focus == Side::Device {
                    browser.load_device(&mut self.processes, port, true)
                } else {
                    browser.reload_local();
                    Vec::new()
                }
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
            KeyCode::Char('c') => browser.verify_selected(&mut self.processes, port),
            KeyCode::Char('S') => browser.request_sync(&mut self.processes, port),
            KeyCode::Char('i')
                if browser.focus == Side::Device
                    && self
                        .manager
                        .capabilities()
                        .contains(Capability::PackageInstall) =>
            {
                self.overlay = Some(Overlay::PackageInstall {
                    input: String::new(),
                });
                Vec::new()
            }
            _ => Vec::new(),
        };

        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
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
            // do directly, now one keypress later.
            (_, FileAction::Open) => {
                self.dispatch_browser(|browser, processes, port| browser.enter(processes, port));
            }
            (_, FileAction::Diff) => self.open_diff(name),
            (Side::Local, FileAction::View) => {
                let Some(browser) = &self.browser else { return };
                self.open_local_file_viewer(browser.local_path.join(name));
            }
            (Side::Local, FileAction::Edit) => {
                let Some(browser) = &self.browser else { return };
                self.pending_edit = Some(PendingEdit {
                    path: browser.local_path.join(name),
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
                if let Some(browser) = &mut self.browser {
                    let path = browser.local_path.join(name);
                    match std::fs::remove_file(&path) {
                        Ok(_) => {
                            self.logs.success(format!("{} removed", path.display()));
                            browser.reload_local();
                        }
                        Err(e) => {
                            self.logs
                                .error(format!("{}: remove failed: {e}", path.display()));
                        }
                    }
                }
            }
            (Side::Local, true) => {
                if let Some(browser) = &mut self.browser {
                    let path = browser.local_path.join(name);
                    match std::fs::remove_dir_all(&path) {
                        Ok(_) => {
                            self.logs.success(format!("{} removed", path.display()));
                            browser.reload_local();
                        }
                        Err(e) => {
                            self.logs
                                .error(format!("{}: remove failed: {e}", path.display()));
                        }
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
                let Some(browser) = &mut self.browser else {
                    return;
                };
                let path = browser.local_path.join(name);
                let result = if is_dir {
                    std::fs::create_dir(&path)
                } else {
                    std::fs::File::create_new(&path).map(|_| ())
                };
                match result {
                    Ok(()) => {
                        self.logs.success(format!("{} created", path.display()));
                        browser.reload_local();
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

    /// Runs the package-install prompt (`i` on the device pane): queues
    /// `mip install` for the typed package name/spec through [`Browser`],
    /// like every other action here that touches the port.
    pub(super) fn install_package(&mut self, input: &str) {
        let package = input.trim();
        if package.is_empty() {
            self.logs.warn("type a package name first");
            return;
        }
        self.dispatch_browser(|browser, processes, port| {
            browser.request_mip_install(package, processes, port)
        });
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
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = f(&mut browser, &mut self.processes, port.as_deref());
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
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
        if self.device_monitor_process.is_some() {
            self.logs
                .warn("close the monitor/REPL before running a script");
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
        self.monitor_source = super::MonitorSource::Run;
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

    pub fn take_pending_monitor(&mut self) -> Option<PendingMonitor> {
        self.pending_monitor.take()
    }

    /// Re-reads the local pane after `$EDITOR` closes: size and contents may
    /// have changed under it while the terminal was suspended.
    pub fn reload_local_files(&mut self) {
        if let Some(browser) = &mut self.browser {
            browser.reload_local();
        }
    }
}
