//! Shared local directory/file selection. Callers own domain validation and
//! persistence; this model owns navigation, editing and filesystem errors.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Directory,
    File(FileFilter),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "chiptui-picker-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn picker(&self, kind: PickerKind) -> PathPicker {
            PathPicker::new(kind, self.0.clone(), &self.0)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn key(picker: &mut PathPicker, code: KeyCode) -> PickerOutcome {
        picker.handle_key(KeyEvent::new(code, KeyModifiers::NONE), 5)
    }
    fn ctrl(picker: &mut PathPicker, ch: char) {
        picker.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL), 5);
    }
    fn type_path(picker: &mut PathPicker, value: &str) -> PickerOutcome {
        ctrl(picker, 'l');
        ctrl(picker, 'u');
        for ch in value.chars() {
            key(picker, KeyCode::Char(ch));
        }
        key(picker, KeyCode::Enter)
    }

    #[test]
    fn filters_apply_to_browsed_and_typed_files_and_keep_directories() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("images")).unwrap();
        for name in ["app.BIN", "other.elf", "wrong.txt", ".hidden.bin"] {
            std::fs::write(fixture.0.join(name), b"image").unwrap();
        }
        let mut picker = fixture.picker(PickerKind::File(FileFilter::Firmware));
        let names: Vec<_> = picker
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["..", "images", "app.BIN", "other.elf"]);
        key(&mut picker, KeyCode::Char('.'));
        assert!(
            picker
                .entries
                .iter()
                .any(|entry| entry.name == ".hidden.bin")
        );
        assert_eq!(type_path(&mut picker, "wrong.txt"), PickerOutcome::Pending);
        assert!(picker.error.as_ref().unwrap().contains(".bin, .elf"));
        assert_eq!(
            type_path(&mut picker, "app.BIN"),
            PickerOutcome::Selected(fixture.0.join("app.BIN"))
        );
    }

    #[test]
    fn typed_directories_navigate_before_explicit_acceptance() {
        let fixture = Fixture::new();
        let folder = fixture.0.join("café with spaces");
        std::fs::create_dir(&folder).unwrap();
        let mut picker = fixture.picker(PickerKind::Directory);
        assert_eq!(
            type_path(&mut picker, "~/café with spaces"),
            PickerOutcome::Pending
        );
        assert_eq!(picker.path, folder);
        assert_eq!(picker.focus, PickerFocus::List);
        assert_eq!(
            key(&mut picker, KeyCode::Enter),
            PickerOutcome::Selected(folder)
        );
        key(&mut picker, KeyCode::Left);
        assert_eq!(picker.path, fixture.0);
        assert_eq!(picker.entries[picker.selected].name, "café with spaces");
    }

    #[test]
    fn unicode_editing_and_help_preserve_the_input_and_cancel_in_layers() {
        let fixture = Fixture::new();
        let mut picker = fixture.picker(PickerKind::Directory);
        ctrl(&mut picker, 'l');
        ctrl(&mut picker, 'u');
        for ch in "é?q".chars() {
            key(&mut picker, KeyCode::Char(ch));
        }
        key(&mut picker, KeyCode::Home);
        key(&mut picker, KeyCode::Right);
        key(&mut picker, KeyCode::Backspace);
        assert_eq!(picker.input, "?q");
        assert_eq!(picker.cursor, 0);
        key(&mut picker, KeyCode::F(1));
        key(&mut picker, KeyCode::Char('x'));
        key(&mut picker, KeyCode::Esc);
        assert!(!picker.help);
        assert_eq!(picker.input, "?q");
        assert_eq!(picker.focus, PickerFocus::Path);
        assert_eq!(key(&mut picker, KeyCode::Esc), PickerOutcome::Pending);
        assert_eq!(key(&mut picker, KeyCode::Esc), PickerOutcome::Cancelled);
    }

    #[test]
    fn creation_is_explicit_refuses_paths_and_never_overwrites() {
        let fixture = Fixture::new();
        let mut picker = fixture.picker(PickerKind::Directory);
        ctrl(&mut picker, 'n');
        for ch in "new folder".chars() {
            key(&mut picker, KeyCode::Char(ch));
        }
        assert!(!fixture.0.join("new folder").exists());
        key(&mut picker, KeyCode::Esc);
        assert!(!fixture.0.join("new folder").exists());
        ctrl(&mut picker, 'n');
        for ch in "../escape".chars() {
            key(&mut picker, KeyCode::Char(ch));
        }
        key(&mut picker, KeyCode::Enter);
        assert!(picker.error.is_some());
        ctrl(&mut picker, 'u');
        for ch in "new folder".chars() {
            key(&mut picker, KeyCode::Char(ch));
        }
        assert_eq!(key(&mut picker, KeyCode::Enter), PickerOutcome::Pending);
        assert_eq!(picker.path, fixture.0.join("new folder"));
        key(&mut picker, KeyCode::Left);
        ctrl(&mut picker, 'n');
        for ch in "new folder".chars() {
            key(&mut picker, KeyCode::Char(ch));
        }
        key(&mut picker, KeyCode::Enter);
        assert!(picker.error.as_ref().unwrap().contains("Cannot create"));
        assert_eq!(picker.path, fixture.0);
    }

    #[test]
    fn a_stale_listing_never_accepts_a_vanished_file() {
        let fixture = Fixture::new();
        let file = fixture.0.join("image.bin");
        std::fs::write(&file, b"image").unwrap();
        let mut picker = fixture.picker(PickerKind::File(FileFilter::All));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            picker.entries[picker.selected].path, file,
            "the drawn snapshot survives until refresh"
        );
        assert_eq!(key(&mut picker, KeyCode::Enter), PickerOutcome::Pending);
        assert!(picker.error.is_some());
        ctrl(&mut picker, 'r');
        assert_eq!(picker.entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_navigate_and_non_utf8_filenames_keep_their_real_path() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let fixture = Fixture::new();
        let actual = fixture.0.join("actual");
        std::fs::create_dir(&actual).unwrap();
        symlink(&actual, fixture.0.join("linked")).unwrap();
        let file = actual.join(std::ffi::OsString::from_vec(b"image-\xff.bin".to_vec()));
        std::fs::write(&file, b"image").unwrap();
        let mut picker = fixture.picker(PickerKind::File(FileFilter::Firmware));
        assert!(
            picker
                .entries
                .iter()
                .any(|entry| entry.name == "linked" && entry.kind == EntryKind::Directory)
        );
        type_path(&mut picker, "actual");
        key(&mut picker, KeyCode::End);
        assert_eq!(
            key(&mut picker, KeyCode::Enter),
            PickerOutcome::Selected(file)
        );
    }

    #[test]
    fn history_is_scoped_persistent_and_preserves_other_settings() {
        let fixture = Fixture::new();
        let config = fixture.0.join("config.toml");
        let before = "# keep this\n[ui]\nmouse = true\n";
        std::fs::write(&config, before).unwrap();
        let firmware = fixture.0.join("firmware");
        std::fs::create_dir(&firmware).unwrap();
        crate::settings::save_picker_directory(&config, "firmware", &firmware).unwrap();
        crate::settings::save_picker_directory(&config, "create_project", &fixture.0).unwrap();
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .starts_with(before)
        );
        assert_eq!(
            crate::settings::picker_directory(&config, "firmware", &fixture.0),
            Some(firmware.clone())
        );
        assert_eq!(
            crate::settings::picker_directory(&config, "create_project", &fixture.0),
            Some(fixture.0.clone())
        );
        std::fs::remove_dir(&firmware).unwrap();
        assert_eq!(
            crate::settings::picker_directory(&config, "firmware", &fixture.0),
            None
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFilter {
    All,
    Firmware,
}

impl FileFilter {
    pub fn accepts(self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Firmware => path.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("bin") || ext.eq_ignore_ascii_case("elf")
            }),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all files",
            Self::Firmware => "firmware: .bin, .elf",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Use,
    Parent,
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    List,
    Path,
    NewDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Pending,
    Cancelled,
    Selected(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPicker {
    pub kind: PickerKind,
    pub path: PathBuf,
    pub selected: usize,
    pub error: Option<String>,
    pub show_hidden: bool,
    pub focus: PickerFocus,
    pub input: String,
    /// Byte offset, always on a character boundary.
    pub cursor: usize,
    pub help: bool,
    home: PathBuf,
    /// A frame and a click use the very same snapshot. Cloning an overlay
    /// shares the listing instead of cloning thousands of paths per key.
    pub entries: Arc<[Entry]>,
}

impl PathPicker {
    pub fn new(kind: PickerKind, start: PathBuf, home: &Path) -> Self {
        let selected_file = start.is_file().then(|| start.clone());
        let mut path = if selected_file.is_some() {
            start.parent().unwrap_or(&start).to_path_buf()
        } else {
            start
        };
        if !path.is_absolute() {
            path = std::env::current_dir()
                .unwrap_or_else(|_| home.to_path_buf())
                .join(path);
        }
        let mut picker = Self {
            kind,
            input: path.to_string_lossy().into_owned(),
            cursor: 0,
            path,
            selected: 0,
            error: None,
            show_hidden: false,
            focus: PickerFocus::List,
            help: false,
            home: home.to_path_buf(),
            entries: Arc::from([]),
        };
        if let Some(file) = &selected_file {
            picker.show_hidden = file
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        }
        picker.reload();
        if matches!(kind, PickerKind::File(_)) && picker.entries.len() > 1 {
            picker.selected = 1; // skip the parent row on opening
        }
        if let Some(file) = selected_file {
            picker.select_path(&file);
        }
        picker
    }

    pub fn reload(&mut self) {
        let previous = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
        let mut rows = Vec::new();
        if self.kind == PickerKind::Directory {
            rows.push(Entry {
                name: "use this directory".into(),
                path: self.path.clone(),
                kind: EntryKind::Use,
            });
        }
        if let Some(parent) = self.path.parent() {
            rows.push(Entry {
                name: "..".into(),
                path: parent.to_path_buf(),
                kind: EntryKind::Parent,
            });
        }
        self.error = None;
        let read = || -> std::io::Result<Vec<Entry>> {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&self.path)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                let metadata = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                let kind = if metadata.is_dir() {
                    EntryKind::Directory
                } else if metadata.is_file()
                    && matches!(self.kind, PickerKind::File(filter) if filter.accepts(&path))
                {
                    EntryKind::File
                } else {
                    continue;
                };
                entries.push(Entry { name, path, kind });
            }
            entries.sort_by(|a, b| {
                (a.kind == EntryKind::File, &a.name).cmp(&(b.kind == EntryKind::File, &b.name))
            });
            Ok(entries)
        };
        match read() {
            Ok(entries) => rows.extend(entries),
            Err(err) => {
                self.error = Some(format!(
                    "Cannot list {}: {err}. Choose another folder or retry with Ctrl+R.",
                    self.path.display()
                ))
            }
        }
        self.entries = rows.into();
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        if let Some(path) = previous {
            self.select_path(&path);
        }
    }

    fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            self.selected = index;
        }
    }

    pub fn step(&mut self, delta: isize) {
        self.selected = (self.selected as isize + delta)
            .clamp(0, self.entries.len().saturating_sub(1) as isize)
            as usize;
    }

    pub fn edit_path(&mut self) {
        self.focus = PickerFocus::Path;
        self.input = self.path.to_string_lossy().into_owned();
        self.cursor = self.input.len();
    }

    fn navigate(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.error = Some(format!(
                "{} is not a directory. Choose an existing folder.",
                path.display()
            ));
            return;
        }
        self.path = path;
        self.selected = 0;
        self.entries = Arc::from([]);
        self.focus = PickerFocus::List;
        self.input = self.path.to_string_lossy().into_owned();
        self.cursor = self.input.len();
        self.reload();
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.path.parent().map(Path::to_path_buf) {
            let left = self.path.clone();
            self.navigate(parent);
            self.select_path(&left);
        }
    }

    fn accept(&mut self, path: PathBuf) -> PickerOutcome {
        let valid = match self.kind {
            PickerKind::Directory => path.is_dir(),
            PickerKind::File(filter) => path.is_file() && filter.accepts(&path),
        };
        if !valid {
            self.error = Some(match self.kind {
                PickerKind::Directory => format!(
                    "{} is not a directory. Choose an existing folder.",
                    path.display()
                ),
                PickerKind::File(filter) => format!(
                    "{} is not an existing file matching {}.",
                    path.display(),
                    filter.label()
                ),
            });
            return PickerOutcome::Pending;
        }
        // Verify access at acceptance too: a listing may have gone stale.
        let access = match self.kind {
            PickerKind::Directory => std::fs::read_dir(&path).map(|_| ()),
            PickerKind::File(_) => std::fs::File::open(&path).map(|_| ()),
        };
        match access {
            Ok(()) => PickerOutcome::Selected(path),
            Err(err) => {
                self.error = Some(format!(
                    "Cannot open {}: {err}. Check permissions or choose another path.",
                    path.display()
                ));
                PickerOutcome::Pending
            }
        }
    }

    fn submit_input(&mut self) -> PickerOutcome {
        if self.focus == PickerFocus::NewDirectory {
            let name = &self.input;
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.contains(std::path::is_separator)
            {
                self.error = Some("Enter one folder name, not a path.".into());
            } else {
                let path = self.path.join(name);
                match std::fs::create_dir(&path) {
                    Ok(()) => self.navigate(path),
                    Err(err) => {
                        self.error = Some(format!(
                            "Cannot create {}: {err}. Change the name or check permissions.",
                            path.display()
                        ))
                    }
                }
            }
        } else if self.input.is_empty() {
            self.error = Some("Enter a path.".into());
        } else {
            let expanded = crate::settings::expand_home(&self.input, &self.home);
            let path = if expanded.is_absolute() {
                expanded
            } else {
                self.path.join(expanded)
            };
            if path.is_dir() {
                self.navigate(path);
            } else if matches!(self.kind, PickerKind::File(_)) {
                return self.accept(path);
            } else {
                self.navigate(path);
            }
        }
        PickerOutcome::Pending
    }

    pub fn handle_key(&mut self, key: KeyEvent, page: usize) -> PickerOutcome {
        if key.code == KeyCode::F(1) {
            self.help = !self.help;
            return PickerOutcome::Pending;
        }
        if self.help {
            if key.code == KeyCode::Esc {
                self.help = false;
            }
            return PickerOutcome::Pending;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            match key.code {
                KeyCode::Char('l') => self.edit_path(),
                KeyCode::Char('h') => {
                    self.show_hidden = !self.show_hidden;
                    self.reload();
                }
                KeyCode::Char('r') => self.reload(),
                KeyCode::Char('n') if self.kind == PickerKind::Directory => {
                    self.focus = PickerFocus::NewDirectory;
                    self.input.clear();
                    self.cursor = 0;
                    self.error = None;
                }
                KeyCode::Char('u') if self.focus != PickerFocus::List => {
                    self.input.clear();
                    self.cursor = 0;
                }
                _ => {}
            }
            return PickerOutcome::Pending;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.focus == PickerFocus::List {
                self.edit_path();
            } else {
                self.focus = PickerFocus::List;
            }
            return PickerOutcome::Pending;
        }
        if self.focus != PickerFocus::List {
            match key.code {
                KeyCode::Esc => {
                    self.focus = PickerFocus::List;
                    self.error = None;
                }
                KeyCode::Enter => return self.submit_input(),
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.input.len(),
                KeyCode::Left => {
                    self.cursor = self.input[..self.cursor]
                        .char_indices()
                        .last()
                        .map_or(0, |(index, _)| index)
                }
                KeyCode::Right => {
                    self.cursor += self.input[self.cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8)
                }
                KeyCode::Backspace if self.cursor > 0 => {
                    let previous = self.input[..self.cursor]
                        .char_indices()
                        .last()
                        .map_or(0, |(index, _)| index);
                    self.input.drain(previous..self.cursor);
                    self.cursor = previous;
                }
                KeyCode::Delete if self.cursor < self.input.len() => {
                    self.input.remove(self.cursor);
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.input.insert(self.cursor, ch);
                    self.cursor += ch.len_utf8();
                }
                _ => {}
            }
            return PickerOutcome::Pending;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return PickerOutcome::Cancelled,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.reload();
            }
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-(page.max(1) as isize)),
            KeyCode::PageDown => self.step(page.max(1) as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.entries.len().saturating_sub(1),
            KeyCode::Left | KeyCode::Backspace => self.ascend(),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(entry) = self.entries.get(self.selected).cloned() {
                    match entry.kind {
                        EntryKind::Directory | EntryKind::Parent => self.navigate(entry.path),
                        _ if key.code == KeyCode::Enter => return self.accept(entry.path),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        PickerOutcome::Pending
    }
}
