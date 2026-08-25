//! The package manager (`Overlay::Packages`): one filterable list over
//! three sources at once --- what `requirements.txt` declares, what the
//! board's `/lib` actually holds, and the micropython-lib index
//! ([`crate::backend::micropython::packages`], fetched once per session
//! through `curl`, the same delegation the firmware pages already use and
//! never a bundled HTTP client).
//!
//! Merging them is the whole point: a search that only *added* could not
//! say what was already declared, could not show what the board carries
//! that the file forgot, and had no way to remove either. Every action
//! writes through `requirements.txt`, so the file the Dependencies row
//! reports coverage for stays the single source of truth.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::micropython::curl;
use crate::backend::micropython::deps;
use crate::backend::micropython::packages;
use crate::process::{ProcessEvent, ProcessId, Stream};

use super::{App, Overlay};

/// How long the one-shot index fetch may take before it is cancelled ---
/// the same ceiling the firmware page searches allow themselves.
const FETCH_TIMEOUT: std::time::Duration = crate::flash::FETCH_TIMEOUT;

/// The declaration file, at the project root (the scaffold's own placement).
pub const REQUIREMENTS_FILE: &str = "requirements.txt";

/// What the device runs at boot, in the order it runs them --- the two
/// names the Project pane's boot report compares.
pub const BOOT_FILES: [&str; 2] = ["boot.py", "main.py"];

/// The session's copy of the micropython-lib index, or where its fetch
/// stands. Ready answers keep for the whole session: ~130 entries, and a
/// stale `version` field costs the picker a label, never an install (mip
/// itself resolves the newest installable version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageIndex {
    Idle,
    Fetching { id: ProcessId, stdout: String },
    Ready(Vec<packages::Package>),
    Failed(String),
}

/// The project's `requirements.txt`, read off the tick rather than the
/// draw path.
///
/// The Dependencies row and the manager both need the file's contents
/// every frame, and both used to `read_to_string` it inside the renderer
/// --- a syscall per frame, per consumer. The file is editable from
/// outside the program, so re-reading it *is* right; doing it 60 times a
/// second is not. This polls on the same 1 s cadence
/// `App::refresh_local_listings` uses for the same reason.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RequirementsCache {
    /// Which file the contents belong to, so a project switch cannot serve
    /// the previous project's declarations.
    path: std::path::PathBuf,
    text: String,
    /// Modification time and length --- the change test. An unreadable
    /// file is never a change, so a transient failure cannot blank the row
    /// (`files::listing_changed`'s own rule).
    stamp: Option<(std::time::SystemTime, u64)>,
}

impl RequirementsCache {
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The manager's keyboard state. It lives on [`App`] rather than inside
/// the overlay variant because the remove confirmation *replaces* the
/// overlay (the slot is one deep), and the window has to come back
/// afterwards exactly as it was --- filter text, cursor and focus.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackagesState {
    /// The filter line, which doubles as the manual-spec field.
    pub input: String,
    pub selected: usize,
    /// Which half `Tab` gave the keyboard to --- the board/shield pickers'
    /// grammar, reused rather than reinvented.
    pub focus: super::DocsFocus,
    /// Scroll offset of the details pane, when it holds the keyboard.
    pub scroll: u16,
}

/// What one row of the manager stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// Install every specification the file declares, in one `mip install`
    /// --- a row rather than a key, because the filter line owns every
    /// printable character.
    InstallAll { total: usize, installed: usize },
    /// Install what the filter line literally says: a `github:`/URL spec,
    /// or a name the index does not carry. The escape hatch a
    /// filter-with-no-matches used to lack entirely.
    ManualSpec(String),
    /// A package: declared, installed, in the index, or any combination.
    Package(PackageRow),
}

/// A package as the manager sees it, from all three sources at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRow {
    /// The package name, or the whole specification for a `github:`/URL
    /// line (which names no `/lib` entry the line itself can predict).
    pub name: String,
    /// The specification exactly as `requirements.txt` writes it.
    pub spec: Option<String>,
    /// Whether `/lib` carries it --- `Unknown` while the directory it
    /// would live in has not been listed.
    pub installed: deps::Installed,
    /// Version and description from the index, when it lists the package.
    pub version: Option<String>,
    pub description: Option<String>,
}

impl PackageRow {
    pub const fn declared(&self) -> bool {
        self.spec.is_some()
    }

    /// The row's mark, in the checklist vocabulary the Project pane uses
    /// (state-carrying glyphs, so no [`crate::icons::IconSet`] reaches
    /// them).
    pub const fn mark(&self) -> &'static str {
        match (self.declared(), self.installed) {
            (true, deps::Installed::Yes) => "✓",
            (true, deps::Installed::No) => "□",
            (true, deps::Installed::Unknown) => "·",
            // On the board and the file does not know: worth saying so,
            // since the next `mip install -r` would not reinstall it.
            (false, deps::Installed::Yes) => "⚠",
            (false, _) => " ",
        }
    }

    /// One-line explanation of the mark, for the details pane.
    pub const fn state(&self) -> &'static str {
        match (self.declared(), self.installed) {
            (true, deps::Installed::Yes) => "declared and installed",
            (true, deps::Installed::No) => "declared, not installed",
            (true, deps::Installed::Unknown) => "declared; not checked yet",
            (false, deps::Installed::Yes) => "on the board, not declared",
            (false, _) => "available from micropython-lib",
        }
    }
}

impl App {
    /// Opens the package manager. Starts the index fetch when there is
    /// nothing to show: a previous failure retries --- the network being
    /// back is exactly when the user tries again.
    ///
    /// The filter and cursor are reset, but the *index* is not refetched
    /// once it is `Ready`: ~130 entries, and a stale `version` costs a
    /// label, never an install (mip resolves the newest itself).
    pub(super) fn open_package_manager(&mut self) {
        self.packages = PackagesState::default();
        self.overlay = Some(Overlay::Packages);
        if matches!(
            self.package_index,
            PackageIndex::Idle | PackageIndex::Failed(_)
        ) {
            self.start_package_index_fetch();
        }
    }

    /// The manager's state, borrowed for the row model and the renderer.
    pub fn packages_state(&self) -> &PackagesState {
        &self.packages
    }

    /// Spawns the `curl` fetch of the index listing, availability-checked
    /// first so a missing `curl` is a named reason rather than a spawn
    /// failure --- the same guard `FlashPanel::search_online` applies.
    fn start_package_index_fetch(&mut self) {
        if !self.package_curl_available() {
            self.package_index = PackageIndex::Failed(
                "curl is not on PATH — install it to search micropython-lib".to_string(),
            );
            return;
        }
        let command = match &self.package_curl_path {
            Some(program) => {
                curl::commands::fetch_page(packages::INDEX_URL).with_program(program.clone())
            }
            None => curl::commands::fetch_page(packages::INDEX_URL),
        };
        let id = self.processes.spawn(command, FETCH_TIMEOUT);
        self.package_index = PackageIndex::Fetching {
            id,
            stdout: String::new(),
        };
    }

    fn package_curl_available(&self) -> bool {
        self.package_curl_path.as_deref().map_or_else(
            || crate::backend::tool_available(curl::commands::PROGRAM),
            |path| crate::backend::executable_at(std::path::Path::new(path)),
        )
    }

    /// The test seam pointing the index fetch at a fake `curl` --- the same
    /// trade-off `FlashPanel::set_curl_tool_path` makes.
    pub fn set_package_curl_tool_path(&mut self, program: impl Into<String>) {
        self.package_curl_path = Some(program.into());
    }

    /// The manager's rows for the current filter, merged from the file,
    /// `/lib` and the index --- computed, never cached, so the list cannot
    /// disagree with any of the three.
    ///
    /// Order is by usefulness, not alphabet: the action rows first, then
    /// what the project declares (the file is its source of truth), then
    /// what the board carries undeclared, then the rest of the catalogue.
    pub fn package_rows(&self) -> Vec<RowKind> {
        let state = self.packages_state();
        let filter = state.input.trim().to_lowercase();
        let specs = deps::parse_requirements(&self.requirements_text());

        let lookup = |path: &crate::device::DevicePath| {
            self.browser
                .as_ref()
                .and_then(|browser| browser.cached_listing(path))
                .map(<[_]>::to_vec)
        };
        let index = match &self.package_index {
            PackageIndex::Ready(index) => index.as_slice(),
            _ => &[],
        };
        let described = |name: &str| index.iter().find(|package| package.name == name);

        // 1. Everything the file declares, in the file's own order.
        let mut rows: Vec<PackageRow> = Vec::new();
        for spec in &specs {
            let name = deps::spec_name(spec).unwrap_or(spec).to_string();
            if rows.iter().any(|row| row.name == name) {
                continue;
            }
            let entry = described(&name);
            rows.push(PackageRow {
                installed: if deps::spec_name(spec).is_some() {
                    deps::installed(&name, &lookup)
                } else {
                    // A `github:`/URL line names no `/lib` entry the line
                    // itself can predict --- mip derives it from the remote
                    // manifest, so the answer is honestly unknown.
                    deps::Installed::Unknown
                },
                name,
                spec: Some(spec.clone()),
                version: entry.map(|package| package.version.clone()),
                description: entry.map(|package| package.description.clone()),
            });
        }

        // 2. What `/lib` carries that the file does not mention --- minus
        // the *namespace* directories a dotted package lives under.
        // `umqtt.simple` puts `umqtt/` there, and `umqtt` is no package:
        // offering it as one invited a recursive delete that would take
        // every sibling package with it.
        let namespaces: Vec<String> = specs
            .iter()
            .filter_map(|spec| deps::spec_name(spec))
            .filter_map(|name| {
                let target = deps::lib_target(name);
                (target.dir.as_str() != deps::LIB_ROOT).then(|| target.dir.name().to_string())
            })
            .collect();
        let lib = crate::device::DevicePath::new(deps::LIB_ROOT);
        for name in lookup(&lib)
            .unwrap_or_default()
            .iter()
            .filter_map(installed_name)
        {
            if rows.iter().any(|row| row.name == name) || namespaces.contains(&name) {
                continue;
            }
            let entry = described(&name);
            rows.push(PackageRow {
                name,
                spec: None,
                installed: deps::Installed::Yes,
                version: entry.map(|package| package.version.clone()),
                description: entry.map(|package| package.description.clone()),
            });
        }
        let declared_or_installed = rows.len();

        // 3. The rest of the catalogue.
        for package in index {
            if rows.iter().any(|row| row.name == package.name) {
                continue;
            }
            rows.push(PackageRow {
                name: package.name.clone(),
                spec: None,
                installed: deps::Installed::No,
                version: Some(package.version.clone()),
                description: Some(package.description.clone()),
            });
        }
        rows[..declared_or_installed].sort_by(|a, b| a.name.cmp(&b.name));

        let matching: Vec<PackageRow> = rows
            .into_iter()
            .filter(|row| matches_filter(row, &filter))
            .collect();

        // The action rows ride on top, and only when they mean something.
        let mut out: Vec<RowKind> = Vec::new();
        if let Some(spec) = self.manual_spec(&matching) {
            out.push(RowKind::ManualSpec(spec));
        }
        let coverage = deps::coverage(&specs, &lookup);
        if coverage.total > 0 && !coverage.is_complete() {
            out.push(RowKind::InstallAll {
                total: coverage.total,
                installed: coverage.installed,
            });
        }
        out.extend(matching.into_iter().map(RowKind::Package));
        out
    }

    /// The filter text offered as a literal specification, when that is
    /// what the user seems to be typing: a `github:`/`gitlab:`/URL form
    /// (a `:` or `/`, the same shape [`deps::parse_requirements`] passes
    /// through verbatim), or anything at all once the filter matches
    /// nothing --- which is precisely the dead end the old search had.
    fn manual_spec(&self, matching: &[PackageRow]) -> Option<String> {
        let typed = self.packages_state().input.trim();
        if typed.is_empty() {
            return None;
        }
        let looks_like_a_spec = typed.contains(':') || typed.contains('/');
        let already_a_row = matching
            .iter()
            .any(|row| row.name.eq_ignore_ascii_case(typed));
        (!already_a_row && (looks_like_a_spec || matching.is_empty())).then(|| typed.to_string())
    }

    /// Feeds a process event into the index fetch. Returns whether the
    /// event was consumed: the fetch's id is matched before any other
    /// subsystem sees the event, so nobody logs someone else's exit.
    pub(super) fn on_package_index_process(&mut self, event: &ProcessEvent) -> bool {
        let fetching_id = match &self.package_index {
            PackageIndex::Fetching { id, .. } => Some(*id),
            _ => None,
        };
        let Some(fetching_id) = fetching_id else {
            return false;
        };
        match event {
            ProcessEvent::Line {
                id,
                stream: Stream::Stdout,
                text,
            } if *id == fetching_id => {
                if let PackageIndex::Fetching { stdout, .. } = &mut self.package_index {
                    stdout.push_str(text);
                    stdout.push('\n');
                }
                true
            }
            ProcessEvent::Finished { id, outcome, .. } if *id == fetching_id => {
                let fetch = std::mem::replace(&mut self.package_index, PackageIndex::Idle);
                let PackageIndex::Fetching { stdout, .. } = fetch else {
                    return true;
                };
                let parsed = packages::parse_index(&stdout);
                match outcome {
                    crate::process::Outcome::Success if !parsed.is_empty() => {
                        let count = parsed.len();
                        self.package_index = PackageIndex::Ready(parsed);
                        self.logs
                            .info(format!("micropython-lib index loaded: {count} packages"));
                    }
                    crate::process::Outcome::Success => {
                        self.package_index = PackageIndex::Failed(
                            "the package index could not be read — try again shortly".to_string(),
                        );
                    }
                    _ => {
                        self.package_index = PackageIndex::Failed(format!(
                            "could not fetch the package index ({}): try again shortly",
                            outcome.summary()
                        ));
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Keys for the open manager.
    ///
    /// The filter line is free text, so **no action lives on a plain
    /// letter** --- the same constraint that makes `?` filter instead of
    /// opening help (`is_text_entry_overlay`). Removal is `Del`, the
    /// gesture the file panes already use, and "install everything" is a
    /// row of the list rather than a key. The vim `j`/`k` arms the search
    /// once had are gone with it: they made `json` and `keyboard`
    /// untypeable.
    pub(super) fn on_packages_key(&mut self, key: KeyEvent) {
        let rows = self.package_rows();
        let len = rows.len();
        let details = self.packages.focus == super::DocsFocus::Details;
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
            }
            KeyCode::Tab => self.packages.focus = self.packages.focus.toggled(),
            KeyCode::Backspace => {
                self.packages.input.pop();
                self.packages.selected = 0;
            }
            // With the details pane focused the arrows scroll it; otherwise
            // they walk the list. The board/shield pickers' own split.
            KeyCode::Up if details => {
                self.packages.scroll = self.packages.scroll.saturating_sub(1);
            }
            KeyCode::Down if details => self.packages.scroll += 1,
            KeyCode::PageUp if details => {
                self.packages.scroll = self.packages.scroll.saturating_sub(10);
            }
            KeyCode::PageDown if details => self.packages.scroll += 10,
            KeyCode::Up => self.move_package_cursor(-1, len),
            KeyCode::Down => self.move_package_cursor(1, len),
            KeyCode::PageUp => self.move_package_cursor(-5, len),
            KeyCode::PageDown => self.move_package_cursor(5, len),
            KeyCode::Home => self.move_package_cursor(i32::MIN, len),
            KeyCode::End => self.move_package_cursor(i32::MAX, len),
            KeyCode::Delete => {
                if let Some(RowKind::Package(row)) = rows.get(self.packages.selected) {
                    let row = row.clone();
                    self.ask_remove_package(&row);
                }
            }
            KeyCode::Enter => {
                if let Some(row) = rows.get(self.packages.selected) {
                    let row = row.clone();
                    self.activate_package_row(&row);
                }
            }
            // Every other printable character filters --- including the
            // ones a Ctrl chord would otherwise smuggle in as text.
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.packages.input.push(c);
                self.packages.selected = 0;
                self.packages.scroll = 0;
            }
            _ => {}
        }
    }

    fn move_package_cursor(&mut self, by: i32, len: usize) {
        if len == 0 {
            self.packages.selected = 0;
            return;
        }
        let last = len - 1;
        let next = match by {
            i32::MIN => 0,
            i32::MAX => last,
            step if step < 0 => self
                .packages
                .selected
                .saturating_sub(step.unsigned_abs() as usize),
            step => (self.packages.selected + step as usize).min(last),
        };
        self.packages.selected = next;
        self.packages.scroll = 0;
    }

    /// `Enter` on the row under the cursor.
    ///
    /// A package row installs, declaring it first when the file does not
    /// already --- unless it is *already* installed (`✓`/`⚠`), in which
    /// case `Enter` asks to remove it instead, the same confirmation `Del`
    /// opens. Re-running an install mip would just skip is not a gesture
    /// worth a whole key; uninstalling is the thing the user cannot reach
    /// any other way from an already-installed row.
    pub(super) fn activate_package_row(&mut self, row: &RowKind) {
        match row {
            RowKind::InstallAll { .. } => self.install_project_dependencies(),
            RowKind::ManualSpec(spec) => {
                let spec = spec.clone();
                self.declare_and_install(&spec);
            }
            RowKind::Package(package) if package.installed == deps::Installed::Yes => {
                self.ask_remove_package(package);
            }
            RowKind::Package(package) => {
                let spec = package.spec.clone().unwrap_or_else(|| package.name.clone());
                self.declare_and_install(&spec);
            }
        }
    }

    /// Writes `spec` into `requirements.txt` (unless an identical line is
    /// already there) and installs it. The file is written *first*: it is
    /// the project's record, and it stays true whether or not a board is
    /// plugged in.
    fn declare_and_install(&mut self, spec: &str) {
        let path = self.requirements_path();
        let text = self.requirements_text();
        // `None` means the line is already there exactly as typed: install
        // it anyway, which is what the user just asked for.
        if let Some(updated) = deps::add_line(&text, spec) {
            match std::fs::write(&path, &updated) {
                Ok(()) => {
                    self.reload_requirements();
                    self.logs
                        .success(format!("{spec} added to {}", path.display()));
                }
                Err(error) => {
                    self.logs
                        .error(format!("{}: could not write it: {error}", path.display()));
                    return;
                }
            }
        }
        let specs = vec![spec.to_string()];
        self.dispatch_browser(|browser, processes, port| {
            browser.request_mip_install(&specs, processes, port)
        });
    }

    /// Opens the removal confirmation for `row`, resolving what the
    /// package actually occupies under `/lib` first --- only paths the
    /// listing shows are offered for deletion, and a dotted name's leaf is
    /// removed without touching the directory it shares with siblings.
    fn ask_remove_package(&mut self, row: &PackageRow) {
        let targets = self.lib_targets_of(&row.name);
        if targets.is_empty() && !row.declared() {
            self.logs.warn(format!("{}: nothing to remove", row.name));
            return;
        }
        self.overlay = Some(Overlay::ConfirmRemovePackage {
            name: row.name.clone(),
            targets,
            declared: row.declared(),
            confirm: false,
        });
    }

    /// The paths `name` occupies under `/lib`, as the cached listings show
    /// them --- each with whether it needs a recursive `rm` (a package
    /// directory does; a single-file module does not).
    fn lib_targets_of(&self, name: &str) -> Vec<(crate::device::DevicePath, bool)> {
        let target = deps::lib_target(name);
        let Some(entries) = self
            .browser
            .as_ref()
            .and_then(|browser| browser.cached_listing(&target.dir))
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|entry| target.candidates.contains(&entry.name))
            .map(|entry| (target.dir.join(&entry.name), entry.is_dir))
            .collect()
    }

    /// Carries out an accepted removal: the file's line goes first (always
    /// honourable, board or no board), then each `/lib` path through the
    /// browser's own queue.
    pub(super) fn remove_package(
        &mut self,
        name: &str,
        targets: &[(crate::device::DevicePath, bool)],
        declared: bool,
    ) {
        if declared {
            let path = self.requirements_path();
            let text = self.requirements_text();
            // `None` means no line declared it --- nothing to rewrite.
            if let Some(updated) = deps::remove_line(&text, name) {
                match std::fs::write(&path, &updated) {
                    Ok(()) => {
                        self.reload_requirements();
                        self.logs
                            .success(format!("{name} dropped from {}", path.display()));
                    }
                    Err(error) => self
                        .logs
                        .error(format!("{}: could not write it: {error}", path.display())),
                }
            }
        }
        if targets.is_empty() {
            self.logs
                .info(format!("{name}: nothing installed on the board to delete"));
            return;
        }
        for (path, recursive) in targets {
            let (path, recursive) = (path.clone(), *recursive);
            self.dispatch_browser(move |browser, processes, port| {
                browser.request_remove_path(path, recursive, processes, port)
            });
        }
    }

    /// Where the declaration file lives.
    pub(super) fn requirements_path(&self) -> std::path::PathBuf {
        self.mpy_effective_root().join(REQUIREMENTS_FILE)
    }

    /// A device listing just settled. Two follow-ups hang off it, both
    /// silent and both once-per-connection (the cache they check is cleared
    /// exactly when their answers go stale --- device change, reset,
    /// install, uninstall):
    ///
    /// - the **root** listing arms the boot files' sha256 comparison, so
    ///   the Project pane's row can reach a real `=`/`≠` instead of sitting
    ///   on the `≈` a size match alone produces;
    /// - the **`/lib`** listing arms a sub-listing per dotted package name
    ///   the requirements declare, since `umqtt.simple` lands in
    ///   `/lib/umqtt/` and the cache holds one directory level at a time.
    pub(super) fn on_device_listing(&mut self, path: &crate::device::DevicePath) {
        if path.is_root() {
            self.arm_boot_file_hashes();
        }
        if *path == crate::device::DevicePath::new(deps::LIB_ROOT) {
            self.arm_lib_subdirectory_listings();
        }
    }

    /// Queues a silent sha256 of `boot.py`/`main.py` for the two the device
    /// root and the project's sync root both hold as files of the same
    /// size --- the only case where the verdict is still open. A size
    /// mismatch is already a definite `≠`, and a file only one side has
    /// needs no digest at all.
    fn arm_boot_file_hashes(&mut self) {
        let root = crate::device::DevicePath::root();
        let local_dir = self.mpy_sync_root();
        let Some(browser) = self.browser.as_ref() else {
            return;
        };
        let Some(remote) = browser.cached_listing(&root).map(<[_]>::to_vec) else {
            return;
        };
        let local = crate::files::read_dir(&local_dir).unwrap_or_default();
        let statuses = crate::files::compare(&local, &remote, browser.verdicts_for(&root));
        let pending: Vec<String> = BOOT_FILES
            .iter()
            .filter(|name| statuses.get(**name) == Some(&crate::files::SyncStatus::SameSize))
            .map(|name| (*name).to_string())
            .collect();
        for name in pending {
            let local_file = local_dir.join(&name);
            let root = root.clone();
            self.dispatch_browser(move |browser, processes, port| {
                browser.request_background_hash(root, &name, &local_file, processes, port)
            });
        }
    }

    /// Queues a listing of each package directory a dotted requirement
    /// would live in, but only for one the `/lib` listing shows is actually
    /// there ([`deps::pending_listing`]) --- an absent directory is already
    /// a definite "not installed" and must not cost a serial round-trip.
    fn arm_lib_subdirectory_listings(&mut self) {
        let specs = deps::parse_requirements(&self.requirements_text());
        let Some(browser) = self.browser.as_ref() else {
            return;
        };
        let lookup =
            |path: &crate::device::DevicePath| browser.cached_listing(path).map(<[_]>::to_vec);
        let mut wanted: Vec<crate::device::DevicePath> = Vec::new();
        for spec in &specs {
            let Some(name) = deps::spec_name(spec) else {
                continue;
            };
            if let Some(dir) = deps::pending_listing(name, &lookup)
                && !wanted.contains(&dir)
            {
                wanted.push(dir);
            }
        }
        for dir in wanted {
            self.dispatch_browser(move |browser, processes, port| {
                browser.request_background_list(dir, processes, port)
            });
        }
    }

    /// The project's `requirements.txt` as text, or empty when there is
    /// none --- served from the tick-refreshed cache, never read here.
    pub(super) fn requirements_text(&self) -> String {
        self.requirements.text().to_string()
    }

    /// Re-reads `requirements.txt` when it looks changed. Called from the
    /// tick beside [`App::refresh_local_listings`], on the same cadence and
    /// with the same rule: an unreadable file is not a change.
    pub(super) fn refresh_requirements(&mut self) {
        if !self.ticks.is_multiple_of(4) {
            return;
        }
        self.reload_requirements_if_stale();
    }

    /// Reads the file when the path or the stamp moved. Called eagerly
    /// after our own writes and on a project switch, where waiting up to a
    /// second for the tick would show the user their own edit late.
    pub(super) fn reload_requirements(&mut self) {
        self.requirements.stamp = None;
        self.requirements.path = std::path::PathBuf::new();
        self.reload_requirements_if_stale();
    }

    fn reload_requirements_if_stale(&mut self) {
        let path = self.requirements_path();
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if path == self.requirements.path && stamp == self.requirements.stamp {
            return;
        }
        self.requirements.path = path.clone();
        self.requirements.stamp = stamp;
        self.requirements.text = std::fs::read_to_string(&path).unwrap_or_default();
    }

    /// Creates a missing `requirements.txt` from the shared template --- the
    /// Dependencies row's `Enter` when nothing exists yet, and the answer it
    /// pairs with: the search opens over the fresh file so the first pick
    /// has somewhere to land.
    pub(super) fn create_requirements_file(&mut self) -> Option<std::path::PathBuf> {
        let path = self.mpy_effective_root().join("requirements.txt");
        let contents = crate::backend::micropython::deps::REQUIREMENTS_TEMPLATE;
        match std::fs::write(&path, contents) {
            Ok(()) => {
                self.reload_requirements();
                self.logs.success(format!("created {}", path.display()));
                Some(path)
            }
            Err(error) => {
                self.logs
                    .error(format!("{}: could not create it: {error}", path.display()));
                None
            }
        }
    }
}

/// The package name a `/lib` entry stands for: a directory is a package,
/// a `.py`/`.mpy` file is a single-file module, and anything else (a data
/// file someone copied in) belongs to no package.
fn installed_name(entry: &crate::backend::micropython::parse::RemoteEntry) -> Option<String> {
    if entry.is_dir {
        return Some(entry.name.clone());
    }
    let name = entry
        .name
        .strip_suffix(".mpy")
        .or_else(|| entry.name.strip_suffix(".py"))?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Case-insensitive substring over name and description --- `packages::search`'s
/// rule, applied to the merged row instead of the index entry.
fn matches_filter(row: &PackageRow, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    row.name.to_lowercase().contains(filter)
        || row
            .description
            .as_ref()
            .is_some_and(|text| text.to_lowercase().contains(filter))
}
