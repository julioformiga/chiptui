//! Graphical simulator preparation without hardware or a system Zephyr install.
use std::path::{Path, PathBuf};

use chiptui::app::{App, Overlay};
use chiptui::backend::zephyr::{
    simulator,
    variants::{Variant, VariantOrigin},
};
use chiptui::backend::{BackendKind, Capability};
use chiptui::project::config;
use chiptui::project_config::ProjectConfigRow;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{TempDir, ctrl, key, render};

fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join(".west")).unwrap();
    for (file, contents) in [
        (
            "VERSION",
            "VERSION_MAJOR = 4\nVERSION_MINOR = 4\nPATCHLEVEL = 0\n",
        ),
        (
            "boards/native/native_sim/native_sim.dts",
            "sdl_dc: sdl_dc {};\ninput_sdl_touch: input-sdl-touch {};\n",
        ),
        (
            "modules/lvgl/Kconfig",
            "config LV_Z_AUTO_INIT\nconfig LV_COLOR_DEPTH_32\n",
        ),
        ("modules/lvgl/Kconfig.memory", "config LV_Z_MEM_POOL_SIZE\n"),
        ("modules/lvgl/lvgl.c", "lv_display_create();\n"),
    ] {
        let path = workspace.join("zephyr").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    workspace
}

fn setup(root: &Path) -> (App, PathBuf) {
    let workspace = workspace(root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("chiptui.toml"), format!("# team configuration\nproject_type = \"zephyr\"\n\n[zephyr]\nworkspace = \"{}\"\nboard = \"xiao_esp32c3\"\n", workspace.display())).unwrap();
    let mut app = App::new(&project);
    app.set_home_dir(root.join("home"));
    app.set_serial_dir(root.join("dev"));
    app.set_device_tool_paths(common::fake("mpremote-no-devices"), common::fake_esptool());
    app.bootstrap();
    app.maybe_scan_devices();
    if let Some(build) = &mut app.build {
        build.set_tool_path(common::fake("west"));
    }
    app.open_project_config(false);
    (app, project)
}

fn open_simulator(app: &mut App) {
    let panel = app.project_config.as_mut().unwrap();
    if panel.sdl_program == "pkg-config" {
        panel.sdl_program = common::fake("pkg-config-sdl");
    }
    let index = panel
        .rows()
        .iter()
        .position(|r| *r == ProjectConfigRow::Simulator)
        .unwrap();
    panel.select(index);
    app.handle(key(KeyCode::Enter));
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_edit
            .is_some()
    );
}

fn prepare_keys(app: &mut App) {
    // The form opens on Name (1), and Prepare is row 4.
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
}

fn prepare(app: &mut App) {
    // Staging hands the window straight to the transaction's review, over
    // the parent.
    prepare_keys(app);
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_edit
            .is_none()
    );
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_pending()
            .is_some()
    );
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmApplyConfig { .. })
    ));
}

fn apply(app: &mut App) {
    // `ctrl+s` opens the review when it is not already open (Prepare's own
    // hand-off), and is a no-op while it is.
    app.handle(ctrl('s'));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmApplyConfig { .. })
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(app.project_config.as_ref().unwrap().error().is_none());
}

#[test]
fn empty_project_stages_graphical_sources_and_keeps_a_device_variant() {
    let temp = TempDir::new("sim-empty");
    let (mut app, project) = setup(&temp);
    let original = std::fs::read_to_string(project.join("chiptui.toml")).unwrap();
    open_simulator(&mut app);
    assert!(render(&mut app, 80, 24).contains("LVGL + SDL"));
    prepare(&mut app);
    // The review stacks over the parent. At 80x24 it covers the parent's
    // title outright, so the stacking is read at a wider frame.
    let review = render(&mut app, 120, 40);
    assert!(review.contains("Apply these changes?"), "{review}");
    assert!(review.contains("Project configuration"), "{review}");
    assert!(render(&mut app, 80, 24).contains("Apply these changes?"));
    assert_eq!(
        std::fs::read_to_string(project.join("chiptui.toml")).unwrap(),
        original
    );
    assert!(!project.join("boards").exists());
    assert!(
        app.config_review_lines()
            .iter()
            .any(|line| line.contains("src/main.c"))
    );
    apply(&mut app);
    let text = std::fs::read_to_string(project.join("chiptui.toml")).unwrap();
    assert!(text.starts_with(&original));
    let variants = config::parse_variants(&text);
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].name, "hardware");
    assert_eq!(
        variants[1].board.as_deref(),
        Some(simulator::DEFAULT_TARGET)
    );
    assert!(
        std::fs::read_to_string(project.join("src/main.c"))
            .unwrap()
            .contains("lv_screen_active")
    );
    assert!(
        project
            .join("boards/native_sim_native_64.overlay")
            .is_file()
    );
    assert_eq!(app.build.as_ref().unwrap().variants.len(), 2);
    assert_eq!(
        app.build.as_ref().unwrap().board.as_ref().unwrap().name,
        "xiao_esp32c3"
    );
}

#[test]
fn existing_project_sources_and_common_config_are_never_migrated() {
    let temp = TempDir::new("sim-existing");
    let (mut app, project) = setup(&temp);
    std::fs::create_dir_all(project.join("src")).unwrap();
    for (file, body) in [
        ("CMakeLists.txt", "find_package(Zephyr REQUIRED)\n"),
        ("prj.conf", "CONFIG_WIFI=y\n"),
        ("src/main.c", "// user's application\n"),
    ] {
        std::fs::write(project.join(file), body).unwrap();
    }
    open_simulator(&mut app);
    prepare(&mut app);
    apply(&mut app);
    for (file, body) in [
        ("CMakeLists.txt", "find_package(Zephyr REQUIRED)\n"),
        ("prj.conf", "CONFIG_WIFI=y\n"),
        ("src/main.c", "// user's application\n"),
    ] {
        assert_eq!(std::fs::read_to_string(project.join(file)).unwrap(), body);
    }
    // Reopening and applying is an upsert, never a second simulator block.
    open_simulator(&mut app);
    prepare(&mut app);
    apply(&mut app);
    assert_eq!(
        config::parse_variants(&std::fs::read_to_string(project.join("chiptui.toml")).unwrap())
            .len(),
        2
    );
}

#[test]
fn cancellation_and_backend_switch_discard_the_entire_preparation() {
    let temp = TempDir::new("sim-cancel");
    let (mut app, project) = setup(&temp);
    open_simulator(&mut app);
    app.handle(key(KeyCode::Esc));
    assert!(!app.project_config.as_ref().unwrap().is_dirty());
    open_simulator(&mut app);
    prepare(&mut app);
    // Esc on the review declines it, back to the parent with the staged
    // preparation still pending; Esc again is the leave question.
    app.handle(key(KeyCode::Esc));
    assert!(matches!(app.overlay, Some(Overlay::ProjectConfig)));
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_pending()
            .is_some()
    );
    app.handle(key(KeyCode::Esc));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmDiscardConfig { .. })
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(!project.join("boards").exists());
    app.open_project_config(false);
    open_simulator(&mut app);
    prepare(&mut app);
    app.handle(key(KeyCode::Esc));
    app.handle(key(KeyCode::Char('b')));
    app.handle(key(KeyCode::Left));
    app.handle(key(KeyCode::Enter));
    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(panel.chosen(), Some(BackendKind::MicroPython));
    assert!(panel.simulator_pending().is_none());
    assert!(!panel.rows().contains(&ProjectConfigRow::Simulator));
}

#[test]
fn nested_application_keeps_config_and_builds_at_repository_root() {
    let temp = TempDir::new("sim-nested");
    let (mut app, project) = setup(&temp);
    std::fs::create_dir_all(project.join("app/src")).unwrap();
    std::fs::write(
        project.join("app/CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    std::fs::write(project.join("app/src/main.c"), "// preserved\n").unwrap();
    open_simulator(&mut app);
    prepare(&mut app);
    apply(&mut app);
    assert!(
        project
            .join("app/boards/native_sim_native_64.conf")
            .is_file()
    );
    assert!(!project.join("boards").exists());
    assert!(!project.join("app/chiptui.toml").exists());
    let v = config::parse_variants(&std::fs::read_to_string(project.join("chiptui.toml")).unwrap());
    assert_eq!(
        v[1].executable(&project),
        project.join("build_sim/zephyr/zephyr.exe")
    );
}

#[test]
fn incompatible_workspace_and_directory_conflicts_are_explained_without_writes() {
    let temp = TempDir::new("sim-conflict");
    let (mut app, project) = setup(&temp);
    std::fs::remove_file(temp.join("workspace/zephyr/modules/lvgl/lvgl.c")).unwrap();
    open_simulator(&mut app);
    prepare_keys(&mut app);
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_edit
            .as_ref()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("compatible Zephyr")
    );
    assert!(!project.join("boards").exists());
    workspace(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("boards/native_sim_native_64.conf")).unwrap();
    // Declare the list to avoid any catalogue dependence in this conflict test.
    let file = project.join("chiptui.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        format!("{text}\n[[variant]]\nname = \"hardware\"\nboard = \"xiao_esp32c3\"\n"),
    )
    .unwrap();
    app.open_project_config(false);
    open_simulator(&mut app);
    prepare_keys(&mut app);
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_edit
            .as_ref()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("not a regular file")
    );
    assert_eq!(
        config::parse_variants(&std::fs::read_to_string(file).unwrap()).len(),
        1
    );
}

#[test]
fn external_config_change_is_not_overwritten_by_apply() {
    let temp = TempDir::new("sim-concurrent");
    let (mut app, project) = setup(&temp);
    open_simulator(&mut app);
    prepare(&mut app);
    let file = project.join("chiptui.toml");
    std::fs::write(&file, "# changed in editor\nproject_type = \"zephyr\"\n").unwrap();
    app.handle(ctrl('s'));
    app.handle(key(KeyCode::Char('y')));
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .error()
            .unwrap()
            .contains("changed outside")
    );
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "# changed in editor\nproject_type = \"zephyr\"\n"
    );
    assert!(!project.join("boards").exists());
}

#[test]
fn simulator_form_and_full_review_survive_resize_and_scroll() {
    let temp = TempDir::new("sim-resize");
    let (mut app, _) = setup(&temp);
    open_simulator(&mut app);
    for (width, height) in [(120, 40), (80, 24), (100, 32)] {
        let frame = render(&mut app, width, height);
        assert!(frame.contains("Simulator settings"));
        assert!(frame.contains("Project configuration"), "{frame}");
        // Check the actual buttons together, not the explanatory prose that
        // also says "Prepare" even when the button labels have been clipped.
        assert!(
            frame
                .lines()
                .any(|line| line.contains("│ Prepare │") && line.contains("│ Cancel │")),
            "{frame}"
        );
    }
    prepare(&mut app);
    app.handle(ctrl('s'));
    assert!(render(&mut app, 80, 24).contains("review all changes"));
    // At most eight page steps; stop as soon as the bottom is reached.
    for _ in 0..8 {
        let panel = app.project_config.as_ref().unwrap();
        if panel.review_scroll == panel.review_max_scroll {
            break;
        }
        app.handle(key(KeyCode::PageDown));
        render(&mut app, 80, 24);
    }
    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(panel.review_scroll, panel.review_max_scroll);
    assert!(render(&mut app, 80, 24).contains("peripheral"));
    app.handle(key(KeyCode::Esc));
    assert!(matches!(app.overlay, Some(Overlay::ProjectConfig)));
}

#[test]
fn surgical_writer_preserves_other_blocks_unknown_keys_comments_and_blank_lines() {
    let text = "# lead\n[[variant]]\nname = 'hardware'\nboard = 'xiao_esp32c3'\ncustom = 42\n\n[[variant]] # simulator\n# keep this comment\nname = 'sim' # short name\nboard = 'native_sim/native/64'\nbuild_dir = 'build_sim'\ncustom = 'keep'\n\n[future]\nunknown = true\n";
    let mut v = simulator::default_variant();
    v.name = "desktop".into();
    let updated = config::upsert_variant(text, Some("sim"), &v).unwrap();
    assert_eq!(
        updated,
        text.replace(
            "name = 'sim' # short name",
            "name = \"desktop\" # short name"
        )
    );
    assert_eq!(
        config::upsert_variant(&updated, Some("desktop"), &v).unwrap(),
        updated
    );
    assert!(config::upsert_variant(text, Some("absent"), &v).is_err());
    assert!(config::upsert_variant(text, None, &simulator::default_variant()).is_err());
    let duplicate = format!("{text}\n[[variant]]\nname = 'sim'\n");
    assert!(config::upsert_variant(&duplicate, Some("sim"), &v).is_err());
}

#[test]
fn preparation_retains_discovered_hardware_and_rejects_colliding_names_and_build_dirs() {
    let temp = TempDir::new("sim-retain");
    let hardware = Variant {
        name: "device".into(),
        board: Some("xiao_esp32c3".into()),
        shield: Some("seeed_xiao_round_display".into()),
        build_dir: "build".into(),
        origin: VariantOrigin::Discovered,
    };
    let plan = simulator::Preparation::new(
        &temp,
        &temp,
        simulator::default_variant(),
        None,
        &[],
        vec![hardware.clone()],
    )
    .unwrap();
    let variants = config::parse_variants(&plan.config_text("# untouched\n").unwrap());
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].shield, hardware.shield);
    let mut sim = simulator::default_variant();
    sim.build_dir = "build/sim".into();
    assert!(
        simulator::Preparation::new(
            &temp,
            &temp,
            sim,
            None,
            std::slice::from_ref(&hardware),
            vec![]
        )
        .is_err()
    );
    let mut sim = simulator::default_variant();
    sim.name = "device".into();
    assert!(simulator::Preparation::new(&temp, &temp, sim, None, &[hardware], vec![]).is_err());
}

#[cfg(unix)]
#[test]
fn simulator_scaffold_refuses_symlink_escapes() {
    let temp = TempDir::new("sim-symlink");
    let project = temp.join("project");
    let external = temp.join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    std::os::unix::fs::symlink(&external, project.join("boards")).unwrap();
    assert!(
        simulator::Preparation::new(
            &project,
            &project,
            simulator::default_variant(),
            None,
            &[],
            vec![]
        )
        .unwrap_err()
        .contains("symlink")
    );
    assert!(!external.join("native_sim_native_64.conf").exists());
}

#[test]
fn micropython_does_not_offer_simulator_preparation() {
    let registry = chiptui::backend::BackendRegistry::with_builtin_backends();
    assert!(
        registry
            .capabilities(Some(BackendKind::Zephyr))
            .contains(Capability::SimulatorPrepare)
    );
    assert!(
        !registry
            .capabilities(Some(BackendKind::MicroPython))
            .contains(Capability::SimulatorPrepare)
    );
}

#[test]
fn simulator_icon_and_nested_form_follow_keyboard_and_mouse_navigation() {
    let temp = TempDir::new("sim-navigation");
    let (mut app, _) = setup(&temp);
    open_simulator(&mut app);
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Delete));
    app.handle(key(KeyCode::Char('x')));
    app.handle(key(KeyCode::Esc));
    let editor = app
        .project_config
        .as_ref()
        .unwrap()
        .simulator_edit
        .as_ref()
        .unwrap();
    assert_eq!(editor.values[0], "sim");
    assert!(!editor.editing);
    app.handle(key(KeyCode::Esc));
    let parent = render(&mut app, 120, 40);
    assert!(
        parent.lines().any(|line| line.contains("◉ Simulator")),
        "{parent}"
    );
    open_simulator(&mut app);
    app.set_mouse_enabled(true);
    let frame = render(&mut app, 80, 24);
    let (row, column) = common::find_cell(&frame, "│ Cancel │").unwrap();
    for _ in 0..2 {
        app.handle(chiptui::event::AppEvent::Mouse(common::click(
            column + 2,
            row,
        )));
    }
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_edit
            .is_none()
    );
    assert!(!app.project_config.as_ref().unwrap().is_dirty());
    assert!(render(&mut app, 80, 24).contains("Project configuration"));
}

#[test]
fn lone_discovered_simulator_is_editable_and_retains_a_separate_device_directory() {
    let temp = TempDir::new("sim-lone");
    let (mut app, project) = setup(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    std::fs::create_dir(project.join("build")).unwrap();
    std::fs::write(
        project.join("build/CMakeCache.txt"),
        "CACHED_BOARD:STRING=native_sim/native/64\n",
    )
    .unwrap();
    open_simulator(&mut app);
    let editor = app
        .project_config
        .as_mut()
        .unwrap()
        .simulator_edit
        .as_mut()
        .unwrap();
    assert!(editor.original_name().is_some());
    editor.values[2] = "build_sim".into();
    prepare(&mut app);
    apply(&mut app);
    let variants =
        config::parse_variants(&std::fs::read_to_string(project.join("chiptui.toml")).unwrap());
    assert_eq!(variants.len(), 2);
    assert_ne!(variants[0].build_dir, "build");
    assert_ne!(variants[0].build_dir, variants[1].build_dir);
}

#[test]
fn quoted_hashes_are_not_rewritten_as_comments() {
    let text = "[[variant]]\nname = 'sim#old' # actual comment\nboard = 'native_sim/native/64'\nbuild_dir = 'build_sim'\ncustom = 'keep#this'\n";
    let updated =
        config::upsert_variant(text, Some("sim#old"), &simulator::default_variant()).unwrap();
    assert_eq!(updated, text.replace("'sim#old'", "\"sim\""));
}

#[test]
fn failed_scalar_write_keeps_preparation_retryable() {
    let temp = TempDir::new("sim-retry");
    let (mut app, project) = setup(&temp);
    open_simulator(&mut app);
    prepare(&mut app);
    // Decline the review so the parent can take another change.
    app.handle(key(KeyCode::Esc));
    let panel = app.project_config.as_mut().unwrap();
    let index = panel
        .rows()
        .iter()
        .position(|r| *r == ProjectConfigRow::Mouse)
        .unwrap();
    panel.select(index);
    app.handle(key(KeyCode::Right));
    // A directory at the user config file path makes its scalar write fail.
    let user_file = temp.join("home/.config/chiptui/config.toml");
    let previous = std::fs::read_to_string(&user_file).unwrap_or_default();
    if user_file.is_file() {
        std::fs::remove_file(&user_file).unwrap();
    }
    std::fs::create_dir_all(&user_file).unwrap();
    app.handle(ctrl('s'));
    app.handle(key(KeyCode::Char('y')));
    assert!(app.project_config.as_ref().unwrap().error().is_some());
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .simulator_pending()
            .is_some()
    );
    assert!(!project.join("boards").exists());
    std::fs::remove_dir(&user_file).unwrap();
    std::fs::write(&user_file, previous).unwrap();
    apply(&mut app);
    assert_eq!(app.build.as_ref().unwrap().variants.len(), 2);
}

#[test]
fn removing_a_declared_simulator_keeps_every_file_and_says_so() {
    let temp = TempDir::new("sim-remove");
    let (mut app, project) = setup(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    // Prepare once, so the variant and its fragments exist on disk.
    open_simulator(&mut app);
    prepare(&mut app);
    apply(&mut app);
    let fragment = project.join("boards/native_sim_native_64.conf");
    assert!(fragment.is_file());

    app.open_project_config(false);
    open_simulator(&mut app);
    // The declared sim is the first choice; Remove is the middle button.
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmApplyConfig { .. })
    ));
    let review = render(&mut app, 120, 40);
    assert!(review.contains("remove simulator variant"), "{review}");
    assert!(review.contains("kept on disk"), "{review}");
    assert!(review.contains("native_sim_native_64.conf"), "{review}");
    app.handle(key(KeyCode::Char('y')));
    let text = std::fs::read_to_string(project.join("chiptui.toml")).unwrap();
    let variants = config::parse_variants(&text);
    assert_eq!(variants.len(), 1, "{text}");
    assert_eq!(variants[0].name, "hardware");
    // The declaration is gone; the files are the user's, and stay.
    assert!(fragment.is_file());
    assert!(app.project_config.as_ref().unwrap().error().is_none());
}

#[test]
fn remove_is_refused_for_new_and_discovered_variants() {
    let temp = TempDir::new("sim-remove-dim");
    let (mut app, project) = setup(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    // A discovered-only simulator: the declaration does not exist.
    std::fs::create_dir_all(project.join("build/zephyr")).unwrap();
    std::fs::write(
        project.join("build/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=native_sim/native/64\n",
    )
    .unwrap();
    open_simulator(&mut app);
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    let panel = app.project_config.as_ref().unwrap();
    let editor = panel.simulator_edit.as_ref().unwrap();
    assert!(editor.error.as_ref().unwrap().contains("discovered"));
    assert!(panel.simulator_pending().is_none());
    // "(new simulator)" has nothing to remove either.
    for _ in 0..5 {
        app.handle(key(KeyCode::Up));
    }
    app.handle(key(KeyCode::Right));
    for _ in 0..5 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    let panel = app.project_config.as_ref().unwrap();
    assert!(
        panel
            .simulator_edit
            .as_ref()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("nothing to remove")
    );
    assert!(panel.simulator_pending().is_none());
}

#[test]
fn a_declined_removal_stays_pending_and_del_undoes_it() {
    let temp = TempDir::new("sim-remove-decline");
    let (mut app, project) = setup(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    open_simulator(&mut app);
    prepare(&mut app);
    apply(&mut app);

    app.open_project_config(false);
    open_simulator(&mut app);
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    // Declining the review keeps the staged removal, visible on the row.
    app.handle(key(KeyCode::Esc));
    assert!(matches!(app.overlay, Some(Overlay::ProjectConfig)));
    let frame = render(&mut app, 120, 40);
    assert!(frame.contains("remove sim (pending)"), "{frame}");
    // Del on the Simulator row undoes the staged removal.
    app.handle(key(KeyCode::Delete));
    assert!(!app.project_config.as_ref().unwrap().is_dirty());
}

#[test]
fn removing_the_last_declaration_warns_about_rediscovery() {
    let temp = TempDir::new("sim-remove-last");
    let (mut app, project) = setup(&temp);
    std::fs::write(
        project.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    // The file declares the sim alone; its build directory exists.
    let file = project.join("chiptui.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        format!("{text}\n[[variant]]\nname = \"sim\"\nboard = \"native_sim/native/64\"\nbuild_dir = \"build_sim\"\n"),
    )
    .unwrap();
    std::fs::create_dir_all(project.join("build_sim/zephyr")).unwrap();
    std::fs::write(
        project.join("build_sim/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=native_sim/native/64\n",
    )
    .unwrap();
    app.open_project_config(false);
    open_simulator(&mut app);
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    let review = render(&mut app, 120, 40);
    assert!(review.contains("reappear as"), "{review}");
    app.handle(key(KeyCode::Char('y')));
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(config::parse_variants(&text).is_empty(), "{text}");
    assert!(text.contains("board = \"xiao_esp32c3\""), "{text}");
}

#[test]
fn sdl_probe_reports_failure_and_cancel_releases_its_process() {
    let temp = TempDir::new("sim-probe");
    let (mut app, _) = setup(&temp);
    app.project_config.as_mut().unwrap().sdl_program =
        temp.join("missing-pkg-config").display().to_string();
    open_simulator(&mut app);
    assert!(common::pump_until(
        &mut app,
        |app| app.project_config.as_ref().unwrap().sdl_probe.is_none(),
        5
    ));
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .sdl_status
            .contains("SDL2 check failed")
    );
    assert!(render(&mut app, 80, 24).contains("SDL2 check failed"));
    app.handle(key(KeyCode::Esc));
    open_simulator(&mut app);
    app.handle(key(KeyCode::Esc));
    assert!(app.project_config.as_ref().unwrap().sdl_probe.is_none());
}
