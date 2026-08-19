# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

A session now has **two screens** (`src/main.rs` alternates them, one `TerminalGuard` for both).
`startup::route` decides which, before the terminal is taken over: a directory whose backend is
known (registry → `chiptui.toml` → evidence, nearest ancestor first) or ambiguous opens the
dashboard; an *empty* one opens it too, so `Overlay::ProjectSetup` can scaffold it; anything else
(a directory with contents and no project — `$HOME`, `~/Downloads`) opens the **home screen**
(`src/home.rs`, `src/ui/home.rs`): create row, live search field, then the recorded projects,
each row tinted with `BackendKind::palette(theme)` — the backend's semantic color (`success`/
`info`) blended toward the theme's own background, so the tint follows the active theme —
deepened under the cursor, never reversed — a painted row cannot also be inverted). Creating a
project is folder picker (the workspace pane's
`dir_rows`, starting at `[projects] last_parent`) → name → an empty directory that routes straight
back into the backend prompt. `del` forgets a registry entry, never the directory. `shift+P` on
the dashboard returns to the list (`App::request_home_screen` → `Overlay::ConfirmSwitchProject`
when commands are running → `switch_requested`, read by `main.rs`, which drops the `App` and with
it every child process). Answering the backend prompt writes the backend's own starting layout
(`Backend::scaffold` → `project::scaffold::create`, never overwriting) and records the project;
it no longer creates a `chiptui.toml`.

Phase 1 of `SPEC.md` §17 is done (core, TUI, detection, backend registry, capabilities), plus the
process manager and the first real device operation: a dual-pane local/device **file browser** for
MicroPython, with list/compare, a per-entry action menu (send to device, download, view, edit,
diff, delete) and a read-only viewer with lightweight syntax highlighting (`src/highlight.rs`) and
`$EDITOR` handoff (`src/editor.rs`, `src/terminal.rs`'s `TerminalGuard::suspend`). `Enter` opens the
menu for *any* entry now, not just text files — a directory gets it too, defaulted to `Open`, plus a
recursive send/download/delete (`Browser::request_upload_dir` and friends, `mpremote fs --recursive
cp`); a binary file (e.g. `.mpy`) still offers send/download/delete, just not view/edit, which stay
gated on `files::is_text_like` (`FileAction::for_entry`). `Diff` (a unified
diff of local vs device, `src/diff.rs`, coloured in the viewer) appears only
when the comparison verdict marks the file as differing or same-size-unchecked.
`→` stays pure navigation, separate from the
menu: it descends into a directory directly (a no-op on a file), mirroring `←`/Backspace going back up
— only `Enter` opens the menu. `a` creates a new
entry in the focused pane inline — a trailing `/` on the typed name makes it a directory
(`Browser::request_mkdir`/`request_touch`). The local pane is titled `Project files: name/path`
(`Browser::local_root`, re-rooted by `set_local_root`): MicroPython makes the project a
question too (`Capability::ProjectSelect`) --- `[micropython] projects` in the user config
(`settings::mpy_projects_raw`/`save_mpy_projects`) answers the Projects base row, a pick
from its subdirectories (`Overlay::ProjectPicker { mpy: true }`, any subdirectory qualifies)
re-roots the local pane session-only (`App::set_mpy_project`), and the pane's other two rows
report `Dependencies` (`requirements.txt`/`manifest.py` presence in the project root) and
`Script` (whether the board is believed to be running user code right now,
`DeviceState::script_state()`). The board's MicroPython version itself, parsed off the REPL
banner by `device::micropython_version` --- fed by the probe and the monitor, dropped with the
board on disconnect/switch --- rides the Device info pane's `Firmware` row instead (see below),
appended to the `MicroPython` label. Editing a device file downloads it to a scratch temp file
(`browser::edit_download_path`), never the project tree — the point is to prove a change on the
device first; `Download` is the separate, explicit step for landing a confirmed-good result in the
project. On a clean `$EDITOR` exit it re-uploads to the same device path and then offers a
`soft-reset`, defaulting to *no* (`Overlay::ConfirmRestartDevice`). A board believed to be
running a script is never interrupted silently: a short `mpremote repl` probe runs before the
first listing (`src/app/probe.rs`), device operations are held behind a confirmation while a
script runs (`Overlay::ConfirmInterruptDevice`), and an accepted interruption ends with a
restore prompt (`Overlay::RestoreDeviceScript`: hard reset, `import main`, or leave stopped).

Row 2 is capability-driven now: a backend that can build without a device filesystem (Zephyr)
claims the *whole* row with a **Project files | Project actions** pair (`maybe_scan_devices`/
`ensure_browser_scanning` skip the browser entirely for such a backend — listing/editing the
project's own sources is the user's editor's job; MicroPython's dual-pane browser and its
capability-gated `FileAction::for_entry` menu are unchanged). The environment's
prerequisites moved *up* into row 1's **Project pane**, which is the checklist now:
`Zephyr path`, `Projects base`, `Project path`, `Board · Shield` (the last two answered by
the build panel but asked here; `←`/`→` on the merged target row switch which half `Enter`
acts on --- `App::board_segment`) for Zephyr, `Projects base`/`Project path`/`Dependencies`/
`Script` for MicroPython (`src/app/project_view.rs`, `App::project_rows`). The pane is
navigable but deliberately off the `Tab` tour: `ctrl+p` enters it (toggle: a second press
returns to wherever focus was; the cursor lands on the first question still open), `Tab`
re-enters the tour at its first stop. The operation buttons they gate live in the **Project
actions** pane --- a small custom widget (`src/ui/button.rs`: one stacked
group sharing a rounded border, a centered icon label per row, a `├─┤` divider between each
pair --- N buttons cost 2N+1 lines, and `draw_dashboard`'s `row2_content_height` sizes row 2
to that content (the log pane, which scrolls, takes the remainder; the browser row keeps
60/40); the group's frame is the theme's muted color, labels bold `fg` (muted while disabled), and the selected row
is `palette.selection`/`palette.fg` (not `Modifier::REVERSED`, which read inconsistently
across terminals) applied as a `Buffer::set_style` patch over the row's inner cells *after*
the label is drawn, filling it edge to edge without ever painting over the side rules,
dividers or outer rules --- the same rule every list, picker and checklist row now follows
through the shared `ui::selection_style`/`ui::muted_style` helpers: selected rows draw on
`palette.selection`, secondary text (labels, legends, hints, timestamps) in `palette.muted`,
so the whole UI follows the active theme; the Monitor's fake terminal cursor keeps reverse
video on purpose, being the terminal's cursor rather than a selection), which stay
visible but dimmed until their answers exist (`WorkspacePanel::action_enabled`,
`App::build_action_enabled`) --- Enter on a dimmed row is a no-op; rows carry no trailing
text (the confirm overlays quote the literal commands, `SPEC.md` §15, not the rows). The
header's `project` field is the project question's other half (`App::header_project`):
empty while the root is not a buildable application, then the picked project's folder name
(the MicroPython pick answers it too). Row 1 itself is a fixed height: both
panes pad their content to four rows (`ui::panels::INFO_ROWS`) in every backend and state
(and the MicroPython cwd note rides `Project path`'s own line as a muted suffix), so the
rows below never shift when a workspace resolves or device details accumulate. The
environment's `versions` (Zephyr + venv Python, read from files) ride the pane's *bottom
border* right edge (`draw_versions_badge`, the same place the Log tab strip rides its top
border) once a workspace resolves; missing tools show as a red `⚠ N` count beside the
backend name in the header (`missing_tools`, over the same `App::tool_status` the startup
warning logs).
The **workspace pane**
(`src/workspace.rs`, `src/ui/workspace.rs`, `src/app/workspace_view.rs`) is the environment
half: it resolves the Zephyr *installation* (`src/backend/zephyr/workspace.rs`) from
configuration and nowhere else --- `chiptui.toml`'s `[zephyr] workspace`, then the user
config `~/.config/chiptui/config.toml` (both parsed by `src/settings.rs`); no directory
conventions, no `$ZEPHYR_BASE`. Startup focus lands on this pane (`App::place_startup_focus`, after
`maybe_scan_devices` in `main.rs`): the environment questions come first. When nothing is
configured, `main.rs` calls
`maybe_open_workspace_picker` right after: `Overlay::DirPicker` is a real
filesystem browser (`workspace::dir_rows`, starting at `$HOME`) where the user navigates
to the installation and accepts it; descending lands on the "use this directory" row so
the reflex `Enter` accepts the folder just entered. The accepted directory is validated by
the *same* `install_check` the config goes through (`.west/` present, the manifest's
checkout present) --- a failure keeps the picker open with the reason plus the
getting-started link (`workspace::GETTING_STARTED`), and a configured-but-broken location
marks the pane's row `✗ !` with the same message in the log (the pane is a fixed four
rows --- the reason does not get one). A validated pick is persisted
(`settings::save_workspace`, a line-level merge that preserves every other key/section) to
the user config, or to `chiptui.toml` when the project pins its own location --- so the
config stays the single source of truth and later starts never re-ask. Tool reporting
honors the same answer: every `App::report_tools` call site resolves the workspace first
(creating the panel early is what keeps startup from warning about a `west` that was never
on `PATH` because it lives in the workspace venv) and `App::tool_status` is the one
availability definition shared by that warning and the header's `⚠ N` badge. It
names no tool: a resolved workspace declares the tools whose *location* it owns
(`Workspace::tool_locations`, empty when resolution fell through to the bare program name)
and `BackendRegistry::tool_status(kind, located)` judges those files with
`backend::executable_at` --- the same "is it runnable" predicate (metadata: a file, with an
execute bit) a `PATH` lookup uses --- while every unlocated tool keeps the `PATH` answer.
`PATH` lookups skip empty entries, which mean the cwd; that makes the *report* stricter
than `execvp`, never the reverse.
`west update` (confirm-gated --- it rewrites the shared workspace,
through `Overlay::ConfirmWorkspace`) and `west sdk list` live as buttons in the Project
actions pane, enabled once the
installation resolves, under `Capability::WorkspaceSync`; both
run through the build panel's one process slot into the Monitor tab. The **Project files**
pane (the old workspace pane, `src/ui/workspace.rs`) is the project's own listing, whole:
its title carries the walked path (`Project files: proj/src/`, never truncating the
prefix), and the body is the list --- no action menu: `Enter` descends into a
directory and opens a text file straight in `$EDITOR`, `v` views one in the viewer, `Del`
asks through `Overlay::ConfirmDelete` (default No), and a binary/unknown entry ignores both
keys (all through `App::run_file_action`, `Side::Local`); `r` renames the entry under the
cursor --- any kind, via `Overlay::RenameEntry` pre-filled with the current name and a local
`fs::rename` on confirm (`App::rename_entry`; a `/` in the typed name is refused, since a
rename must not silently become a move), and a below-root listing leads with a `[..]` parent row
(`WorkspacePanel::parent_row`), selected after every descent, `Enter`/`→` on it stepping back
up. The pane also owns the
environment's second persisted fact: the **projects folder** (`[zephyr] projects`, resolved by
`src/backend/zephyr/projects.rs` from the same two config levels, or picked through the same
`DirPicker` with `DirPurpose::Projects` --- existence-only validation, saved via
`settings::save_projects`); accepting a folder chains straight into the project picker. Under
`Capability::ProjectSelect` every project command (build/clean/rebuild/menuconfig/flash) passes a
gate first (`App::require_buildable_project`): the build panel's root must hold build elements
(`projects::is_buildable` --- a `CMakeLists.txt`, `west build`'s one hard requirement); the
`Project path` checklist row doubles as the gate's explanation when the root does not. A root that
passes (the cwd when it *is* a project) builds without ceremony; one that does not is refused with
the reason and the flow opens --- folder picker when unconfigured, else `Overlay::ProjectPicker`
(all immediate subdirectories listed, each marked buildable or not; accepting a directory without
build elements keeps the picker open with the reason). A valid pick is session-only
(`BuildPanel::set_project`: re-roots every command, resets build dir/cached board/last report,
`ProjectOrigin::Picked` vs `WorkingDir` shown on the workspace checklist row) --- nothing is written. The
resolution feeds
the build panel's commands via
`BuildPanel::set_tool_path`/`set_tool_env`: the venv's `west` (`<workspace>/.venv/bin/west`,
executed directly — a venv console script embeds its interpreter path, so no activation is
needed) plus per-command env (`ZEPHYR_BASE` always, so an app outside the workspace still
finds it; `ZEPHYR_SDK_INSTALL_DIR`/`PATH`/`VIRTUAL_ENV` when applicable —
`process::Command::env`). The **project panel** (`src/build.rs`, `src/ui/build.rs`,
`src/app/build_view.rs`, titled "Project actions") is buttons only --- no checklist rows
(the board answer comes from the build dir's CMake cache (`cached_board` ---
`<build-dir>/zephyr/CMakeCache.txt`, falling
back to the sysbuild top-level cache; a hand-picked board is session state no finished
command demotes) or a hand pick, and gates
Update Zephyr/SDK List/Menuconfig/Clean/Build/Rebuild/Flash (the list's order; while a
command runs `Stop` is appended and appears in the pane's three-row footer, always reserved
--- the pane's height never changes when a command starts --- hugging the stack's bottom rule
and split horizontally: the state line on the left half, `Stop` as its own half-width button
box on the right, same rows, side by side — never a row of the stack; a pane too short for
both pins the box to the bottom and clips the stack above it)
via
`BuildPanel::lifecycle_ready`, the command state pinned to the pane's last line (skipped
when the rows already fill the pane); starting Build/Rebuild shows the Monitor tab
(`MonitorSource::Build`) but keeps focus on the panel with the cursor on `Stop`, moving it to
`Flash` on a success and back to `Build` on a failure or after a `Clean` (which parks it on
`Build` while running — the step a clean clears the way for),
`Clean`
behind `Overlay::ConfirmBuild` (destructive capability). **Menuconfig** (`west build -t
menuconfig`) is interactive ncurses, so it parks a `pending_command` that `main.rs` runs under
`TerminalGuard::suspend` — the same hand-off as `$EDITOR`. The lifecycle always targets the
conventional `build` directory inside the project (implicit in commands; the `BuildDirPicker`
overlay and `set_build_dir` plumbing remain but no row offers them). Commands come from the
backend (`Backend::build_command`,
`src/backend/zephyr/commands.rs`: `west build`[-`b`][`--shield`]/`-t clean`/`--pristine=always`,
`west update`, `west sdk list`), run with the project root as cwd — the UI never names `west`
(workspace-scoped commands run in the workspace). The panel's `Board` checklist row (under
`Capability::BoardSelect`) opens `Overlay::BoardPicker`: a
filterable list over a background `west boards` fetch (`Backend::board_list_command`, parsed by
`build::parse_boards`); a pick is persisted in the project's `[[project]]` registry entry
(`settings::ProjectEntry`'s `board`/`shield`, written by `App::persist_board_shield`) and reloaded
on every open, outranking the build cache (`BoardOrigin::Config` vs `Picked` vs `Cache` — the row
says which); nothing is ever written into the project directory. The optional `Shield` row right
below (under `Capability::ShieldSelect`) opens `Overlay::ShieldPicker` over a background `west
shields` fetch — same `ListFetch` machinery as boards — with a leading `(none)` row to clear the
pick (the clearing persists too); the saved answer reaches only first-configuration builds as
`--shield` (never an incremental build of an already-configured directory). A project switch
re-derives both answers for the project switched to (`App::set_project_root`). `Flash`
(`west flash`, the board's own
runner from `runner.yml` — never a hard-coded programmer) sits last under
`Capability::Flash`, always behind `Overlay::ConfirmBuild` (destructive); the dashboard's `x`
routes a build-panel backend there instead of esptool's dialog, and the "Device info" pane shows
esptool's report for any backend whose board answers the background `chip-id` query (Zephyr
included; without an answer it falls back to its honest placeholder).
The Zephyr monitor is wired too: `m` runs `west monitor [--port P]` (`Backend::monitor_command`)
in the same PTY session MicroPython uses, and port discovery for a backend without `mpremote
devs` is `device::usb_serial_ports` — a synchronous `/dev` walk (no subprocess) feeding the
same `DeviceState`/picker flow (`App::scan_serial_devices`, `serial_dir` overridable for
deterministic tests; `home_dir` is the equivalent seam for workspace discovery). Connect/
disconnect feedback covers both device-shaped backends (`check_device_hotplug` gates on
`Filesystem || Monitor`, not `Filesystem` alone): the tick poll counts USB serial ports through
the same `serial_dir` walk and rescans on a change — `mpremote devs` for a filesystem backend,
the `/dev` walk for a monitor-only one — but never while an overlay is open (a picker must not
pop over a dialog mid-answer). Any empty or failed rescan routes through `device_disconnected`,
which drops the departed board's esptool identity *and* the firmware-identification state, so a
replug — even on the same port — refills the identity and re-runs the firmware identification
instead of sitting at a stale answer. With that,
Zephyr's Phase 3 surface (detect, board, build, clean, flash, monitor) plus its environment
layer (workspace/venv/SDK resolution, menuconfig, build dirs, `west update`, `west sdk list`)
is complete; debug/signing remain Roadmap items.

`lib.rs` + `main.rs`: everything except `terminal` and `ui` is testable without a tty, and `ui` is
testable through ratatui's `TestBackend` (see `tests/ui_render.rs`, `tests/files_view.rs`).

## Source of truth

- **`SPEC.md`** — product and architecture reference (goals, non-goals, backend model, UI/UX,
  MVP phasing, acceptance criteria).
- **`AGENTS.md`** — implementation rules and development workflow. It applies in full to Claude Code
  as well; read it before modifying anything.

Both are authoritative. Do not restate their content here — keep `SPEC.md` product-focused,
`AGENTS.md` process-focused, and this file limited to what neither covers.

## Commands

The verification set from `AGENTS.md`:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Single test / focused runs:

```bash
cargo test detect::              # tests whose path matches a substring
cargo test --lib                 # in-crate unit tests only
cargo test --test ui_render      # one integration target
```

Running the TUI: `cargo run` from inside a target embedded project directory (the app is
project-aware and searches upward from the current working directory). It exits with a clear error
when stdout is not a tty, so piping it is a safe smoke test.

To eyeball a rendered frame without a terminal, render into `TestBackend` and print
`terminal.backend().to_string()` — that is how the layout was verified.

Toolchain: stable Rust, edition 2024 (`rustc` 1.97 installed system-wide).

## Architecture: the load-bearing constraints

These are the decisions that shape most code, and getting them wrong causes wide refactors later:

1. **Capabilities, not conditionals.** The UI never branches on `is_micropython()` /
   `is_zephyr()`. Backends declare capabilities (build, upload, filesystem, repl, monitor, flash…)
   and views derive available actions from that set. This is what keeps future backends cheap.

2. **Delegate to external CLIs.** MicroPython goes through `mpremote`/`esptool`; Zephyr through
   `west`/`cmake`/`ninja`. Protocol reimplementation needs a demonstrated limitation first.

3. **Commands are structured, never shell strings.** Command construction is centralized (it is the
   main defense against upstream CLI drift) and executed without a shell. Prefer machine-readable
   tool output over parsing human-readable output.

4. **Nothing long-running touches the UI event loop.** Process execution must stream stdout/stderr,
   report exit status, and support cancellation and cleanup while navigation stays live.
   Async is not assumed — a synchronous event-driven design with processes off the UI thread is
   acceptable and preferred if it works.

5. **Interactive sessions (REPL, serial monitor) are a separate mechanism** from ordinary
   line-oriented subprocess capture, isolated from build/flash so a failure cannot corrupt terminal
   state. Terminal restoration must happen on *every* exit path, including panics and errors.

6. **Detection is weighted and explainable, with manual override.** Multiple signals produce a
   confidence score; ambiguity prompts the user rather than guessing. `pyproject.toml` alone must
   never identify a MicroPython project.

## How those constraints are realized today

- **`Backend` is one trait** (`src/backend/mod.rs`) covering both halves of a backend's identity:
  `detect(&DirScan) -> Vec<Signal>` (weighted evidence, never a boolean) and `capabilities()`.
  `SPEC.md` §6 lists `detect()` alongside `build()`/`flash()`; that ordering is circular, because
  detection must run before any backend instance is chosen. Operations will be added as a
  *separate* trait once there is a process manager to run them.
- **`DirScan`** (`src/project/scan.rs`) is an immutable directory snapshot — entry names plus the
  contents of an allowlist (`TEXT_FILES`). Detection never touches the filesystem itself, which is
  why every scoring rule is unit-testable in memory. A backend cannot trigger arbitrary I/O from
  inside a scoring function; if a new rule needs a file's contents, add it to `TEXT_FILES`.
- **Confidence is `score / saturation`, clamped to 1.0.** Each backend declares its own saturation
  so the number stays explainable ("3.0 of the 4.0 points needed"). Thresholds live in
  `src/project/detect.rs`: `MIN_CONFIDENCE` (below = unknown), `AUTO_CONFIDENCE` (above = selected
  silently), `AMBIGUITY_MARGIN` (candidates this close = ask the user). Changing a weight will move
  fixtures across those thresholds — the detection tests assert against them by name.
- **Override is a UI action; config files stay hand-rolled.** `ProjectManager::set_override`
  survives re-detection and keeps the automatic evidence for display. The config files that do
  exist (`chiptui.toml`'s `project_type` + `[zephyr]`, the user config's `[zephyr]`,
  `[projects]` and `[[project]]` blocks) are parsed by tolerant hand-rolled parsers
  (`src/project/config.rs`, `src/settings.rs`) — still no TOML dependency, per the same bias as
  the other one-shape parsers.
- **Nothing is written into a project directory except its own sources.** ChipTUI *reads* a
  project's `chiptui.toml` (and lets it outrank everything) but never creates one; the persisted
  "this directory is a Zephyr project" lives in the user config's `[[project]]` registry
  (`settings::ProjectRegistry`, fed into `ProjectManager::set_known_projects` and consulted by
  `detect_from_known` as `DetectionSource::Registered`, just under `Config`).
  `App::record_open_project` is the one place a project is recorded, and `main.rs` calls it for
  every route. The registry file is rewritten on every project open, so `settings::write_config`
  is atomic (tmp + rename) — it carries `[zephyr]` too. Tests that answer the empty-project
  prompt **must** `set_home_dir` first, or they write into the developer's real config.
- **The renderer publishes `App::log_viewport`** each frame so page-scrolling matches the drawn
  height (and the wrap width, into `LogStore::set_view_width`, so clamping matches too). Long log
  entries wrap with a hanging indent past the stamp (`logs::PREFIX_WIDTH`); scrolling, the clamp and
  the pane's scrollbar all count *visual* (post-wrap) lines, and the buffer is capped at 1_000
  entries. The Monitor tab scrolls the same way (`App::monitor_scroll`), across its four
  consoles — anchored to the *top* of the document so live output never shifts a scrolled view,
  gutter reserved via block padding, one `render_console`/`window_console` path doing the row
  windowing. Row 3 is one bordered pane whose top border carries the Log/Monitor tab strip
  (the Ratatui `Tabs` example pattern, `panels::draw_log_tabs`, drawn *after* the pane so it sits on
  the border; `symbols::DOT` divider, the active tab underlined and bold --- cyan when focused,
  default color otherwise --- vs the dim inactive one). At the
  strip's right edge rides the active tab's status (a leading space keeps the dashes off it): for
  Monitor, the source's title with a live icon
  and the output's row count — an animated spinner (`ui::SPINNER`, keyed off `App::ticks`) while a
  command runs, a green ✓ (red ✗ on failure) for the last finished one — plus `↑N` (rows below the
  view) once the user leaves the tail, mirroring Log's indicator; for Log, the entry count
  (plus `↑N` while scrolled). The panes themselves are untitled (`pane_border`). Rendering is
  otherwise a pure function of `App`.
- **Processes** (`src/process/`): `spawn` returns immediately; a supervisor thread plus two reader
  threads push `ProcessEvent`s into one channel that `main.rs` drains each frame. Two non-obvious
  rules live here. *Killing reaches only the direct child* — a grandchild keeps the pipes open, so
  a killed process reports `Finished` **without** waiting for the readers (otherwise the timeout
  deadlocks on the very hang it exists to escape). A *natural* exit instead waits (bounded,
  `READ_DRAIN_TIMEOUT`) on a reader counter before reporting, keeping the invariant that
  "Finished implies all output arrived" without joining threads. And `ProcessManager` is dropped
  with `cancel_all`, so no child keeps a serial port after the TUI exits.
- **`Browser` emits, never logs** (`src/browser.rs`): device results come back as `Notice` values
  and a `BrowserUpdate` that `App` forwards to the log and to `DeviceState`. That is what makes the
  whole state machine testable without a UI.
- **One `mpremote` at a time.** `mpremote` opens the serial port exclusively, so `Browser` keeps a
  queue and a single `in_flight` request. Listings are cached per `DevicePath` because each `ls`
  costs seconds over serial; `r` invalidates.
- **The device is chosen before it is used.** `open_files`/`App::maybe_scan_devices` (the latter
  run once at startup, from `main.rs`, so the Dashboard header does not sit on "not scanned" until
  the user opens the file browser) only scan; the first `ls` waits for the scan to name a port.
  Letting `mpremote` auto-connect first would talk to whichever board answers — the guess `SPEC.md`
  §8 forbids. `mpremote devs` lists *every* comport (32 legacy `/dev/ttyS*` on a typical Linux box),
  so `parse_devices` keeps only USB devices, matching mpremote's own auto-connect rule.
- **`=` vs `≈` is a real distinction.** `SameSize` means only that lengths match; `Identical`
  requires a sha256 check (`c`), device side via `mpremote fs sha256sum`, local side via `sha2`.
- **Monitor/run output is VT-interpreted** (`src/console.rs`): a REPL echo is not plain text —
  MicroPython's readline redraws the edited line with `\b`, `\x1b[K` and `\x1b[nD`, which a naive
  append-per-char renderer shows as literal `[K` garbage. `LineConsole` keeps the cursor position
  and escape-parser state across PTY chunks and edits the current line accordingly; sequences it
  does not implement (colors, OSC) are consumed, never rendered.
- **A running script is never interrupted silently** (`src/app/probe.rs`, `Browser::set_interrupt_gate`):
  mpremote Ctrl-C's whatever is executing to enter raw REPL for *any* device command, so before
  the first listing on a selected port ChipTUI opens a short `mpremote repl` PTY and classifies
  what it sees (`device::monitor_script_activity`: a `>>> ` prompt means idle, output with no
  prompt means running, banners filtered, silence inconclusive — a probe verdict is a *belief*,
  `ScriptState`, reset whenever the selected port changes). A "running" belief turns on the
  browser's interrupt gate: queued requests are held (`held_for_interrupt`) until the user
  confirms; accepting marks the script stopped, resumes the queue and arms the restore question
  for when it drains. Restore deliberately uses no-follow commands (`mpremote reset`, `exec
  --no-follow "import main"`) because a `soft-reset` leaves the script *stopped* — raw-REPL
  reboots skip `main.py`. The monitor updates the same belief live; a script that swallows
  Ctrl-C surfaces as the classified `ReplBlocked` error with its way out. The background
  `esptool chip-id` identity query (`FlashPanel::query_device_info`, chip not flash — the
  connection banner's identity half; flash geometry stays in the Flash view) runs *first* on a
  newly selected device: after the probe releases the port, the first device listing is held
  behind it (`App::hold_root_listing_for_chip_identity`/`held_root_listing`, released by
  `FlashUpdate::background_chip_query_finished` or, if the query can never start, by the tick's
  `DeferredQuery::Dropped`), so the port changes hands probe → esptool → mpremote instead of
  being contended. The query is also gated on the script belief
  (`maybe_run_deferred_flash_query`, tick-polled): esptool resets the board to read the chip,
  so it waits for an idle device, a closed overlay and a free port instead of racing a restore
  decision or silently resetting a script the user just declined to interrupt. The identity
  question is backend-agnostic where the board can answer it: a non-filesystem backend's
  selection (`scan_serial_devices`' auto-pick, `apply_device_picker`) defers the same query ---
  Zephyr runs on ESP32 boards whose esptool runner answers `chip-id`; on anything else it fails
  harmlessly and the pane keeps its honest placeholder. Such a backend's `FlashPanel` exists
  purely as the query engine (`ensure_flash_panel` skips the `firmware/` dir for a backend
  without `DeviceInfo`/`EraseFlash`), never lists (no browser is created), and 'x' still routes
  to the build panel's flash. A successful identity query chains into the firmware
  *identification* read (`arm_firmware_check`, once per selected port — `App::firmware_check_port`):
  `esptool read-flash 0x0 0x20000` (the bootloader region,
  the partition table and the start of the app area — the Zephyr/MCUboot banner lives in the
  bootloader, below 0x8000 — into a temp file) is part of the same selection chain, not an extra
  intrusion to ask about — esptool already reset the board once to read the chip, so the read adds
  no interruption the chain has not already made, and no overlay asks permission. The first
  listing is held behind it too (`hold_root_listing_for_firmware`, driven by
  `drive_held_root_listing` whenever a link of the chain reports back), because only MicroPython
  exposes a filesystem mpremote can walk: a verdict of Zephyr or ESP-IDF refuses the listing with
  `cannot read files — the device runs X, not MicroPython` in the device pane, and erased flash
  says to flash MicroPython first (`non_micropython_block_reason`); MicroPython releases the
  listing, and so does a board that could not be asked at all (a failed chip query — no
  esptool-backed bootloader — or a refused read: mpremote then fails on its own, which is what
  `Overlay::ConfirmEraseForMicroPython` exists for). A board the probe believes is *running a
  script* holds the listing behind the same chain instead of queueing it in the browser: a foreign
  firmware printing its boot banner (any auto-reset ESP32) is indistinguishable from a busy
  script at probe time, so `check_interrupt_gate` also fires for the held listing and the one
  question covers the whole chain — accepting marks the script stopped and lets chip-id →
  read-flash run to a verdict (which then releases *or refuses* the listing), declining drops
  the listing (`decline_held_listing`) with the same cancelled-script pane state the browser's
  held requests get; a foreign verdict cancels the restore question (`restore_pending`), since
  the "script" was the firmware's boot banner, and `maybe_offer_restore` waits for the whole
  chain to drain before opening, so the queries no longer guard on it (doing so deadlocked the
  chain: the restore question waits for the identification that waits on the belief the question
  itself would clear). The bytes are parsed by
  `src/firmware_id.rs` (Zephyr's `mcuboot`/`slot0_partition` partition labels decide first, then
  case-insensitive banner strings — MicroPython-on-Zephyr reads as Zephyr, the structural
  truth — then the `esp_app_desc_t` magic `0xABCD5432` scanned in the *app* region names a
  plain ESP-IDF app; bootloader bytes never classify anything, since the ESP-IDF bootloader
  is shared by all three firmwares), and the answer lands on its own row of the Device info
  pane, directly under the MAC, as `Firmware: MicroPython|Zephyr|ESP-IDF`
  (`DeviceDetails::firmware`) --- the verdict carries the version the *same read* found
  (`firmware_id::version`: the `MicroPython v1.28.0 on …` / `*** Booting Zephyr OS build
  v4.0.0 ***` banner strings, or for a plain IDF app the `esp_app_desc_t`'s stamped fields,
  where the IDF build's version outranks a project's), so the row reads e.g.
  `Firmware: Zephyr v4.0.0` / `Firmware: ESP-IDF v5.3.1`; a MicroPython verdict whose read
  found no version string falls back to the REPL-banner fact (`App::mpy_version`), and a
  firmware that names no version stays bare (labels identify without one; a guessed version
  is worse than none). One real layout needs the second read the hunt exists for: a Zephyr
  *simple boot* image is one contiguous XIP image whose application banner lives far past the
  identification window (on real hardware, an ESP32-C3: kernel strings at 0xa00, banner at
  0x6053c, no partition table, no `esp_app_desc`), so a versionless verdict arms
  `FlashPanel::query_firmware_version` (`version_hunt_pending`), a follow-up
  `read-flash 0x20000 0x80000` that only dates the standing verdict and never re-judges it
  (`firmware_id::HUNT_OFFSET`/`HUNT_SIZE`, `apply_version_from`) --- driven through the same
  tick-polled deferral as the other background queries (`App::maybe_run_deferred_version_hunt`),
  refused under an open overlay, dropped with the identity it belonged to, and inert by design
  for ESP-IDF (the descriptor the window already read is its only version source), `undefined`
  when the read failed or recognized nothing — with one distinction: a window that is entirely
  `0xFF` is erased flash, reported as `none (erased flash)` in warning color (`firmware_id::
  classify` → `FirmwareVerdict::Erased`), because "no firmware installed" is an answer, not
  an unknown; an empty/truncated read deliberately does not qualify as erased. The read
  itself waits for a free port like the chip query
  (`maybe_run_deferred_firmware_check`) — but not for a script believed running: by the time it
  is armed the chip query has already reset the board. Switching devices clears the old board's
  answer and re-arms the read; a successful erase/write-flash invalidates it
  (`FlashUpdate::firmware_invalidated`) so the next listing re-identifies, and `r` on the device
  pane (`reload_device_pane`) re-runs the identification whenever MicroPython is not confirmed —
  the recovery path after re-flashing. The features row is truncated to one line and the crystal
  rides
  the chip's own row, so the MAC and Firmware rows keep their fixed place in the pane's
  four rows.

## Testing

The normal suite must run without hardware. `tests/fixtures/bin/` holds fake executables — a
`mpremote` reproducing the 1.28 output formats, plus `slow` and `noisy` for timeout/cancel/stderr
paths, `mpremote-busy-board`/`mpremote-quiet-board` for a board stuck in a printing/silent
blocking loop (see `tests/busy_device.rs`), and `bursty` guarding output-before-`Finished`
ordering. Tests reference them by **absolute path** (`env!("CARGO_MANIFEST_DIR")`) and point the
browser at them with `Browser::set_tool_path`; nothing mutates `PATH`, so tests stay parallel-safe.
Add fakes for `esptool`, `west`, `cmake` and `ninja` the same way. Hardware tests stay separate and
explicitly documented.

If you change a fixture's canned sizes or digest, `tests/files_view.rs` asserts against them — the
`same.py` digest there is the real sha256 of the local fixture's contents, which is what makes the
`Identical` path meaningful.
