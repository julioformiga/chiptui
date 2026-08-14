# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

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
(`Browser::request_mkdir`/`request_touch`). Editing a device file downloads it to a scratch temp file
(`browser::edit_download_path`), never the project tree — the point is to prove a change on the
device first; `Download` is the separate, explicit step for landing a confirmed-good result in the
project. On a clean `$EDITOR` exit it re-uploads to the same device path and then offers a
`soft-reset`, defaulting to *no* (`Overlay::ConfirmRestartDevice`). A board believed to be
running a script is never interrupted silently: a short `mpremote repl` probe runs before the
first listing (`src/app/probe.rs`), device operations are held behind a confirmation while a
script runs (`Overlay::ConfirmInterruptDevice`), and an accepted interruption ends with a
restore prompt (`Overlay::RestoreDeviceScript`: hard reset, `import main`, or leave stopped).

The local pane is backend-agnostic now: every backend gets a browser (`maybe_scan_devices` creates
one; only the device *scan* waits for `Capability::Filesystem`), and the local menu is
capability-gated (`FileAction::for_entry`: no `SendToDevice` without `Upload`, no `Diff` without
`Filesystem`), so Zephyr's local pane offers exactly open/view/edit/delete. A backend that can
build but has no device filesystem (Zephyr) fills row 2's right half with a **build panel**
(`src/build.rs`, `src/ui/build.rs`, `src/app/build_view.rs`): Build/Clean/Rebuild as a navigable
list quoting the literal commands, board from `build/zephyr/CMakeCache.txt` (`cached_board`),
`Stop` while a command runs, `Clean` behind `Overlay::ConfirmBuild` (destructive capability),
output streaming into the Monitor tab (`MonitorSource::Build`). Commands come from the backend
(`Backend::build_command`, `src/backend/zephyr/commands.rs`: `west build`[-`b`]/
`-t clean`/`--pristine=always`), run with the project root as cwd — the UI never names `west`.
The panel's `Board` action (under `Capability::BoardSelect`) opens `Overlay::BoardPicker`: a
filterable list over a background `west boards` fetch (`Backend::board_list_command`, parsed by
`build::parse_boards`); a pick is session-only (`BoardOrigin::Picked` vs `Cache` — the header
says which), never written to the project.
Not done yet: Zephyr flash (the `x` dialog still shows esptool's for any `Flash` backend), and
the Zephyr serial monitor.

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
- **Override is a UI action, not a config file.** `ProjectManager::set_override` survives
  re-detection and keeps the automatic evidence for display. The project-local config file from
  `SPEC.md` §7 is not implemented; there is no TOML dependency yet.
- **The renderer publishes `App::log_viewport`** each frame so page-scrolling matches the drawn
  height. Rendering is otherwise a pure function of `App`.
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
  `esptool` chip query is also gated on that belief (`maybe_run_deferred_flash_query`,
  tick-polled): esptool resets the board to read the chip, so it waits for an idle device,
  a closed overlay and a free port instead of racing a restore decision or silently
  resetting a script the user just declined to interrupt.

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
