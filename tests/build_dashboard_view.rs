//! The build dashboard window (`Overlay::BuildDashboard`): the Zephyr build
//! report read inside the terminal, over the artifacts the build wrote.
//!
//! Every click test finds its target in the *drawn* frame and clicks that
//! column --- byte offsets are not columns, the borders being multi-byte.

mod common;

use chiptui::app::AppEvent;
use chiptui::app::{App, DocsFocus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::build_dashboard::DashboardTab;
use common::{click, fake, key, key_event, render, settle_while};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

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
  ["CONFIG_NET_L2_ETHERNET","y","bool","y","select",["WIFI_ESP32 && !SMP"]],
  ["CONFIG_LV_COLOR_DEPTH_32","y","bool",null,"unset",null]
]"#;

const REPORT: &str = r#"{"symbols":{"name":"Root","size":300,"identifier":"root","loc":[],
  "children":[{"name":"kernel","size":300,"identifier":"kernel","loc":[],
    "children":[
      {"name":"heap","size":200,"identifier":"kernel/heap","loc":["ram"],
       "address":16,"section":".bss"},
      {"name":"stack","size":100,"identifier":"kernel/stack","loc":["ram"],
       "address":32,"section":".bss"}]}]},
  "total_size":600}"#;

/// A Zephyr project whose build directory holds every artifact the window
/// reads, with the window already open.
fn dashboard_app(tag: &str) -> (App, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "chiptui-bdv-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let zephyr = root.join("build/zephyr");
    std::fs::create_dir_all(&zephyr).unwrap();
    std::fs::create_dir_all(root.join("build/dashboard")).unwrap();
    std::fs::write(root.join("CMakeLists.txt"), "find_package(Zephyr)\n").unwrap();
    std::fs::write(root.join("prj.conf"), "CONFIG_GPIO=y\n").unwrap();
    std::fs::write(
        root.join("build/build_info.yml"),
        "cmake:\n  board:\n    name: 'xiao_esp32c3'\n    qualifiers: 'esp32c3'\n\
         \x20 zephyr:\n    version: '4.4.99'\nwest:\n  command: '/v/bin/west build -b x'\n",
    )
    .unwrap();
    std::fs::write(zephyr.join("zephyr.stat"), STAT).unwrap();
    std::fs::write(zephyr.join(".config-trace.json"), TRACE).unwrap();
    std::fs::write(zephyr.join("zephyr.dts"), DTS).unwrap();
    std::fs::write(zephyr.join("zephyr.elf"), b"elf").unwrap();
    std::fs::write(root.join("build/dashboard/all_report.json"), REPORT).unwrap();

    // Startup must not look at the machine's real /dev or $HOME --- the
    // `zephyr_app` discipline every Zephyr test here follows.
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    app.place_startup_focus();

    // In through the menu's own row, so the door is what the tests exercise.
    app.overlay = Some(Overlay::ZephyrActions { selected: 2 });
    app.handle(key(KeyCode::Enter));
    (app, root)
}

fn chord(code: KeyCode) -> chiptui::app::AppEvent {
    key_event(code, KeyModifiers::CONTROL)
}

fn frame(app: &mut App) -> String {
    render(app, 80, 32)
}

fn wide(app: &mut App) -> String {
    render(app, 120, 40)
}

/// The menu's third row opens the window rather than starting a command.
#[test]
fn the_menu_row_opens_the_window() {
    let (app, root) = dashboard_app("open");
    assert!(matches!(app.overlay, Some(Overlay::BuildDashboard)));
    assert!(
        !app.build.as_ref().is_some_and(|panel| panel.is_busy()),
        "nothing is run to read files that already exist"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The window fits the declared 80x32 minimum: the whole strip, both panes,
/// the filter and the hint, with no row lost and no label cut off the strip.
#[test]
fn the_window_fits_the_declared_minimum() {
    let (mut app, root) = dashboard_app("min");
    let frame = frame(&mut app);
    for tab in DashboardTab::ALL {
        assert!(
            frame.contains(tab.label()),
            "the strip must show {}:\n{frame}",
            tab.label()
        );
    }
    assert!(frame.contains("Details"), "the details pane:\n{frame}");
    assert!(
        frame.contains("ctrl+"),
        "the hint names the chord that switches tabs:\n{frame}"
    );
    assert!(
        frame.contains("xiao_esp32c3"),
        "the Summary opens on the board:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A row keeps its label whatever its value: the Summary's application path
/// and build command are long enough to take the whole width, and a row
/// without a name is not a row (the value is in the details pane anyway).
#[test]
fn a_long_value_never_costs_a_row_its_label() {
    let (mut app, root) = dashboard_app("long-values");
    let frame = frame(&mut app);
    assert!(
        frame.contains("Build command"),
        "the label survives a long value:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The Summary's memory figures come from `zephyr.stat`, so the window and
/// the HTML report describe one build the same way.
#[test]
fn the_summary_shows_the_memory_buckets() {
    let (mut app, root) = dashboard_app("summary");
    let frame = wide(&mut app);
    for label in ["text", "rodata", "rwdata", "bss"] {
        assert!(frame.contains(label), "{label} is a row:\n{frame}");
    }
    assert!(
        frame.contains("4.4.99"),
        "the Zephyr version is a Summary fact:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The strip answers the dashboard-wide chord, and only the chord: the plain
/// arrows belong to the list, the details and the two trees.
#[test]
fn the_chord_walks_the_strip_and_plain_arrows_do_not() {
    let (mut app, root) = dashboard_app("chord");
    assert_eq!(app.build_dashboard.tab, DashboardTab::Summary);

    app.handle(key(KeyCode::Right));
    assert_eq!(
        app.build_dashboard.tab,
        DashboardTab::Summary,
        "a plain arrow is not a tab switch"
    );

    app.handle(chord(KeyCode::Right));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Memory);
    app.handle(chord(KeyCode::Right));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Kconfig);
    app.handle(chord(KeyCode::Left));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Memory);

    // Clamped at both ends, never wrapping --- every strip's rule here.
    app.handle(chord(KeyCode::Left));
    app.handle(chord(KeyCode::Left));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Summary);
    let _ = std::fs::remove_dir_all(&root);
}

/// Switching to a tab loads it, and the Kconfig details name the origin the
/// trace file recorded --- a `select` carries expressions, not a location.
#[test]
fn the_kconfig_tab_loads_and_details_a_selected_symbol() {
    let (mut app, root) = dashboard_app("kconfig");
    app.handle(chord(KeyCode::Right));
    app.handle(chord(KeyCode::Right));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Kconfig);

    let frame = wide(&mut app);
    assert!(
        frame.contains("CONFIG_BOARD"),
        "the tab loaded on entry:\n{frame}"
    );
    assert!(
        frame.contains("3 symbols"),
        "the count rides the filter line:\n{frame}"
    );

    // Rows are sorted by name, so CONFIG_NET_L2_ETHERNET is the third.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    let frame = wide(&mut app);
    assert!(frame.contains("selected"), "the origin is named:\n{frame}");
    assert!(
        frame.contains("WIFI_ESP32"),
        "and a select shows the expression that forced it:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Every printable character is filter text --- which is why no action lives
/// on a plain letter, and why `q` does not close the window.
#[test]
fn typing_filters_and_q_is_text_not_an_exit() {
    let (mut app, root) = dashboard_app("filter");
    app.handle(chord(KeyCode::Right));
    app.handle(chord(KeyCode::Right));

    for c in "NET".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = wide(&mut app);
    assert!(frame.contains("CONFIG_NET_L2_ETHERNET"));
    assert!(
        !frame.contains("CONFIG_BOARD"),
        "the filter narrowed the list:\n{frame}"
    );

    app.handle(key(KeyCode::Char('q')));
    assert!(
        matches!(app.overlay, Some(Overlay::BuildDashboard)),
        "`q` is filter text here, not the way out"
    );
    app.handle(key(KeyCode::Esc));
    assert!(app.overlay.is_none(), "`Esc` is");
    let _ = std::fs::remove_dir_all(&root);
}

/// The devicetree opens with its root expanded and everything below shut;
/// `→` opens a node and reveals only its own children.
#[test]
fn the_devicetree_tab_expands_one_node_at_a_time() {
    let (mut app, root) = dashboard_app("dt");
    for _ in 0..3 {
        app.handle(chord(KeyCode::Right));
    }
    assert_eq!(app.build_dashboard.tab, DashboardTab::DeviceTree);

    let frame = wide(&mut app);
    assert!(frame.contains("soc"), "the root's children show:\n{frame}");
    assert!(!frame.contains("i2c@1"), "but nothing below them:\n{frame}");

    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Right));
    let frame = wide(&mut app);
    assert!(frame.contains("i2c@1"), "`→` opened the node:\n{frame}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Filtering a tree flattens it to full paths, so a match is reachable
/// without expanding a path to it.
#[test]
fn filtering_a_tree_flattens_it_to_full_paths() {
    let (mut app, root) = dashboard_app("dt-filter");
    for _ in 0..3 {
        app.handle(chord(KeyCode::Right));
    }
    for c in "i2c".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = wide(&mut app);
    assert!(
        frame.contains("/soc/i2c@1"),
        "the match shows with its whole path:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `Tab` swaps the keyboard between the two halves, the pickers' grammar.
#[test]
fn tab_hands_the_keyboard_between_the_panes() {
    let (mut app, root) = dashboard_app("focus");
    assert_eq!(app.build_dashboard.focus, DocsFocus::List);
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.build_dashboard.focus, DocsFocus::Details);

    // With the details focused the arrows scroll it instead of the list.
    let before = app.build_dashboard.pane().selected;
    app.handle(key(KeyCode::Down));
    assert_eq!(app.build_dashboard.pane().selected, before);
    assert_eq!(app.build_dashboard.pane().scroll, 1);

    app.handle(key(KeyCode::Tab));
    assert_eq!(app.build_dashboard.focus, DocsFocus::List);
    let _ = std::fs::remove_dir_all(&root);
}

/// A build with no memory report says so and names what would make one,
/// rather than showing an empty tree.
#[test]
fn a_missing_memory_report_is_a_named_state() {
    let (mut app, root) = dashboard_app("no-memory");
    std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    let frame = wide(&mut app);
    assert!(
        frame.contains("Generate the memory report"),
        "the tab explains itself with the row that fixes it:\n{frame}"
    );
    assert!(
        frame.contains("enter generates the report"),
        "and the hint names the key:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The ELF tab makes the Summary auditable: every section says which bucket
/// it counts toward, and `.text` is `WAX` on a real ESP32 build --- executable
/// wins over writable.
#[test]
fn the_elf_tab_says_which_bucket_a_section_counts_toward() {
    let (mut app, root) = dashboard_app("elf");
    for _ in 0..4 {
        app.handle(chord(KeyCode::Right));
    }
    assert_eq!(app.build_dashboard.tab, DashboardTab::ElfStats);
    // Rows are `[nr] name`; walk to `.text`.
    app.handle(key(KeyCode::Down));
    let frame = wide(&mut app);
    assert!(frame.contains(".text"), "the section list:\n{frame}");
    assert!(
        frame.contains("text (PROGBITS, executable)"),
        "and where it lands in the Summary:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The column a label starts at in the drawn frame --- byte offsets are not
/// columns, the borders being multi-byte.
fn find_label(frame: &str, needle: &str) -> (u16, u16) {
    for (row, line) in frame.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            let column = line[..at].chars().count() as u16;
            return (column, row as u16);
        }
    }
    panic!("{needle:?} is not drawn:\n{frame}");
}

/// A click on a strip label switches to that tab --- the strip is walked by
/// the same `(tab, title)` builder the renderer draws from.
#[test]
fn a_click_on_the_strip_switches_tab() {
    let (mut app, root) = dashboard_app("click-strip");
    app.set_mouse_enabled(true);
    let frame = wide(&mut app);
    let (column, row) = find_label(&frame, "Kconfig");
    app.handle(AppEvent::Mouse(click(column + 2, row)));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Kconfig);
    let _ = std::fs::remove_dir_all(&root);
}

/// A click on a row selects it and hands the list the keyboard; it never
/// activates --- the picker grammar every click in this app follows.
#[test]
fn a_click_on_a_row_selects_without_activating() {
    let (mut app, root) = dashboard_app("click-row");
    app.set_mouse_enabled(true);
    for _ in 0..3 {
        app.handle(chord(KeyCode::Right));
    }
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.build_dashboard.focus, DocsFocus::Details);

    let frame = wide(&mut app);
    let (column, row) = find_label(&frame, "soc");
    app.handle(AppEvent::Mouse(click(column, row)));
    assert_eq!(
        app.build_dashboard.focus,
        DocsFocus::List,
        "the click hands it back"
    );
    assert_eq!(app.build_dashboard.pane().selected, 1);
    let frame = wide(&mut app);
    assert!(
        !frame.contains("i2c@1"),
        "selecting is not expanding:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A click on the details pane hands it the keyboard --- the mouse's `Tab`.
#[test]
fn a_click_on_the_details_hands_it_the_keyboard() {
    let (mut app, root) = dashboard_app("click-details");
    app.set_mouse_enabled(true);
    let frame = wide(&mut app);
    let (column, row) = find_label(&frame, "Details");
    app.handle(AppEvent::Mouse(click(column + 4, row + 3)));
    assert_eq!(app.build_dashboard.focus, DocsFocus::Details);
    let _ = std::fs::remove_dir_all(&root);
}

/// A click outside the window closes it, exactly like `Esc` --- and it is
/// tested at a point that is on a *row* the list also occupies, which is
/// where a column-blind hit test would answer the row instead.
#[test]
fn a_click_outside_the_window_closes_it() {
    let (mut app, root) = dashboard_app("click-outside");
    app.set_mouse_enabled(true);
    let frame = wide(&mut app);
    let (_, row) = find_label(&frame, "Details");
    // Column 0 is outside the modal, which starts at column 1.
    app.handle(AppEvent::Mouse(click(0, row)));
    assert!(app.overlay.is_none(), "a click beside the window closes it");
    let _ = std::fs::remove_dir_all(&root);
}

/// The wheel steps the list under the pointer and scrolls the details ---
/// never moving the focus, the board picker's own wheel grammar.
#[test]
fn the_wheel_steps_the_list_and_scrolls_the_details() {
    let (mut app, root) = dashboard_app("wheel");
    app.set_mouse_enabled(true);
    for _ in 0..3 {
        app.handle(chord(KeyCode::Right));
    }
    let frame = wide(&mut app);
    let (column, row) = find_label(&frame, "soc");

    let mut down = click(column, row);
    down.kind = ratatui::crossterm::event::MouseEventKind::ScrollDown;
    app.handle(AppEvent::Mouse(down));
    assert_eq!(app.build_dashboard.pane().selected, 1);
    assert_eq!(
        app.build_dashboard.focus,
        DocsFocus::List,
        "the wheel never moves the focus"
    );

    let (dcolumn, drow) = find_label(&frame, "Details");
    let mut over_details = click(dcolumn + 4, drow + 3);
    over_details.kind = ratatui::crossterm::event::MouseEventKind::ScrollDown;
    app.handle(AppEvent::Mouse(over_details));
    assert_eq!(app.build_dashboard.pane().scroll, 1);
    assert_eq!(app.build_dashboard.focus, DocsFocus::List);
    let _ = std::fs::remove_dir_all(&root);
}

/// A workspace whose venv carries a `python` --- what the memory report
/// needs, since it runs a script out of the checkout rather than a console
/// script that embeds its own interpreter.
///
/// `interpreter` names which fixture stands in for it, which is the only
/// seam this feature has: the panel must *not* rewrite this command's
/// program the way it rewrites every other one with the resolved `west`.
fn workspace_with_interpreter(home: &std::path::Path, interpreter: &str) -> std::path::PathBuf {
    let dir = home.join("zephyrproject");
    std::fs::create_dir_all(dir.join(".west")).unwrap();
    std::fs::create_dir_all(dir.join("zephyr")).unwrap();
    std::fs::write(dir.join(".west/config"), "[manifest]\npath = zephyr\n").unwrap();
    std::fs::write(
        dir.join("zephyr/VERSION"),
        "VERSION_MAJOR = 4\nVERSION_MINOR = 4\nPATCHLEVEL = 0\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
    std::fs::copy(fake("west"), dir.join(".venv/bin/west")).unwrap();
    std::fs::copy(fake(interpreter), dir.join(".venv/bin/python")).unwrap();
    dir
}

/// An app whose workspace resolves and whose build panel runs the fake
/// `size_report` in place of the venv interpreter.
fn app_with_report_tool(tag: &str) -> (App, std::path::PathBuf) {
    app_with_interpreter(tag, "size-report")
}

fn app_with_interpreter(tag: &str, interpreter: &str) -> (App, std::path::PathBuf) {
    let (mut app, root) = dashboard_app(tag);
    let ws = workspace_with_interpreter(&root.join("home"), interpreter);
    std::fs::write(
        root.join("chiptui.toml"),
        format!("[zephyr]\nworkspace = \"{}\"\n", ws.display()),
    )
    .unwrap();
    app.overlay = None;
    app.workspace = None;
    app.build = None;
    app.maybe_scan_devices();
    // The seam is the workspace's own `.venv/bin/python`, which
    // `workspace_with_python` wrote the fake into --- *not* `set_tool_path`,
    // which overrides the program and is exactly the bug this test exists to
    // catch: `tool_path` is the resolved `west`, and rewriting the
    // interpreter with it made west read the script path as a subcommand.
    app.overlay = Some(Overlay::ZephyrActions { selected: 2 });
    app.handle(key(KeyCode::Enter));
    (app, root)
}

/// With no report on disk the Memory tab leads with a row that offers to
/// make one, and says what that costs.
#[test]
fn the_memory_tab_offers_to_generate_a_missing_report() {
    let (mut app, root) = dashboard_app("prompt");
    std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    let frame = wide(&mut app);
    assert!(
        frame.contains("Generate the memory report"),
        "the tab offers the run:\n{frame}"
    );
    assert!(
        frame.contains("size_report"),
        "and the details name what it runs:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `Enter` on that row closes the window, runs the command through the build
/// panel's one process slot, and re-opens on Memory with the fresh report.
#[test]
fn generating_the_report_closes_the_window_and_brings_it_back() {
    let (mut app, root) = app_with_report_tool("generate");
    std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    assert_eq!(app.build_dashboard.tab, DashboardTab::Memory);
    assert!(app.build_dashboard.selected_is_prompt());

    app.handle(key(KeyCode::Enter));
    assert!(
        app.overlay.is_none(),
        "a run of minutes belongs in the Monitor, not behind a modal"
    );
    assert!(app.build.as_ref().unwrap().is_busy());
    // The command names the script and the placeholder, not three paths ---
    // and it runs the *interpreter*, not `west`. The panel rewrites every
    // other command's program with the resolved west; this one must keep its
    // own, or west answers `unknown command "…/size_report"`.
    let line = app.build.as_ref().unwrap().output.front().unwrap().clone();
    assert!(
        line.starts_with("$ python "),
        "the program stays the venv interpreter: {line}"
    );
    assert!(line.contains("scripts/footprint/size_report"), "{line}");
    assert!(line.contains("{target}_report.json"), "{line}");
    assert!(line.contains("--workspace="), "{line}");

    settle_while(
        &mut app,
        |app| app.build.as_ref().is_some_and(|panel| panel.is_busy()),
        "the memory report",
    );
    for event in app.processes.drain() {
        app.handle(chiptui::app::AppEvent::Process(event));
    }
    assert!(
        matches!(app.overlay, Some(Overlay::BuildDashboard)),
        "the window comes back once the report exists"
    );
    assert_eq!(app.build_dashboard.tab, DashboardTab::Memory);
    let frame = wide(&mut app);
    assert!(
        frame.contains("kernel"),
        "and it shows the report that was just written:\n{frame}"
    );
    assert!(
        !frame.contains("Generate the memory report"),
        "the offer is gone, the report being current:\n{frame}"
    );
    // The tree opens where it left off: the root expanded, the rest shut.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Right));
    let frame = wide(&mut app);
    assert!(
        frame.contains("heap_all"),
        "and the `all` report is the one that was read:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A report older than the ELF is not thrown away: the offer to regenerate
/// leads, and the stale numbers stay readable below it.
#[test]
fn a_stale_report_keeps_its_rows_under_the_offer() {
    let (mut app, root) = dashboard_app("stale");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(root.join("build/zephyr/zephyr.elf"), b"rebuilt").unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    let frame = wide(&mut app);
    assert!(
        frame.contains("Regenerate"),
        "the offer names why:\n{frame}"
    );
    assert!(
        frame.contains("Root"),
        "the old numbers stay readable:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Without a resolved workspace there is no interpreter to run the script
/// with, and the refusal says so instead of failing silently.
#[test]
fn generating_without_a_workspace_is_a_named_refusal() {
    let (mut app, root) = dashboard_app("no-ws");
    std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    let before = app.logs.len();
    app.handle(key(KeyCode::Enter));
    assert!(
        !app.build.as_ref().is_some_and(|panel| panel.is_busy()),
        "nothing runs"
    );
    assert!(app.logs.len() > before, "and the log says why");
    let _ = std::fs::remove_dir_all(&root);
}

/// A failed run does **not** bring the window back: the Monitor holds the
/// explanation, and a modal over it would hide exactly what the reader
/// needs. The panel reports the failure in the log either way.
#[test]
fn a_failed_report_leaves_the_monitor_showing_instead_of_reopening() {
    let (mut app, root) = app_with_interpreter("generate-fails", "size-report-fails");
    std::fs::remove_file(root.join("build/dashboard/all_report.json")).unwrap();
    app.build_dashboard.invalidate_memory();
    app.handle(chord(KeyCode::Right));
    assert!(app.build_dashboard.selected_is_prompt());

    app.handle(key(KeyCode::Enter));
    assert!(app.overlay.is_none());
    settle_while(
        &mut app,
        |app| app.build.as_ref().is_some_and(|panel| panel.is_busy()),
        "the failing memory report",
    );
    for event in app.processes.drain() {
        app.handle(AppEvent::Process(event));
    }

    assert!(
        app.overlay.is_none(),
        "a failure must not cover the Monitor with the window"
    );
    // The run's own output is what explains it, and the log says where.
    let frame = wide(&mut app);
    assert!(
        frame.contains("MemoryError"),
        "the child's stderr reaches the Monitor:\n{frame}"
    );
    let report = app
        .build
        .as_ref()
        .and_then(|panel| panel.last.clone())
        .expect("the panel records the run");
    assert_eq!(report.what, "Memory report");
    assert!(!report.ok, "and records it as a failure");
    assert!(!report.cancelled, "which is not the same as a stop");
    let _ = std::fs::remove_dir_all(&root);
}
