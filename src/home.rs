//! The home screen: which project to open.
//!
//! `SPEC.md` §7. It appears when the working directory answers nothing
//! ([`crate::startup::route`]) and whenever the user asks to switch
//! projects, and it is backed entirely by the user config's project registry
//! ([`crate::settings::ProjectRegistry`]) --- the answer ChipTUI records
//! instead of leaving a marker file inside every project.
//!
//! Like the rest of the app this is state plus pure transitions: it owns no
//! terminal and draws nothing (`crate::ui::home` does), so every flow here
//! is testable without a tty. The two filesystem writes it *does* perform
//! --- creating a project directory and forgetting a registry entry --- are
//! the point of the screen, and both report their failure into
//! [`HomeScreen::status`] rather than ending the session.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::settings::{self, ProjectEntry, ProjectRegistry};
use crate::workspace::{DirRowKind, dir_rows};

/// What the screen decided, once it decides anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeOutcome {
    /// Open the dashboard on this directory. It may be a project that was
    /// just created, in which case it is empty and the backend prompt takes
    /// over from there --- the same path a `mkdir`-and-`cd` start takes.
    Open(PathBuf),
    Quit,
}

/// Whether a screen point sits inside `rect` (Ratatui `Rect` has no
/// contains for positions; the mouse side of the screen shares the one in
/// `app::mouse`, this is the model-side copy the same way).
fn in_rect(rect: ratatui::layout::Rect, point: (u16, u16)) -> bool {
    rect.x <= point.0
        && point.0 < rect.x + rect.width
        && rect.y <= point.1
        && point.1 < rect.y + rect.height
}

/// A click inside one of the flow modals: only the folder picker has rows
/// to select; the name prompt and the forget question are typed answers
/// with no click surface.
fn click_flow(flow: &mut Flow, point: (u16, u16), area: ratatui::layout::Rect) {
    let Flow::CreateDir {
        path,
        selected,
        error,
    } = flow
    else {
        return;
    };
    let popup = crate::ui::centered(area, 72, 18);
    let (rows, _) = dir_rows(path);
    let len = rows.len();
    let height = popup.height.saturating_sub(2 + 1 + 2) as usize; // borders, path line, footer
    if len == 0 || height == 0 {
        return;
    }
    let inner_y = popup.y + 1 + 1; // border, path line
    let inner_bottom = popup.y + popup.height - 1 - 2; // border, footer
    if point.1 < inner_y || point.1 >= inner_bottom {
        return;
    }
    let offset = (*selected).saturating_sub(height - 1);
    let index = offset + (point.1 - inner_y) as usize;
    if index < len {
        *selected = index;
        // Selecting a row is navigation: like every key move, it clears the
        // picker's last refusal.
        *error = None;
    }
}

/// One row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    /// Always first, never filtered out: the way to a project that does not
    /// exist yet.
    Create,
    Project(&'a ProjectEntry),
}

/// A modal step layered over the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Choosing the folder the new project's own directory goes into.
    /// Navigation is the workspace picker's ([`dir_rows`]) --- one directory
    /// browser in the codebase, two questions asked with it.
    CreateDir {
        path: PathBuf,
        selected: usize,
        error: Option<String>,
    },
    /// Naming the new project. The name becomes a directory inside `parent`,
    /// which is why it is rejected rather than sanitized: the user should
    /// see the name they typed on disk.
    CreateName {
        parent: PathBuf,
        input: String,
        error: Option<String>,
    },
    /// Removing an entry from the registry. Confirmed because the row is
    /// the only record of a project ChipTUI created; the directory itself is
    /// never touched, which is what the prompt says.
    Forget { path: PathBuf, name: String },
}

pub struct HomeScreen {
    /// The user config this screen reads and writes.
    config: PathBuf,
    /// `$HOME`: where the folder picker starts when nothing was used
    /// before, and what the list abbreviates paths against.
    home: PathBuf,
    /// The session's icon set ([`crate::app::resolve_icons`]), read once
    /// here because this screen exists before any `App` --- the same
    /// startup read `App::new` does, and the reason the home answers the
    /// same `[ui] icons` the dashboard will.
    icons: crate::icons::IconSet,
    entries: Vec<ProjectEntry>,
    query: String,
    /// Index into [`Self::rows`], so `0` is always the create row.
    selected: usize,
    flow: Option<Flow>,
    status: Option<String>,
}

impl HomeScreen {
    /// Loads the screen from the config at `config_dir`.
    pub fn new(config_dir: &Path, home: &Path) -> Self {
        let mut screen = Self {
            config: settings::user_config_path(config_dir),
            home: home.to_path_buf(),
            icons: crate::app::resolve_icons(config_dir),
            entries: Vec::new(),
            query: String::new(),
            selected: 0,
            flow: None,
            status: None,
        };
        screen.reload();
        screen
    }

    fn reload(&mut self) {
        let config_dir = self
            .config
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        self.entries = ProjectRegistry::load(&config_dir, &self.home)
            .listed()
            .into_iter()
            .cloned()
            .collect();
        self.clamp();
    }

    /// The session's icon set, resolved in [`Self::new`] --- the home's
    /// backend marks answer it (they are decoration, hidden by `none`).
    pub fn icons(&self) -> crate::icons::IconSet {
        self.icons
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn flow(&self) -> Option<&Flow> {
        self.flow.as_ref()
    }

    /// A failure or notice to show under the list --- cleared by the next
    /// action that could produce a new one.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Whether the registry has anything at all, as opposed to the filter
    /// hiding everything: the two deserve different empty states.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The visible rows: the create row, then every project matching the
    /// filter.
    pub fn rows(&self) -> Vec<Row<'_>> {
        std::iter::once(Row::Create)
            .chain(self.matches().into_iter().map(Row::Project))
            .collect()
    }

    /// Projects matching the current query, in registry order (most
    /// recently opened first).
    fn matches(&self) -> Vec<&ProjectEntry> {
        let query = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|entry| matches_query(entry, &query))
            .collect()
    }

    /// The path as the list shows it: `~` for the home directory, so the
    /// column stays readable on the paths that dominate it.
    pub fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if !self.home.as_os_str().is_empty() => format!("~/{}", rest.display()),
            _ => path.display().to_string(),
        }
    }

    fn clamp(&mut self) {
        let last = self.rows().len().saturating_sub(1);
        self.selected = self.selected.min(last);
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.rows().len() as isize;
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    /// The project under the cursor, or `None` on the create row.
    fn selected_project(&self) -> Option<ProjectEntry> {
        match self.rows().get(self.selected) {
            Some(Row::Project(entry)) => Some((*entry).clone()),
            _ => None,
        }
    }

    /// Mouse gestures on the home screen --- the same opt-in reporting the
    /// dashboard answers (`main.rs` gates on `[ui] mouse` before a gesture
    /// ever reaches here). The grammar: this screen is a launcher, every
    /// row's `Enter` leads somewhere reversible, so a click *selects and
    /// accepts* the row it lands on; the folder-picker flow's rows select
    /// only (`Enter` stays the accept, the same rule the dashboard's
    /// pickers follow); the wheel steps the list's cursor, clamped at the
    /// ends. A gesture over a text-input flow (`CreateName`) or the forget
    /// prompt has no surface --- those are typed answers.
    pub fn on_mouse(
        &mut self,
        event: ratatui::crossterm::event::MouseEvent,
        area: ratatui::layout::Rect,
    ) -> Option<HomeOutcome> {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let point = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(flow) = &mut self.flow {
                    click_flow(flow, point, area);
                } else {
                    return self.click_list(point, area);
                }
                None
            }
            MouseEventKind::ScrollUp => {
                self.wheel(-1, point, area);
                None
            }
            MouseEventKind::ScrollDown => {
                self.wheel(1, point, area);
                None
            }
            _ => None,
        }
    }

    /// A click on the launcher itself: the create row opens the flow, a
    /// project row selects and opens it (`accept_row`, the `Enter` path).
    fn click_list(
        &mut self,
        point: (u16, u16),
        area: ratatui::layout::Rect,
    ) -> Option<HomeOutcome> {
        let areas = crate::ui::home::hit_areas(area);
        if in_rect(areas.create, point) {
            self.selected = 0;
            return self.accept_row();
        }
        if !in_rect(areas.list, point) || areas.list.height == 0 {
            return None;
        }
        // The list draws `rows[1..]` with a fresh `ListState` (selected =
        // `self.selected - 1`), so the minimal-scroll math maps the click
        // back onto a row the same way the dashboard's lists do.
        let rows = self.rows();
        let projects = rows.len() - 1;
        if projects == 0 {
            return None;
        }
        let selected = self.selected.saturating_sub(1);
        let height = areas.list.height as usize;
        let offset = selected.saturating_sub(height - 1);
        let index = offset + (point.1 - areas.list.y) as usize;
        if index >= projects {
            return None;
        }
        self.selected = index + 1;
        self.accept_row()
    }

    /// The wheel over the list steps the cursor, clamped at the ends (the
    /// keyboard's arrows wrap; a wheel that wraps feels like a bug).
    fn wheel(&mut self, direction: isize, point: (u16, u16), area: ratatui::layout::Rect) {
        let areas = crate::ui::home::hit_areas(area);
        if !in_rect(areas.list, point) {
            return;
        }
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let moved = self.selected as isize + direction * 3;
        self.selected = moved.clamp(0, len as isize - 1) as usize;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<HomeOutcome> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(HomeOutcome::Quit);
        }
        match self.flow.take() {
            Some(flow) => self.on_flow_key(flow, key),
            None => self.on_list_key(key),
        }
    }

    /// Keys on the list itself. Typing goes to the filter, which is why
    /// leaving is `esc` and not `q`: with a live search field there are no
    /// letters left to spend on commands.
    fn on_list_key(&mut self, key: KeyEvent) -> Option<HomeOutcome> {
        match key.code {
            KeyCode::Esc => {
                if self.query.is_empty() {
                    return Some(HomeOutcome::Quit);
                }
                self.query.clear();
                self.reset_to_first_match();
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Enter => return self.accept_row(),
            KeyCode::Delete => {
                if let Some(entry) = self.selected_project() {
                    self.status = None;
                    self.flow = Some(Flow::Forget {
                        path: entry.path,
                        name: entry.name,
                    });
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.reset_to_first_match();
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.reset_to_first_match();
            }
            _ => {}
        }
        None
    }

    /// After the filter changes, the cursor goes to the first result --- the
    /// point of a live search is that `enter` follows typing.
    fn reset_to_first_match(&mut self) {
        self.status = None;
        self.selected = usize::from(self.rows().len() > 1);
    }

    fn accept_row(&mut self) -> Option<HomeOutcome> {
        self.status = None;
        match self.rows().get(self.selected)? {
            Row::Create => {
                self.flow = Some(Flow::CreateDir {
                    path: self.start_dir(),
                    selected: 0,
                    error: None,
                });
                None
            }
            Row::Project(entry) => Some(HomeOutcome::Open(entry.path.clone())),
        }
    }

    /// Where the folder picker opens: where the last project was created,
    /// falling back to `$HOME` --- to navigate *from*, never to create in.
    fn start_dir(&self) -> PathBuf {
        let config_dir = self
            .config
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        settings::last_parent(&config_dir, &self.home)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| self.home.clone())
    }

    fn on_flow_key(&mut self, flow: Flow, key: KeyEvent) -> Option<HomeOutcome> {
        match flow {
            Flow::CreateDir {
                path,
                selected,
                error,
            } => self.on_create_dir_key(path, selected, error, key),
            Flow::CreateName {
                parent,
                input,
                error,
            } => self.on_create_name_key(parent, input, error, key),
            Flow::Forget { path, name } => {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => self.forget(&path),
                    KeyCode::Esc | KeyCode::Char('n') => {}
                    _ => self.flow = Some(Flow::Forget { path, name }),
                }
                None
            }
        }
    }

    fn on_create_dir_key(
        &mut self,
        path: PathBuf,
        selected: usize,
        error: Option<String>,
        key: KeyEvent,
    ) -> Option<HomeOutcome> {
        let (rows, read_error) = dir_rows(&path);
        let mut selected = selected.min(rows.len().saturating_sub(1));
        match key.code {
            KeyCode::Esc => return None,
            KeyCode::Up => selected = step(selected, -1, rows.len()),
            KeyCode::Down => selected = step(selected, 1, rows.len()),
            KeyCode::Left | KeyCode::Backspace => {
                if let Some(parent) = path.parent() {
                    self.flow = Some(Flow::CreateDir {
                        path: parent.to_path_buf(),
                        selected: 0,
                        error: None,
                    });
                    return None;
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                let row = rows.get(selected)?;
                match row.kind {
                    // Accepting a folder is only "where the project goes";
                    // it is not itself the project, so the only thing that
                    // has to be true is that it is a directory.
                    DirRowKind::Use if key.code == KeyCode::Enter => {
                        if row.path.is_dir() {
                            self.flow = Some(Flow::CreateName {
                                parent: row.path.clone(),
                                input: String::new(),
                                error: None,
                            });
                        } else {
                            self.flow = Some(Flow::CreateDir {
                                path,
                                selected,
                                error: Some(format!("{} is not a directory", row.path.display())),
                            });
                        }
                        return None;
                    }
                    DirRowKind::Use => {}
                    // Descending lands on "use this directory", so a reflex
                    // second `enter` accepts the folder just entered.
                    DirRowKind::Parent | DirRowKind::Dir => {
                        self.flow = Some(Flow::CreateDir {
                            path: row.path.clone(),
                            selected: 0,
                            error: None,
                        });
                        return None;
                    }
                }
            }
            _ => {}
        }
        self.flow = Some(Flow::CreateDir {
            path,
            selected,
            error: error.or(read_error),
        });
        None
    }

    fn on_create_name_key(
        &mut self,
        parent: PathBuf,
        mut input: String,
        error: Option<String>,
        key: KeyEvent,
    ) -> Option<HomeOutcome> {
        let mut error = error;
        match key.code {
            // Back to the folder picker, at the folder just accepted.
            KeyCode::Esc => {
                self.flow = Some(Flow::CreateDir {
                    path: parent,
                    selected: 0,
                    error: None,
                });
                return None;
            }
            KeyCode::Backspace => {
                input.pop();
                error = None;
            }
            KeyCode::Char(c) => {
                input.push(c);
                error = None;
            }
            KeyCode::Enter => match self.create_project(&parent, input.trim()) {
                Ok(dir) => return Some(HomeOutcome::Open(dir)),
                Err(reason) => error = Some(reason),
            },
            _ => {}
        }
        self.flow = Some(Flow::CreateName {
            parent,
            input,
            error,
        });
        None
    }

    /// Creates `parent/name` and remembers where it went. The backend --- and
    /// with it the scaffold and the registry entry --- is answered by the
    /// dashboard's prompt once the (empty) directory opens, so nothing here
    /// needs to know which backend this will be.
    fn create_project(&mut self, parent: &Path, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() {
            return Err("type a name for the project".to_string());
        }
        if name.contains(std::path::is_separator) || name == "." || name == ".." {
            return Err("the name is a folder name, not a path".to_string());
        }
        let dir = parent.join(name);
        if dir.exists() {
            return Err(format!("{} already exists", dir.display()));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|source| format!("could not create {name}: {source}"))?;
        if let Err(source) = settings::save_last_parent(&self.config, parent) {
            // Not fatal: the project exists, only the convenience of
            // starting here next time is lost.
            self.status = Some(format!("could not record the folder: {source}"));
        }
        Ok(dir)
    }

    fn forget(&mut self, path: &Path) {
        match settings::forget_project(&self.config, path) {
            Ok(()) => {
                self.status = Some(format!("{} removed from the list", path.display()));
                self.reload();
            }
            Err(source) => self.status = Some(format!("could not update the config: {source}")),
        }
    }
}

fn matches_query(entry: &ProjectEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        entry.name.to_lowercase(),
        entry.path.display().to_string().to_lowercase(),
        entry.backend.display_name().to_lowercase(),
    );
    query.split_whitespace().all(|term| haystack.contains(term))
}

fn step(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(screen: &mut HomeScreen, text: &str) {
        for c in text.chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
    }

    struct Fixture {
        home: PathBuf,
        config_dir: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let home = std::env::temp_dir().join(format!(
                "chiptui-home-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&home);
            let config_dir = home.join(".config");
            std::fs::create_dir_all(&config_dir).unwrap();
            Self { home, config_dir }
        }

        fn record(&self, name: &str, backend: BackendKind) -> PathBuf {
            let dir = self.home.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            settings::record_project(
                &settings::user_config_path(&self.config_dir),
                ProjectEntry::new(&dir, backend).opened_now(),
            )
            .unwrap();
            dir
        }

        fn screen(&self) -> HomeScreen {
            HomeScreen::new(&self.config_dir, &self.home)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn the_list_is_the_registry_plus_the_create_row() {
        let fixture = Fixture::new("list");
        fixture.record("blinky", BackendKind::Zephyr);
        fixture.record("sensor", BackendKind::MicroPython);

        let screen = fixture.screen();
        assert_eq!(screen.rows().len(), 3);
        assert!(matches!(screen.rows()[0], Row::Create));
    }

    #[test]
    fn typing_filters_and_puts_the_cursor_on_the_first_result() {
        let fixture = Fixture::new("filter");
        fixture.record("blinky", BackendKind::Zephyr);
        fixture.record("sensor-node", BackendKind::MicroPython);

        let mut screen = fixture.screen();
        typed(&mut screen, "sens");

        let rows = screen.rows();
        assert_eq!(rows.len(), 2, "only the match and the create row");
        assert!(matches!(rows[1], Row::Project(entry) if entry.name == "sensor-node"));
        assert_eq!(screen.selected(), 1, "enter follows typing");

        // The backend name is searchable too, and so is the path.
        screen.handle_key(key(KeyCode::Esc));
        typed(&mut screen, "zephyr");
        assert!(matches!(screen.rows()[1], Row::Project(entry) if entry.name == "blinky"));
    }

    #[test]
    fn a_query_matching_nothing_leaves_only_the_create_row() {
        let fixture = Fixture::new("nomatch");
        fixture.record("blinky", BackendKind::Zephyr);

        let mut screen = fixture.screen();
        typed(&mut screen, "nothing");
        assert_eq!(screen.rows().len(), 1);
        assert_eq!(screen.selected(), 0);
        assert!(
            !screen.is_empty(),
            "the registry is not empty, the filter is"
        );
    }

    #[test]
    fn enter_opens_the_selected_project_and_esc_quits() {
        let fixture = Fixture::new("open");
        let dir = fixture.record("blinky", BackendKind::Zephyr);

        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            Some(HomeOutcome::Open(dir))
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            Some(HomeOutcome::Quit)
        );
    }

    #[test]
    fn esc_clears_the_filter_before_it_quits() {
        let fixture = Fixture::new("esc");
        fixture.record("blinky", BackendKind::Zephyr);

        let mut screen = fixture.screen();
        typed(&mut screen, "bli");
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(screen.query(), "");
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            Some(HomeOutcome::Quit)
        );
    }

    #[test]
    fn delete_forgets_a_project_without_deleting_it() {
        let fixture = Fixture::new("forget");
        let dir = fixture.record("blinky", BackendKind::Zephyr);

        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Delete));
        assert!(matches!(screen.flow(), Some(Flow::Forget { .. })));

        screen.handle_key(key(KeyCode::Enter));
        assert!(screen.flow().is_none());
        assert_eq!(screen.rows().len(), 1, "the row is gone");
        assert!(dir.is_dir(), "the project itself is untouched");
    }

    #[test]
    fn declining_the_forget_prompt_keeps_the_row() {
        let fixture = Fixture::new("keep");
        fixture.record("blinky", BackendKind::Zephyr);

        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Delete));
        screen.handle_key(key(KeyCode::Esc));

        assert!(screen.flow().is_none());
        assert_eq!(screen.rows().len(), 2);
    }

    #[test]
    fn creating_a_project_picks_a_folder_then_a_name() {
        let fixture = Fixture::new("create");
        let parent = fixture.home.join("apps");
        std::fs::create_dir_all(&parent).unwrap();

        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Enter)); // the create row
        let Some(Flow::CreateDir { path, .. }) = screen.flow() else {
            panic!("expected the folder picker, got {:?}", screen.flow());
        };
        assert_eq!(path, &fixture.home, "starts at $HOME the first time");

        // Descend into `apps` (rows: use, .., .config, apps) and accept it.
        let rows = dir_rows(&fixture.home).0;
        let index = rows.iter().position(|row| row.name == "apps").unwrap();
        for _ in 0..index {
            screen.handle_key(key(KeyCode::Down));
        }
        screen.handle_key(key(KeyCode::Enter));
        screen.handle_key(key(KeyCode::Enter)); // "use this directory"
        assert!(matches!(screen.flow(), Some(Flow::CreateName { .. })));

        typed(&mut screen, "new-app");
        let outcome = screen.handle_key(key(KeyCode::Enter));

        let created = parent.join("new-app");
        assert_eq!(outcome, Some(HomeOutcome::Open(created.clone())));
        assert!(created.is_dir(), "the directory exists, and is empty");
        assert_eq!(
            created.read_dir().unwrap().count(),
            0,
            "the backend prompt scaffolds it, not the creator"
        );

        // Where it went is remembered for the next project.
        assert_eq!(
            settings::last_parent(&fixture.config_dir, &fixture.home).as_deref(),
            Some(parent.as_path())
        );
        assert_eq!(fixture.screen().start_dir(), parent);
    }

    #[test]
    fn an_existing_name_is_refused_without_touching_it() {
        let fixture = Fixture::new("clash");
        let taken = fixture.home.join("taken");
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("keep.txt"), "mine").unwrap();

        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Enter));
        screen.handle_key(key(KeyCode::Enter)); // use $HOME
        typed(&mut screen, "taken");
        let outcome = screen.handle_key(key(KeyCode::Enter));

        assert_eq!(outcome, None, "nothing is opened");
        match screen.flow() {
            Some(Flow::CreateName { error, .. }) => {
                assert!(error.as_ref().unwrap().contains("already exists"));
            }
            other => panic!("expected the name step to stay open, got {other:?}"),
        }
        assert!(taken.join("keep.txt").exists());
    }

    #[test]
    fn a_name_that_is_a_path_is_refused() {
        let fixture = Fixture::new("path-name");
        let mut screen = fixture.screen();
        screen.handle_key(key(KeyCode::Enter));
        screen.handle_key(key(KeyCode::Enter));
        typed(&mut screen, "apps/blinky");
        screen.handle_key(key(KeyCode::Enter));

        match screen.flow() {
            Some(Flow::CreateName { error, .. }) => {
                assert!(error.as_ref().unwrap().contains("folder name"));
            }
            other => panic!("expected the name step to stay open, got {other:?}"),
        }
        assert!(!fixture.home.join("apps").exists(), "no path was created");
    }

    #[test]
    fn a_vanished_project_is_not_listed() {
        let fixture = Fixture::new("vanished");
        let dir = fixture.record("gone", BackendKind::Zephyr);
        std::fs::remove_dir_all(&dir).unwrap();

        let screen = fixture.screen();
        assert_eq!(screen.rows().len(), 1);
        assert!(screen.is_empty());
    }

    #[test]
    fn paths_are_shown_against_the_home_directory() {
        let fixture = Fixture::new("display");
        let screen = fixture.screen();
        assert_eq!(
            screen.display_path(&fixture.home.join("apps/blinky")),
            "~/apps/blinky"
        );
        assert_eq!(screen.display_path(Path::new("/opt/x")), "/opt/x");
    }
}
