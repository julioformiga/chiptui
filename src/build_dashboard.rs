//! The build dashboard's window state: which tab, which row, what is loaded.
//!
//! Zephyr 4.4's `west build -t dashboard` renders a build directory into a
//! multi-page HTML report and opens a browser. This is the same report read
//! inside the terminal, over the artifacts the build already wrote
//! ([`crate::backend::zephyr::report`]) instead of over the generated HTML.
//! Both doors stay: the Zephyr Actions menu offers this window and the HTML
//! report side by side.
//!
//! # Why the state lives on `App`
//!
//! [`crate::app::Overlay::BuildDashboard`] is a unit variant and everything
//! it shows is here, the arrangement [`crate::app::packages::PackagesState`]
//! already uses, for the two reasons that made it necessary there:
//!
//! * `on_overlay_key` clones the overlay on every keystroke. Five tabs'
//!   worth of rows inside the variant would be re-cloned per key.
//! * The overlay slot is one deep. Generating the memory report *closes*
//!   this window, runs a command for a minute, and re-opens it --- which
//!   only works if the window's tab, cursor, filter and scroll outlived it.
//!
//! # Loading
//!
//! Artifacts are parsed when a tab is entered, never in the draw path, and
//! kept behind a `(mtime, len)` stamp so re-entering a tab after a rebuild
//! re-reads and re-entering it otherwise does not. Every parse is
//! milliseconds --- the module docs of
//! [`crate::backend::zephyr::report`] carry the measurements --- so none of
//! it is deferred to a thread.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::app::DocsFocus;
use crate::backend::zephyr::report::{
    self, ReportPaths, Stamp, build_info, build_info::BuildInfo, devicetree, devicetree::DtNode,
    elf_stat, kconfig, kconfig::KconfigSymbol, memory, memory::MemoryReport,
};

/// The window's pages, in strip order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardTab {
    #[default]
    Summary,
    Memory,
    Kconfig,
    DeviceTree,
    ElfStats,
}

impl DashboardTab {
    /// Every tab, in the order the strip draws them --- the same order the
    /// HTML report's own navigation uses.
    pub const ALL: [Self; 5] = [
        Self::Summary,
        Self::Memory,
        Self::Kconfig,
        Self::DeviceTree,
        Self::ElfStats,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::Memory => "Memory",
            Self::Kconfig => "Kconfig",
            Self::DeviceTree => "Devicetree",
            Self::ElfStats => "ELF",
        }
    }

    /// The tab's index in [`Self::ALL`], which is also its position in the
    /// per-tab state array.
    pub const fn index(self) -> usize {
        match self {
            Self::Summary => 0,
            Self::Memory => 1,
            Self::Kconfig => 2,
            Self::DeviceTree => 3,
            Self::ElfStats => 4,
        }
    }

    /// The tab `steps` away, clamped at both ends. Never wraps --- the
    /// dashboard's own strip rule (`App::switch_log_tab`).
    pub fn stepped(self, steps: i32) -> Self {
        let last = Self::ALL.len() as i32 - 1;
        let next = (self.index() as i32 + steps).clamp(0, last);
        Self::ALL[next as usize]
    }
}

/// One tab's keyboard state.
///
/// Per tab, not shared: a filter typed on Kconfig must not narrow the
/// devicetree, and a cursor is a position in *this* tab's list. Switching
/// away and back returns to where the user was.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PaneState {
    /// The filter line.
    pub input: String,
    /// The list cursor.
    pub selected: usize,
    /// The details pane's scroll, when it holds the keyboard.
    pub scroll: u16,
}

/// An artifact's parse, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Cached<T> {
    /// Not read yet --- no tab has asked for it.
    Idle,
    Ready {
        value: T,
        /// What the value was parsed from, so a rebuild is noticed and an
        /// unchanged file is not re-read.
        stamp: Option<Stamp>,
    },
    /// The file is not there, with the sentence the tab shows. Distinct from
    /// [`Self::Failed`]: a build with `CONFIG_OUTPUT_STAT=n` has no
    /// `zephyr.stat`, and that is a fact about the build, not an error.
    Missing(String),
    /// The file is there and could not be read or understood.
    Failed(String),
}

/// `Idle` is the zero value. Derived `Default` would demand `T: Default`,
/// which none of the parsed types owes anyone.
impl<T> Default for Cached<T> {
    fn default() -> Self {
        Self::Idle
    }
}

impl<T> Cached<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Ready { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The sentence to draw when there is no value.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Missing(reason) | Self::Failed(reason) => Some(reason.as_str()),
            Self::Idle => Some("not read yet"),
            Self::Ready { .. } => None,
        }
    }

    fn stamp(&self) -> Option<&Option<Stamp>> {
        match self {
            Self::Ready { stamp, .. } => Some(stamp),
            _ => None,
        }
    }
}

/// What the marker column shows for a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The row has children and they are showing.
    Expanded,
    /// The row has children and they are hidden.
    Collapsed,
    /// The row has no children, or the tab is not a tree.
    None,
}

/// One drawn row of the list pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The row's position in its tab's own data, so the details pane can
    /// look the entry back up without carrying a copy of it.
    pub index: usize,
    /// Indentation level; always 0 on a tab that is not a tree, and 0 for
    /// every row while a filter is active (a filtered tree is a flat list of
    /// matches --- see [`DashboardState::rows`]).
    pub depth: usize,
    pub marker: Marker,
    pub label: String,
    /// Drawn flush right: a size, a value, a count.
    pub trailing: String,
    /// True for a row the pane should draw muted --- a disabled devicetree
    /// node, an unset Kconfig symbol, a zero-size section.
    pub dimmed: bool,
    /// True for the Memory tab's one synthetic row, which runs a command
    /// instead of standing for an entry. It carries no `index` into any
    /// data, so every lookup must check this first.
    pub prompt: bool,
}

/// One line of the details pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailLine {
    /// A label/value pair, drawn in two columns.
    Field {
        label: String,
        value: String,
    },
    /// Free text, wrapped by the renderer.
    Text(String),
    /// A section break with a caption.
    Heading(String),
    Blank,
}

impl DetailLine {
    pub fn field(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Field {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Everything the window shows.
#[derive(Debug, Default)]
pub struct DashboardState {
    pub tab: DashboardTab,
    /// Which half of the body has the keyboard --- the board/shield
    /// pickers' grammar, reused rather than reinvented.
    pub focus: DocsFocus,
    panes: [PaneState; 5],
    /// Which build directory the caches belong to; a change resets them all.
    source: Option<(PathBuf, String)>,
    build_info: Cached<BuildInfo>,
    toolchain: Option<String>,
    stat: Cached<Vec<elf_stat::Section>>,
    stat_text: String,
    kconfig: Cached<Vec<KconfigSymbol>>,
    kconfig_traced: bool,
    devicetree: Cached<Vec<DtNode>>,
    memory: Cached<MemoryReport>,
    /// Sizes of the ELF and the binary, and the ELF's timestamp, for the
    /// Summary --- read from the filesystem, which is the only place they
    /// exist.
    elf_size: Option<u64>,
    bin_size: Option<u64>,
    /// Expanded node keys, one set per tree tab. Keyed by the trees' own
    /// stable identifiers (`size_report`'s `identifier`, a devicetree path)
    /// rather than by row index, which every filter change would invalidate.
    expanded_memory: HashSet<String>,
    expanded_devicetree: HashSet<String>,
    /// Whether the memory report on disk predates the ELF.
    memory_stale: bool,
}

impl DashboardState {
    /// The state of the tab in view.
    pub fn pane(&self) -> &PaneState {
        &self.panes[self.tab.index()]
    }

    pub fn pane_mut(&mut self) -> &mut PaneState {
        &mut self.panes[self.tab.index()]
    }

    /// The build directory the loaded data belongs to.
    pub fn source(&self) -> Option<&(PathBuf, String)> {
        self.source.as_ref()
    }

    pub fn memory(&self) -> &Cached<MemoryReport> {
        &self.memory
    }

    pub fn memory_is_stale(&self) -> bool {
        self.memory_stale
    }

    /// Whether the Kconfig rows came from the trace file rather than from
    /// the `.config` fallback --- which decides whether the details pane can
    /// speak about origins at all.
    pub fn kconfig_traced(&self) -> bool {
        self.kconfig_traced
    }

    pub fn stat_text(&self) -> &str {
        &self.stat_text
    }

    /// Points the state at a build directory, clearing everything when it is
    /// a different one. Answers whether anything was cleared.
    pub fn retarget(&mut self, root: &std::path::Path, build_dir: &str) -> bool {
        let target = (root.to_path_buf(), build_dir.to_string());
        if self.source.as_ref() == Some(&target) {
            return false;
        }
        let tab = self.tab;
        *self = Self {
            tab,
            source: Some(target),
            ..Self::default()
        };
        true
    }

    /// Forgets the memory report, so the next entry re-reads it --- what a
    /// finished `size_report` run needs.
    pub fn invalidate_memory(&mut self) {
        self.memory = Cached::Idle;
    }
}

/// Whether a cached value must be re-read: it was never read, or the file
/// behind it changed. An unreadable file is not a change.
fn is_stale<T>(cached: &Cached<T>, current: Option<Stamp>) -> bool {
    match cached.stamp() {
        None => true,
        Some(loaded) => current.is_some() && *loaded != current,
    }
}

/// Reads a file, mapping its absence and its failures to the two states the
/// pane can explain.
fn read(path: &std::path::Path, missing: &str) -> Result<(String, Option<Stamp>), Cached<()>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok((text, report::stamp(path))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(Cached::Missing(missing.to_string()))
        }
        Err(error) => Err(Cached::Failed(format!(
            "{path:?} could not be read: {error}"
        ))),
    }
}

/// Re-types the error half of [`read`], whose payload is always empty.
fn carry<T>(state: Cached<()>) -> Cached<T> {
    match state {
        Cached::Missing(reason) => Cached::Missing(reason),
        Cached::Failed(reason) => Cached::Failed(reason),
        _ => Cached::Idle,
    }
}

/// The size the memory report guards itself with.
///
/// A real report is a node per symbol: 4.5 MB and 6309 nodes on the project
/// this was measured against, parsed in 185 ms by a debug build. Ten times
/// that would be a visible stall in the draw loop, so it is refused with a
/// sentence instead --- the tab still works, it just says why.
pub const MAX_MEMORY_REPORT_BYTES: u64 = 64 * 1024 * 1024;

impl DashboardState {
    /// Reads whatever the tab in view needs, if it is not already current.
    ///
    /// Called when the window opens and on every tab switch --- never from
    /// the renderer. Re-entering a tab after a rebuild picks the new
    /// artifacts up, which is the only reload this window has and the reason
    /// it needs no reload key (every printable character is filter text).
    pub fn ensure_tab(&mut self, paths: &ReportPaths) {
        match self.tab {
            DashboardTab::Summary => {
                self.load_build_info(paths);
                self.load_stat(paths);
                self.elf_size = report::stamp(&paths.elf()).map(|(_, len)| len);
                self.bin_size = report::stamp(&paths.bin()).map(|(_, len)| len);
            }
            DashboardTab::ElfStats => self.load_stat(paths),
            // `build_info.yml` too, and not for a row of its own: it names
            // the Zephyr checkout, which is what makes a symbol's absolute
            // location readable as `boards/…/xiao_esp32c3.conf`.
            DashboardTab::Kconfig => {
                self.load_build_info(paths);
                self.load_kconfig(paths);
            }
            DashboardTab::DeviceTree => self.load_devicetree(paths),
            DashboardTab::Memory => self.load_memory(paths),
        }
        self.clamp_cursor();
    }

    fn load_build_info(&mut self, paths: &ReportPaths) {
        let path = paths.build_info();
        if !is_stale(&self.build_info, report::stamp(&path)) {
            return;
        }
        self.build_info = match read(
            &path,
            "no build_info.yml --- this build directory is not configured",
        ) {
            Ok((text, stamp)) => Cached::Ready {
                value: build_info::parse(&text),
                stamp,
            },
            Err(state) => carry(state),
        };
        self.toolchain = paths
            .cmake_compiler()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| build_info::toolchain_version(&text));
    }

    fn load_stat(&mut self, paths: &ReportPaths) {
        let path = paths.stat();
        if !is_stale(&self.stat, report::stamp(&path)) {
            return;
        }
        self.stat = match read(
            &path,
            "no zephyr.stat --- a build with CONFIG_OUTPUT_STAT=y writes it",
        ) {
            Ok((text, stamp)) => {
                let sections = elf_stat::parse(&text);
                self.stat_text = text;
                Cached::Ready {
                    value: sections,
                    stamp,
                }
            }
            Err(state) => {
                self.stat_text.clear();
                carry(state)
            }
        };
    }

    fn load_kconfig(&mut self, paths: &ReportPaths) {
        let trace = paths.config_trace();
        let trace_stamp = report::stamp(&trace);
        // The trace is the good source; `.config` is what an older build has.
        // Which one answered decides what the details pane may claim, so the
        // staleness test follows whichever file is actually in use.
        let path = if trace_stamp.is_some() {
            trace
        } else {
            paths.config()
        };
        if !is_stale(&self.kconfig, report::stamp(&path)) {
            return;
        }
        let traced = trace_stamp.is_some();
        self.kconfig = match read(
            &path,
            "no Kconfig output --- this build directory has not been configured",
        ) {
            Ok((text, stamp)) => {
                let symbols = if traced {
                    kconfig::parse_trace(&text)
                } else {
                    Some(kconfig::parse_config(&text))
                };
                match symbols {
                    Some(value) => {
                        self.kconfig_traced = traced;
                        Cached::Ready { value, stamp }
                    }
                    None => Cached::Failed(".config-trace.json is not readable".to_string()),
                }
            }
            Err(state) => carry(state),
        };
    }

    fn load_devicetree(&mut self, paths: &ReportPaths) {
        let path = paths.devicetree();
        if !is_stale(&self.devicetree, report::stamp(&path)) {
            return;
        }
        self.devicetree = match read(
            &path,
            "no zephyr.dts --- this build has not been configured",
        ) {
            Ok((text, stamp)) => {
                let nodes = devicetree::parse(&text);
                if nodes.is_empty() {
                    Cached::Failed("zephyr.dts holds no nodes".to_string())
                } else {
                    // The root starts open, its children closed --- the HTML
                    // browser's own default.
                    if self.expanded_devicetree.is_empty() {
                        self.expanded_devicetree.insert(nodes[0].path.clone());
                    }
                    Cached::Ready {
                        value: nodes,
                        stamp,
                    }
                }
            }
            Err(state) => carry(state),
        };
    }

    fn load_memory(&mut self, paths: &ReportPaths) {
        let path = paths.memory_report("all");
        self.memory_stale = report::memory_report_stale(paths);
        let stamp = report::stamp(&path);
        if !is_stale(&self.memory, stamp) {
            return;
        }
        if let Some((_, len)) = stamp
            && len > MAX_MEMORY_REPORT_BYTES
        {
            self.memory = Cached::Failed(format!(
                "the memory report is {} --- too large to read here",
                report::display_size(len)
            ));
            return;
        }
        self.memory = match read(
            &path,
            "no memory report yet --- generating one reads the ELF's debug info",
        ) {
            Ok((text, stamp)) => match memory::parse(&text) {
                Some(value) => {
                    if self.expanded_memory.is_empty() && !value.nodes.is_empty() {
                        self.expanded_memory
                            .insert(value.nodes[0].identifier.clone());
                    }
                    Cached::Ready { value, stamp }
                }
                None => Cached::Failed("the memory report could not be read".to_string()),
            },
            Err(state) => carry(state),
        };
    }
}

/// How many of the largest symbols the Memory tab leads with ---
/// `dashboard.py`'s own top-ten table.
const LARGEST_SYMBOLS: usize = 10;

impl DashboardState {
    /// The Summary tab's label/value pairs, which are also its rows.
    ///
    /// Derived rather than stored: every input is already cached, and a row
    /// that has no answer is dropped instead of drawn empty --- the pane
    /// says less rather than saying nothing in more places.
    pub fn summary_fields(&self) -> Vec<(String, String)> {
        let mut fields = Vec::new();
        let mut push = |label: &str, value: Option<String>| {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                fields.push((label.to_string(), value));
            }
        };
        if let Some(info) = self.build_info.value() {
            push("Application", info.application.clone());
            push(
                "Board",
                info.board.clone().map(|board| match &info.qualifiers {
                    Some(qualifiers) => format!("{board} \u{b7} {qualifiers}"),
                    None => board,
                }),
            );
            push("Revision", info.revision.clone());
            push(
                "Toolchain",
                match (&info.toolchain, &self.toolchain) {
                    (Some(name), Some(version)) => Some(format!("{name} \u{b7} {version}")),
                    (name, version) => name.clone().or_else(|| version.clone()),
                },
            );
            push("Zephyr", info.zephyr_version.clone());
            push("West", info.west_version.clone());
            push("Workspace", info.topdir.clone());
            push("Build command", info.command.clone());
        }
        push("ELF", self.elf_size.map(report::display_size));
        push("Binary", self.bin_size.map(report::display_size));
        if let Some(sections) = self.stat.value() {
            let summary = elf_stat::summary(sections);
            // Shares are of the *image* --- text, rodata, rwdata and bss ---
            // not of every byte in the ELF. `other` is dominated by the
            // debug sections, which never reach the board: on a real build
            // it is 80% of the file, and letting it into the denominator
            // reports a 900 KB text segment as "7%" of nothing the reader
            // cares about. `other` therefore carries a size and no share.
            let resident = summary.text + summary.rodata + summary.rwdata + summary.bss;
            for (label, size) in summary.rows() {
                let value = if label == "other" || resident == 0 {
                    format!("{:>10}", report::display_size(size))
                } else {
                    let share = size as f64 * 100.0 / resident as f64;
                    format!("{:>10}  {share:>4.0}%", report::display_size(size))
                };
                fields.push((label.to_string(), value));
            }
        }
        fields
    }

    /// The rows the list pane draws for the tab in view.
    ///
    /// A filter does two different things depending on the tab. On a flat
    /// list it selects rows. On a tree it *flattens*: the matches are shown
    /// as a plain list, each labelled by its full path, because the
    /// alternative --- keeping the hierarchy --- leaves the reader expanding
    /// their way down to a hit they can already see exists.
    pub fn rows(&self) -> Vec<Row> {
        let filter = self.pane().input.trim().to_lowercase();
        match self.tab {
            DashboardTab::Summary => self
                .summary_fields()
                .into_iter()
                .enumerate()
                .filter(|(_, (label, value))| matches(&filter, &[label, value]))
                .map(|(index, (label, value))| Row {
                    index,
                    depth: 0,
                    marker: Marker::None,
                    label,
                    trailing: value,
                    dimmed: false,
                    prompt: false,
                })
                .collect(),
            DashboardTab::Kconfig => self
                .kconfig
                .value()
                .map(|symbols| {
                    symbols
                        .iter()
                        .enumerate()
                        .filter(|(_, symbol)| {
                            matches(&filter, &[&symbol.name, &symbol.display_value()])
                        })
                        .map(|(index, symbol)| Row {
                            index,
                            depth: 0,
                            marker: Marker::None,
                            label: symbol.name.clone(),
                            trailing: symbol.display_value(),
                            dimmed: symbol.value.is_none(),
                            prompt: false,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            DashboardTab::ElfStats => self
                .stat
                .value()
                .map(|sections| {
                    sections
                        .iter()
                        .enumerate()
                        .filter(|(_, section)| {
                            matches(&filter, &[&section.name, &section.kind, &section.flags])
                        })
                        .map(|(index, section)| Row {
                            index,
                            depth: 0,
                            marker: Marker::None,
                            label: format!("[{:>2}] {}", section.index, section.name),
                            trailing: report::display_size(section.size),
                            dimmed: section.size == 0,
                            prompt: false,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            DashboardTab::DeviceTree => self.devicetree_rows(&filter),
            DashboardTab::Memory => self.memory_rows(&filter),
        }
    }

    fn devicetree_rows(&self, filter: &str) -> Vec<Row> {
        let Some(nodes) = self.devicetree.value() else {
            return Vec::new();
        };
        if !filter.is_empty() {
            return nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| {
                    matches(filter, &[&node.path, &node.label()])
                        || node
                            .props
                            .iter()
                            .any(|prop| matches(filter, &[&prop.name, &prop.value]))
                })
                .map(|(index, node)| Row {
                    index,
                    depth: 0,
                    marker: Marker::None,
                    label: node.path.clone(),
                    trailing: props_count(node.props.len()),
                    dimmed: node.disabled(),
                    prompt: false,
                })
                .collect();
        }
        let visible = |index: usize| -> bool {
            // A node draws when every ancestor above it is expanded. The
            // ancestors are the nearest earlier node at each smaller depth,
            // which a backward walk finds without building a parent map.
            let mut depth = nodes[index].depth;
            for node in nodes[..index].iter().rev() {
                if node.depth < depth {
                    if !self.expanded_devicetree.contains(&node.path) {
                        return false;
                    }
                    depth = node.depth;
                    if depth == 0 {
                        break;
                    }
                }
            }
            true
        };
        nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| visible(*index))
            .map(|(index, node)| Row {
                index,
                depth: node.depth,
                marker: self.marker(
                    node.has_children,
                    self.expanded_devicetree.contains(&node.path),
                ),
                label: node.label(),
                trailing: props_count(node.props.len()),
                dimmed: node.disabled(),
                prompt: false,
            })
            .collect()
    }

    /// The Memory tab's synthetic first row, when the report is absent or
    /// predates the ELF. A stale report keeps its rows *below* the prompt:
    /// old numbers labelled stale beat no numbers at all.
    fn memory_prompt(&self) -> Option<Row> {
        let missing = self.memory.value().is_none();
        if !missing && !self.memory_stale {
            return None;
        }
        Some(Row {
            index: usize::MAX,
            depth: 0,
            marker: Marker::None,
            label: if missing {
                "\u{25b6} Generate the memory report".to_string()
            } else {
                "\u{25b6} Regenerate \u{2014} the ELF is newer than this report".to_string()
            },
            trailing: String::new(),
            dimmed: false,
            prompt: true,
        })
    }

    fn memory_rows(&self, filter: &str) -> Vec<Row> {
        let prompt = self.memory_prompt();
        let Some(report) = self.memory.value() else {
            return prompt.into_iter().collect();
        };
        let nodes = &report.nodes;
        if !filter.is_empty() {
            return nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| matches(filter, &[&node.identifier]))
                .map(|(index, node)| Row {
                    index,
                    depth: 0,
                    marker: Marker::None,
                    label: node.identifier.clone(),
                    trailing: report::display_size(node.size),
                    dimmed: node.size == 0,
                    prompt: false,
                })
                .collect();
        }
        let visible = |index: usize| -> bool {
            let mut depth = nodes[index].depth;
            for node in nodes[..index].iter().rev() {
                if node.depth < depth {
                    if !self.expanded_memory.contains(&node.identifier) {
                        return false;
                    }
                    depth = node.depth;
                    if depth == 0 {
                        break;
                    }
                }
            }
            true
        };
        prompt
            .into_iter()
            .chain(
                nodes
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| visible(*index))
                    .map(|(index, node)| Row {
                        index,
                        depth: node.depth,
                        marker: self.marker(
                            node.has_children,
                            self.expanded_memory.contains(&node.identifier),
                        ),
                        label: node.name.clone(),
                        trailing: report::display_size(node.size),
                        dimmed: node.size == 0,
                        prompt: false,
                    }),
            )
            .collect()
    }

    fn marker(&self, has_children: bool, expanded: bool) -> Marker {
        match (has_children, expanded) {
            (false, _) => Marker::None,
            (true, true) => Marker::Expanded,
            (true, false) => Marker::Collapsed,
        }
    }
}

/// Case-insensitive substring over any of the haystacks; an empty filter
/// matches everything.
fn matches(filter: &str, haystacks: &[&str]) -> bool {
    filter.is_empty()
        || haystacks
            .iter()
            .any(|text| text.to_lowercase().contains(filter))
}

fn props_count(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "1 prop".to_string(),
        many => format!("{many} props"),
    }
}

impl DashboardState {
    /// Keeps the cursor inside the rows that exist. Called after anything
    /// that can shorten the list: a load, a filter keystroke, a collapse.
    pub fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        let pane = self.pane_mut();
        pane.selected = pane.selected.min(len.saturating_sub(1));
    }

    /// Moves the list cursor, clamped at both ends. Clamped rather than
    /// wrapping: these lists run to thousands of rows, where wrapping from
    /// the top to the bottom reads as a glitch rather than as a shortcut.
    pub fn move_cursor(&mut self, by: i32) {
        let len = self.rows().len();
        if len == 0 {
            self.pane_mut().selected = 0;
            return;
        }
        let last = len as i32 - 1;
        let pane = self.pane_mut();
        pane.selected = match by {
            i32::MIN => 0,
            i32::MAX => last as usize,
            by => (pane.selected as i32 + by).clamp(0, last) as usize,
        };
    }

    /// The row under the cursor, if any.
    pub fn selected_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.pane().selected)
    }

    /// Whether the tab in view draws a tree, and so answers `←`/`→`. A
    /// filtered tree is a flat list of matches, where expanding means
    /// nothing.
    pub fn is_tree(&self) -> bool {
        matches!(self.tab, DashboardTab::Memory | DashboardTab::DeviceTree)
            && self.pane().input.trim().is_empty()
    }

    /// The expansion key of the row under the cursor, for a tree tab.
    fn selected_key(&self) -> Option<String> {
        let row = self.selected_row()?;
        if row.prompt {
            return None;
        }
        match self.tab {
            DashboardTab::Memory => Some(
                self.memory
                    .value()?
                    .nodes
                    .get(row.index)?
                    .identifier
                    .clone(),
            ),
            DashboardTab::DeviceTree => Some(self.devicetree.value()?.get(row.index)?.path.clone()),
            _ => None,
        }
    }

    fn expanded_mut(&mut self) -> Option<&mut HashSet<String>> {
        match self.tab {
            DashboardTab::Memory => Some(&mut self.expanded_memory),
            DashboardTab::DeviceTree => Some(&mut self.expanded_devicetree),
            _ => None,
        }
    }

    /// Whether the row under the cursor is the Memory tab's generate
    /// prompt --- the one row in the window that runs something.
    pub fn selected_is_prompt(&self) -> bool {
        self.selected_row().is_some_and(|row| row.prompt)
    }

    /// Opens or closes the row under the cursor. Answers whether anything
    /// changed, so a caller can tell an ineffective `Enter` from a real one.
    pub fn toggle_selected(&mut self) -> bool {
        if !self.is_tree() {
            return false;
        }
        let Some(row) = self.selected_row() else {
            return false;
        };
        if row.marker == Marker::None {
            return false;
        }
        let Some(key) = self.selected_key() else {
            return false;
        };
        if let Some(expanded) = self.expanded_mut()
            && !expanded.remove(&key)
        {
            expanded.insert(key);
        }
        self.clamp_cursor();
        true
    }

    /// `→`: opens a closed row, and steps into the first child of an open
    /// one --- the file browser's arrow grammar, transposed onto a tree.
    pub fn expand_selected(&mut self) {
        if !self.is_tree() {
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        match row.marker {
            Marker::Collapsed => {
                self.toggle_selected();
            }
            Marker::Expanded => self.move_cursor(1),
            Marker::None => {}
        }
    }

    /// `←`: closes an open row, and steps out to the parent of a closed or
    /// leaf one.
    pub fn collapse_selected(&mut self) {
        if !self.is_tree() {
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.marker == Marker::Expanded {
            self.toggle_selected();
            return;
        }
        // Walk back to the nearest row one level shallower.
        if row.depth == 0 {
            return;
        }
        let rows = self.rows();
        let cursor = self.pane().selected;
        if let Some(parent) = rows[..cursor]
            .iter()
            .rposition(|candidate| candidate.depth < row.depth)
        {
            self.pane_mut().selected = parent;
        }
    }
}

impl DashboardState {
    /// The details pane's content for the row under the cursor.
    ///
    /// Plain data, not styled spans: the renderer decides how a
    /// [`DetailLine`] looks, and this stays testable without a terminal.
    pub fn details(&self) -> Vec<DetailLine> {
        let Some(row) = self.selected_row() else {
            return self
                .empty_reason()
                .map(|reason| vec![DetailLine::Text(reason.to_string())])
                .unwrap_or_default();
        };
        if row.prompt {
            return vec![
                DetailLine::Heading("Memory report".to_string()),
                DetailLine::Blank,
                DetailLine::Text(
                    "Enter runs Zephyr's own size_report over the build's debug info. \
                     It takes a few minutes and streams into the Monitor, where Stop \
                     works; this window closes and comes back here when it finishes."
                        .to_string(),
                ),
                DetailLine::Blank,
                DetailLine::Text(
                    "The three report files land in the build directory's dashboard/ \
                     folder --- the same place west build -t dashboard writes them, so \
                     one run serves both dashboards."
                        .to_string(),
                ),
            ];
        }
        match self.tab {
            DashboardTab::Summary => vec![
                DetailLine::Heading(row.label),
                DetailLine::Blank,
                DetailLine::Text(row.trailing),
            ],
            DashboardTab::Kconfig => self.kconfig_details(row.index),
            DashboardTab::ElfStats => self.section_details(row.index),
            DashboardTab::DeviceTree => self.devicetree_details(row.index),
            DashboardTab::Memory => self.memory_details(row.index),
        }
    }

    /// Why the list is empty, when it is: the artifact's own state, or the
    /// filter having excluded everything.
    pub fn empty_reason(&self) -> Option<&str> {
        let cached = match self.tab {
            DashboardTab::Summary => self.build_info.reason(),
            DashboardTab::Kconfig => self.kconfig.reason(),
            DashboardTab::ElfStats => self.stat.reason(),
            DashboardTab::DeviceTree => self.devicetree.reason(),
            DashboardTab::Memory => self.memory.reason(),
        };
        cached.or({
            if self.pane().input.trim().is_empty() {
                None
            } else {
                Some("nothing matches the filter")
            }
        })
    }

    fn kconfig_details(&self, index: usize) -> Vec<DetailLine> {
        let Some(symbol) = self.kconfig.value().and_then(|list| list.get(index)) else {
            return Vec::new();
        };
        let mut lines = vec![
            DetailLine::Heading(symbol.name.clone()),
            DetailLine::Blank,
            DetailLine::field("type", &symbol.kind),
            DetailLine::field(
                "value",
                if symbol.value.is_none() {
                    "(none)".to_string()
                } else {
                    symbol.display_value()
                },
            ),
        ];
        if self.kconfig_traced {
            lines.push(DetailLine::field("visible", &symbol.visibility));
        }
        lines.push(DetailLine::field("source", symbol.source.label()));
        if let Some(location) = symbol.source.location() {
            // A `Text` line rather than a field: `labelled` wraps on word
            // boundaries and a path has none, so an absolute one was being
            // clipped at the pane's edge. `wrap_words` hard-splits instead.
            lines.push(DetailLine::Text(format!(
                "{}:{}",
                self.relative(&location.file),
                location.line
            )));
        }
        let expressions = symbol.source.expressions();
        if !expressions.is_empty() {
            lines.push(DetailLine::Blank);
            // Several expressions mean "any of these"; the `||` is the
            // Kconfig operator that joins them, and dashboard.py writes it
            // the same way.
            for (position, expression) in expressions.iter().enumerate() {
                if position > 0 {
                    lines.push(DetailLine::Text("||".to_string()));
                }
                lines.push(DetailLine::Text(expression.clone()));
            }
        }
        if !self.kconfig_traced {
            lines.push(DetailLine::Blank);
            lines.push(DetailLine::Text(
                "read from .config, which records no origin --- the type above is \
                 inferred from the value"
                    .to_string(),
            ));
        }
        lines
    }

    fn section_details(&self, index: usize) -> Vec<DetailLine> {
        let Some(section) = self.stat.value().and_then(|list| list.get(index)) else {
            return Vec::new();
        };
        let mut lines = vec![
            DetailLine::Heading(if section.name.is_empty() {
                format!("section {}", section.index)
            } else {
                section.name.clone()
            }),
            DetailLine::Blank,
            DetailLine::field("number", section.index.to_string()),
            DetailLine::field("type", &section.kind),
            DetailLine::field("address", format!("0x{:08x}", section.addr)),
            DetailLine::field(
                "size",
                format!(
                    "{} ({} bytes)",
                    report::display_size(section.size),
                    section.size
                ),
            ),
            DetailLine::field("flags", flag_names(&section.flags)),
            DetailLine::Blank,
            // The line that makes the Summary's five buckets auditable:
            // every section says which one it landed in and why.
            DetailLine::field("counted as", bucket_of(section)),
        ];
        if section.name.ends_with("[...]") {
            lines.push(DetailLine::Blank);
            lines.push(DetailLine::Text(
                "readelf elides section names past 17 characters when not asked \
                 for wide output"
                    .to_string(),
            ));
        }
        lines
    }

    fn devicetree_details(&self, index: usize) -> Vec<DetailLine> {
        let Some(node) = self.devicetree.value().and_then(|list| list.get(index)) else {
            return Vec::new();
        };
        let mut lines = vec![DetailLine::Heading(node.path.clone()), DetailLine::Blank];
        if !node.labels.is_empty() {
            lines.push(DetailLine::field("label", node.labels.join(", ")));
        }
        if let Some(file) = &node.file {
            lines.push(DetailLine::field(
                "defined in",
                match node.line {
                    Some(line) => format!("{file}:{line}"),
                    None => file.clone(),
                },
            ));
        }
        if node.props.is_empty() {
            lines.push(DetailLine::Blank);
            lines.push(DetailLine::Text("no properties".to_string()));
            return lines;
        }
        lines.push(DetailLine::Blank);
        for prop in &node.props {
            lines.push(DetailLine::field(
                &prop.name,
                if prop.is_flag() {
                    "(flag)".to_string()
                } else {
                    prop.value.clone()
                },
            ));
            if let (Some(file), Some(line)) = (&prop.file, prop.line) {
                lines.push(DetailLine::Text(format!("  {file}:{line}")));
            }
        }
        lines
    }

    fn memory_details(&self, index: usize) -> Vec<DetailLine> {
        let Some(report) = self.memory.value() else {
            return Vec::new();
        };
        let Some(node) = report.nodes.get(index) else {
            return Vec::new();
        };
        let mut lines = vec![
            DetailLine::Heading(node.name.clone()),
            DetailLine::Blank,
            DetailLine::field("path", &node.identifier),
            DetailLine::field(
                "size",
                format!("{} ({} bytes)", report::display_size(node.size), node.size),
            ),
            DetailLine::field("share", format!("{:.2}%", report.percent(node.size))),
        ];
        if let Some(address) = node.address {
            lines.push(DetailLine::field("address", format!("0x{address:08x}")));
        }
        if let Some(section) = &node.section {
            lines.push(DetailLine::field("section", section));
        }
        if !node.loc.is_empty() {
            lines.push(DetailLine::field("region", node.loc.join(", ")));
        }
        if node.has_children {
            let children = report
                .nodes
                .iter()
                .filter(|other| {
                    other.depth == node.depth + 1 && other.identifier.starts_with(&node.identifier)
                })
                .count();
            lines.push(DetailLine::field("holds", format!("{children} entries")));
        }
        lines
    }

    /// A path shown against the tree it belongs to.
    ///
    /// `.config-trace.json` records absolute paths, which are mostly
    /// prefix: the interesting half is `boards/…/xiao_esp32c3.conf`, not the
    /// twelve characters of `$HOME` before it. `dashboard.py` relativises
    /// against `ZEPHYR_BASE` for the same reason; this tries the project
    /// root too, and prefers the longest match so a path inside the build
    /// directory is shown against the project rather than against the
    /// checkout.
    fn relative(&self, path: &str) -> String {
        let candidate = std::path::Path::new(path);
        let best = self
            .path_roots()
            .into_iter()
            .filter(|root| candidate.starts_with(root))
            .max_by_key(|root| root.as_os_str().len());
        match best.and_then(|root| candidate.strip_prefix(&root).ok().map(Path::to_path_buf)) {
            Some(relative) => relative.display().to_string(),
            None => path.to_string(),
        }
    }

    /// The trees a path may be shown against: the Zephyr checkout the build
    /// recorded, and the project the window is open on.
    fn path_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(base) = self
            .build_info
            .value()
            .and_then(|info| info.zephyr_base.as_ref())
        {
            roots.push(PathBuf::from(base));
        }
        if let Some((root, _)) = &self.source {
            roots.push(root.clone());
        }
        roots
    }

    /// The Memory tab's leading rows: the largest symbols, which is the
    /// question a memory report is usually opened to answer.
    pub fn largest_symbols(&self) -> Vec<(String, String, String)> {
        let Some(report) = self.memory.value() else {
            return Vec::new();
        };
        report
            .largest(LARGEST_SYMBOLS)
            .into_iter()
            .map(|node| {
                (
                    node.name.clone(),
                    report::display_size(node.size),
                    format!("{:.2}%", report.percent(node.size)),
                )
            })
            .collect()
    }
}

/// The flag letters spelled out, in readelf's own `Key to Flags:` wording.
fn flag_names(flags: &str) -> String {
    if flags.is_empty() {
        return "(none)".to_string();
    }
    let named: Vec<String> = flags
        .chars()
        .map(|flag| {
            let name = match flag {
                'W' => "write",
                'A' => "alloc",
                'X' => "execute",
                'M' => "merge",
                'S' => "strings",
                'I' => "info",
                'L' => "link order",
                'O' => "extra OS processing",
                'G' => "group",
                'T' => "TLS",
                'C' => "compressed",
                'E' => "exclude",
                'D' => "mbind",
                'p' => "processor specific",
                'o' => "OS specific",
                _ => return flag.to_string(),
            };
            format!("{flag} {name}")
        })
        .collect();
    named.join(" \u{b7} ")
}

/// Which of the Summary's five buckets a section counts toward, and the
/// reason --- the same branch order [`elf_stat::summary`] applies.
fn bucket_of(section: &elf_stat::Section) -> String {
    if section.kind == "NOBITS" {
        return "bss (NOBITS)".to_string();
    }
    if section.kind != "PROGBITS" {
        return format!("other ({})", section.kind);
    }
    if section.executable() {
        return "text (PROGBITS, executable)".to_string();
    }
    if section.writable() {
        return "rwdata (PROGBITS, writable)".to_string();
    }
    if section.allocated() {
        return "rodata (PROGBITS, allocated)".to_string();
    }
    "other (PROGBITS, not allocated)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DTS: &str = "\
/* node '/' defined in board.dts:1 */
/ {
\tmodel = \"A Board\";   /* in board.dts:2 */

\t/* node '/soc' defined in soc.dtsi:3 */
\tsoc {
\t\t/* node '/soc/i2c@1' defined in soc.dtsi:4 */
\t\ti2c0: i2c@1 {
\t\t\tstatus = \"disabled\";  /* in soc.dtsi:5 */
\t\t};
\t};

\t/* node '/chosen' defined in board.dts:9 */
\tchosen {
\t\tzephyr,console = &uart0;  /* in board.dts:10 */
\t};
};
";

    const STAT: &str = "\
ELF Header:
  Class:                             ELF32
  Machine:                           RISC-V

Section Headers:
  [Nr] Name              Type            Addr     Off    Size   ES Flg Lk Inf Al
  [ 0]                   NULL            00000000 000000 000000 00      0   0  0
  [ 1] .text             PROGBITS        42000000 020000 000800 00 WAX  0   0 16
  [ 2] .rodata           PROGBITS        3c000000 021000 000400 00   A  0   0  4
  [ 3] .bss              NOBITS          3fc80000 022000 000200 00  WA  0   0  8
Key to Flags:
  W (write), A (alloc), X (execute)
";

    const TRACE: &str = r#"[
  ["CONFIG_BOARD","n","string","a_board","default",["Kconfig.board",7]],
  ["CONFIG_NET","y","bool","y","select",["WIFI && !SMP"]],
  ["CONFIG_OFF","y","bool",null,"unset",null]
]"#;

    const REPORT: &str = r#"{"symbols":{"name":"Root","size":300,"identifier":"root","loc":[],
      "children":[
        {"name":"kernel","size":300,"identifier":"kernel","loc":[],
         "children":[
           {"name":"heap","size":200,"identifier":"kernel/heap","loc":["ram"],
            "address":16,"section":".bss"},
           {"name":"stack","size":100,"identifier":"kernel/stack","loc":["ram"],
            "address":32,"section":".bss"}]}]},
      "total_size":600}"#;

    /// A build directory holding whichever artifacts a test needs.
    fn build_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "chiptui-dash-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
        std::fs::create_dir_all(root.join("build/dashboard")).unwrap();
        let zephyr = root.join("build/zephyr");
        std::fs::write(
            root.join("build/build_info.yml"),
            "cmake:\n  board:\n    name: 'a_board'\n    qualifiers: 'soc1'\n\
             \x20 zephyr:\n    version: '4.4.0'\nwest:\n  command: '/v/bin/west build'\n",
        )
        .unwrap();
        std::fs::write(zephyr.join("zephyr.stat"), STAT).unwrap();
        std::fs::write(zephyr.join(".config-trace.json"), TRACE).unwrap();
        std::fs::write(zephyr.join("zephyr.dts"), DTS).unwrap();
        std::fs::write(zephyr.join("zephyr.elf"), b"elf").unwrap();
        std::fs::write(zephyr.join("zephyr.bin"), b"binary!!").unwrap();
        std::fs::write(root.join("build/dashboard/all_report.json"), REPORT).unwrap();
        root
    }

    fn state_on(root: &std::path::Path, tab: DashboardTab) -> (DashboardState, ReportPaths) {
        let paths = ReportPaths::new(root, "build");
        let mut state = DashboardState::default();
        state.retarget(root, "build");
        state.tab = tab;
        state.ensure_tab(&paths);
        (state, paths)
    }

    fn labels(state: &DashboardState) -> Vec<String> {
        state.rows().into_iter().map(|row| row.label).collect()
    }

    #[test]
    fn the_summary_reads_the_build_and_the_memory_buckets() {
        let root = build_dir("summary");
        let (state, _) = state_on(&root, DashboardTab::Summary);
        let fields: Vec<(String, String)> = state.summary_fields();
        let by_label = |label: &str| {
            fields
                .iter()
                .find(|(name, _)| name == label)
                .map(|(_, value)| value.trim().to_string())
        };
        assert_eq!(by_label("Board").as_deref(), Some("a_board \u{b7} soc1"));
        assert_eq!(by_label("Zephyr").as_deref(), Some("4.4.0"));
        assert_eq!(
            by_label("Build command").as_deref(),
            Some("west build"),
            "the invocation loses only its program path"
        );
        assert_eq!(by_label("Binary").as_deref(), Some("8 Bytes"));
        // .text is WAX --- executable wins, so it is text and not rwdata.
        assert!(by_label("text").unwrap().starts_with("2 KB"));
        assert!(by_label("rodata").unwrap().starts_with("1 KB"));
        assert!(by_label("bss").unwrap().starts_with("512 Bytes"));
    }

    /// The debug sections dwarf everything that reaches the board --- 80% of
    /// a real ELF --- so they carry a size and no share. Letting them into
    /// the denominator reported a 900 KB text segment as 7% of a number
    /// nobody is asking about.
    #[test]
    fn memory_shares_are_of_the_image_not_of_the_whole_elf() {
        let root = build_dir("summary-shares");
        let (state, _) = state_on(&root, DashboardTab::Summary);
        let fields = state.summary_fields();
        let value = |label: &str| {
            fields
                .iter()
                .find(|(name, _)| name == label)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        // text 0x800, rodata 0x400, rwdata 0, bss 0x200 --- 3584 resident.
        assert!(value("text").contains("57%"), "{}", value("text"));
        assert!(value("rodata").contains("29%"), "{}", value("rodata"));
        assert!(value("bss").contains("14%"), "{}", value("bss"));
        assert!(
            !value("other").contains('%'),
            "the debug sections carry a size and no share: {}",
            value("other")
        );
    }

    /// A symbol's location is shown against the tree it belongs to.
    /// `.config-trace.json` records absolute paths that are mostly prefix.
    #[test]
    fn a_symbol_location_is_relative_to_the_project_or_the_checkout() {
        let root = build_dir("relative");
        std::fs::write(
            root.join("build/zephyr/.config-trace.json"),
            format!(
                r#"[["CONFIG_A","y","bool","y","assign",["{}/build/zephyr/.config",1435]],
                    ["CONFIG_B","y","bool","y","default",["/elsewhere/Kconfig",7]]]"#,
                root.display()
            ),
        )
        .unwrap();
        let (mut state, _) = state_on(&root, DashboardTab::Kconfig);
        state.pane_mut().selected = 0;
        assert!(
            state
                .details()
                .contains(&DetailLine::Text("build/zephyr/.config:1435".to_string())),
            "{:?}",
            state.details()
        );
        // A path under neither tree is shown whole rather than mangled.
        state.pane_mut().selected = 1;
        assert!(
            state
                .details()
                .contains(&DetailLine::Text("/elsewhere/Kconfig:7".to_string())),
            "{:?}",
            state.details()
        );
    }

    /// A field with no answer is dropped, not drawn blank --- the pane says
    /// less rather than saying nothing in more places.
    #[test]
    fn a_summary_field_without_an_answer_is_not_a_row() {
        let root = build_dir("summary-gaps");
        let (state, _) = state_on(&root, DashboardTab::Summary);
        let names: Vec<String> = state
            .summary_fields()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(!names.contains(&"Application".to_string()));
        assert!(!names.contains(&"Revision".to_string()));
        assert!(names.contains(&"Board".to_string()));
    }

    #[test]
    fn the_kconfig_tab_lists_symbols_and_details_their_origin() {
        let root = build_dir("kconfig");
        let (mut state, _) = state_on(&root, DashboardTab::Kconfig);
        assert_eq!(
            labels(&state),
            vec!["CONFIG_BOARD", "CONFIG_NET", "CONFIG_OFF"]
        );
        assert!(state.kconfig_traced());
        // The unset symbol has no value and is drawn dimmed.
        assert!(state.rows()[2].dimmed);
        assert_eq!(state.rows()[0].trailing, "\"a_board\"");

        state.pane_mut().selected = 1;
        let details = state.details();
        assert!(details.contains(&DetailLine::field("source", "selected")));
        assert!(details.contains(&DetailLine::Text("WIFI && !SMP".to_string())));
        assert!(
            !details
                .iter()
                .any(|line| matches!(line, DetailLine::Field { label, .. } if label == "at")),
            "a select carries expressions, never a file and a line"
        );
    }

    /// With no trace file the tab still works off `.config`, and says so
    /// rather than claiming an origin it cannot see.
    #[test]
    fn the_kconfig_tab_falls_back_to_config_and_admits_it() {
        let root = build_dir("kconfig-fallback");
        std::fs::remove_file(root.join("build/zephyr/.config-trace.json")).unwrap();
        std::fs::write(
            root.join("build/zephyr/.config"),
            "CONFIG_A=y\n# CONFIG_B is not set\n",
        )
        .unwrap();
        let (mut state, _) = state_on(&root, DashboardTab::Kconfig);
        assert_eq!(labels(&state), vec!["CONFIG_A", "CONFIG_B"]);
        assert!(!state.kconfig_traced());
        state.pane_mut().selected = 0;
        assert!(
            state
                .details()
                .contains(&DetailLine::field("source", "not recorded"))
        );
    }

    #[test]
    fn the_elf_tab_says_which_bucket_each_section_counts_toward() {
        let root = build_dir("elf");
        let (mut state, _) = state_on(&root, DashboardTab::ElfStats);
        assert_eq!(
            labels(&state),
            vec!["[ 0] ", "[ 1] .text", "[ 2] .rodata", "[ 3] .bss"]
        );
        state.pane_mut().selected = 1;
        let details = state.details();
        assert!(details.contains(&DetailLine::field(
            "counted as",
            "text (PROGBITS, executable)"
        )));
        assert!(details.contains(&DetailLine::field(
            "flags",
            "W write \u{b7} A alloc \u{b7} X execute"
        )));
        state.pane_mut().selected = 3;
        assert!(
            state
                .details()
                .contains(&DetailLine::field("counted as", "bss (NOBITS)"))
        );
    }

    /// The tree opens with the root expanded and everything below it shut,
    /// the HTML browser's own default.
    #[test]
    fn a_tree_opens_with_only_its_root_expanded() {
        let root = build_dir("dt-collapsed");
        let (state, _) = state_on(&root, DashboardTab::DeviceTree);
        assert_eq!(labels(&state), vec!["/", "soc", "chosen"]);
        assert_eq!(state.rows()[1].marker, Marker::Collapsed);
        assert_eq!(state.rows()[0].marker, Marker::Expanded);
        assert_eq!(state.rows()[2].marker, Marker::None);
    }

    #[test]
    fn expanding_a_node_reveals_only_its_own_children() {
        let root = build_dir("dt-expand");
        let (mut state, _) = state_on(&root, DashboardTab::DeviceTree);
        state.pane_mut().selected = 1; // soc
        state.expand_selected();
        assert_eq!(labels(&state), vec!["/", "soc", "i2c0: i2c@1", "chosen"]);
        // `→` on an already-open row steps into it rather than doing nothing.
        state.expand_selected();
        assert_eq!(state.pane().selected, 2);
        // `←` on a leaf steps back out to the parent.
        state.collapse_selected();
        assert_eq!(state.pane().selected, 1);
        // and again closes it.
        state.collapse_selected();
        assert_eq!(labels(&state), vec!["/", "soc", "chosen"]);
    }

    /// A disabled node is marked; a node with no `status` at all is not.
    #[test]
    fn a_disabled_node_is_the_only_one_marked() {
        let root = build_dir("dt-status");
        let (mut state, _) = state_on(&root, DashboardTab::DeviceTree);
        state.pane_mut().selected = 1;
        state.expand_selected();
        let rows = state.rows();
        assert!(rows[2].dimmed, "i2c@1 is disabled");
        assert!(!rows[1].dimmed, "soc declares no status");
    }

    /// Filtering a tree flattens it: the matches show as a plain list with
    /// their full paths, rather than leaving the reader to expand their way
    /// down to a hit they can already see exists.
    #[test]
    fn a_filter_flattens_a_tree_into_full_paths() {
        let root = build_dir("dt-filter");
        let (mut state, _) = state_on(&root, DashboardTab::DeviceTree);
        state.pane_mut().input = "i2c".to_string();
        assert_eq!(labels(&state), vec!["/soc/i2c@1"]);
        assert_eq!(state.rows()[0].depth, 0);
        assert_eq!(state.rows()[0].marker, Marker::None);
        assert!(!state.is_tree(), "arrows are not expansion while filtering");
    }

    /// The filter reaches property names too, which is how a node is found
    /// by what it declares rather than by what it is called.
    #[test]
    fn the_devicetree_filter_reaches_property_names() {
        let root = build_dir("dt-props");
        let (mut state, _) = state_on(&root, DashboardTab::DeviceTree);
        state.pane_mut().input = "zephyr,console".to_string();
        assert_eq!(labels(&state), vec!["/chosen"]);
    }

    #[test]
    fn the_memory_tab_reads_the_report_and_its_largest_symbols() {
        let root = build_dir("memory");
        let (mut state, _) = state_on(&root, DashboardTab::Memory);
        // The root opens expanded, so its own children are already there.
        assert_eq!(labels(&state), vec!["Root", "kernel"]);
        state.pane_mut().selected = 1;
        state.expand_selected();
        assert_eq!(labels(&state), vec!["Root", "kernel", "heap", "stack"]);
        assert_eq!(state.rows()[2].depth, 2);
        assert_eq!(state.rows()[2].trailing, "200 Bytes");

        // A terminal node details its address, section and share.
        state.pane_mut().selected = 2;
        let details = state.details();
        assert!(details.contains(&DetailLine::field("section", ".bss")));
        assert!(details.contains(&DetailLine::field("share", "33.33%")));
        assert!(details.contains(&DetailLine::field("region", "ram")));
        assert_eq!(
            state.largest_symbols(),
            vec![
                (
                    "heap".to_string(),
                    "200 Bytes".to_string(),
                    "33.33%".to_string()
                ),
                (
                    "stack".to_string(),
                    "100 Bytes".to_string(),
                    "16.67%".to_string()
                ),
            ]
        );
        assert!(!state.memory_is_stale(), "the report is newer than the elf");
    }

    /// The staleness rule is `dashboard.py`'s, so both dashboards agree
    /// about the same pair of files.
    #[test]
    fn a_report_older_than_the_elf_reads_as_stale() {
        let root = build_dir("memory-stale");
        let paths = ReportPaths::new(&root, "build");
        // Touch the ELF so it is strictly newer than the report.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(paths.elf(), b"elf again").unwrap();
        let (state, _) = state_on(&root, DashboardTab::Memory);
        assert!(state.memory_is_stale());
    }

    #[test]
    fn a_build_without_a_memory_report_offers_to_make_one() {
        let root = build_dir("memory-missing");
        std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
        let (state, _) = state_on(&root, DashboardTab::Memory);
        // One row, and it is the offer --- an empty list would state the
        // problem, a row states the fix.
        let rows = state.rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].prompt);
        assert!(rows[0].label.contains("Generate the memory report"));
        assert!(state.selected_is_prompt());
        assert!(state.memory_is_stale());
    }

    /// A stale report keeps its rows *under* the offer: old numbers
    /// labelled stale beat no numbers at all.
    #[test]
    fn a_stale_report_is_offered_for_regeneration_without_being_hidden() {
        let root = build_dir("memory-stale-rows");
        let paths = ReportPaths::new(&root, "build");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(paths.elf(), b"rebuilt").unwrap();
        let (mut state, _) = state_on(&root, DashboardTab::Memory);
        let rows = state.rows();
        assert!(rows[0].prompt);
        assert!(rows[0].label.contains("Regenerate"));
        assert_eq!(rows[1].label, "Root", "the old tree is still readable");
        // The offer is not an entry: nothing ever looks it up in the report.
        assert!(!state.toggle_selected(), "the offer is not a tree row");
    }

    /// A missing artifact is a named state, not an error --- `zephyr.stat`
    /// only exists when `CONFIG_OUTPUT_STAT=y`.
    #[test]
    fn a_missing_artifact_names_itself() {
        let root = build_dir("missing-stat");
        std::fs::remove_file(root.join("build/zephyr/zephyr.stat")).unwrap();
        let (state, _) = state_on(&root, DashboardTab::ElfStats);
        assert!(state.rows().is_empty());
        assert!(
            state
                .empty_reason()
                .unwrap()
                .contains("CONFIG_OUTPUT_STAT=y")
        );
    }

    /// Re-entering a tab after a rebuild picks up the new file; re-entering
    /// it otherwise re-reads nothing. That is the whole reload story, and
    /// why the window needs no reload key (every letter is filter text).
    #[test]
    fn re_entering_a_tab_notices_a_rebuild_and_nothing_else() {
        let root = build_dir("reload");
        let (mut state, paths) = state_on(&root, DashboardTab::Kconfig);
        assert_eq!(state.rows().len(), 3);

        state.ensure_tab(&paths);
        assert_eq!(state.rows().len(), 3, "an unchanged file changes nothing");

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            paths.config_trace(),
            r#"[["CONFIG_ONLY","y","bool","y","assign",null]]"#,
        )
        .unwrap();
        state.ensure_tab(&paths);
        assert_eq!(labels(&state), vec!["CONFIG_ONLY"]);
    }

    /// Pointing the window at another build clears everything, so one
    /// project's symbols can never be shown under another's name.
    #[test]
    fn retargeting_another_build_clears_the_caches() {
        let root = build_dir("retarget-a");
        let other = build_dir("retarget-b");
        let (mut state, _) = state_on(&root, DashboardTab::Kconfig);
        state.pane_mut().input = "NET".to_string();
        assert_eq!(state.rows().len(), 1);

        assert!(!state.retarget(&root, "build"), "the same build is a no-op");
        assert_eq!(state.rows().len(), 1, "and keeps the filter");

        assert!(state.retarget(&other, "build"));
        assert_eq!(state.tab, DashboardTab::Kconfig, "the tab survives");
        assert!(state.pane().input.is_empty(), "the filter does not");
        assert!(state.rows().is_empty(), "nothing is loaded yet");
    }

    /// A filter that shortens the list must not leave the cursor past its
    /// end --- and the tab's own cursor is its own.
    #[test]
    fn the_cursor_is_clamped_and_kept_per_tab() {
        let root = build_dir("cursor");
        let paths = ReportPaths::new(&root, "build");
        let mut state = DashboardState::default();
        state.retarget(&root, "build");

        state.tab = DashboardTab::Kconfig;
        state.ensure_tab(&paths);
        state.move_cursor(i32::MAX);
        assert_eq!(state.pane().selected, 2);

        state.tab = DashboardTab::ElfStats;
        state.ensure_tab(&paths);
        assert_eq!(state.pane().selected, 0, "another tab, another cursor");

        state.tab = DashboardTab::Kconfig;
        assert_eq!(
            state.pane().selected,
            2,
            "and the first one is where it was"
        );

        state.pane_mut().input = "BOARD".to_string();
        state.clamp_cursor();
        assert_eq!(state.pane().selected, 0);
    }

    #[test]
    fn the_strip_clamps_at_both_ends_and_never_wraps() {
        assert_eq!(DashboardTab::Summary.stepped(-1), DashboardTab::Summary);
        assert_eq!(DashboardTab::Summary.stepped(1), DashboardTab::Memory);
        assert_eq!(DashboardTab::ElfStats.stepped(1), DashboardTab::ElfStats);
        assert_eq!(DashboardTab::ElfStats.stepped(-1), DashboardTab::DeviceTree);
        for (position, tab) in DashboardTab::ALL.into_iter().enumerate() {
            assert_eq!(tab.index(), position);
        }
    }

    /// A flat tab answers neither expansion arrow, whatever the row.
    #[test]
    fn expansion_arrows_do_nothing_on_a_flat_tab() {
        let root = build_dir("flat");
        let (mut state, _) = state_on(&root, DashboardTab::Kconfig);
        assert!(!state.is_tree());
        state.pane_mut().selected = 1;
        state.expand_selected();
        state.collapse_selected();
        assert_eq!(state.pane().selected, 1);
        assert!(!state.toggle_selected());
    }
}
