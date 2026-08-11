# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

Phase 1 of `SPEC.md` §17 is done (core, TUI, detection, backend registry, capabilities), plus the
process manager and the first real device operation: a dual-pane local/device **file browser** for
MicroPython (list + compare only — no upload, download, delete or mkdir yet). Zephyr backends still
declare detection and capabilities only. The repo is not under git.

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
  a killed process reports `Finished` **without** joining the readers (otherwise the timeout
  deadlocks on the very hang it exists to escape); consequently "Finished implies all output
  arrived" holds only for natural exits. And `ProcessManager` is dropped with `cancel_all`, so no
  child keeps a serial port after the TUI exits.
- **`Browser` emits, never logs** (`src/browser.rs`): device results come back as `Notice` values
  and a `BrowserUpdate` that `App` forwards to the log and to `DeviceState`. That is what makes the
  whole state machine testable without a UI.
- **One `mpremote` at a time.** `mpremote` opens the serial port exclusively, so `Browser` keeps a
  queue and a single `in_flight` request. Listings are cached per `DevicePath` because each `ls`
  costs seconds over serial; `r` invalidates.
- **The device is chosen before it is used.** `open_files` only scans; the first `ls` waits for the
  scan to name a port. Letting `mpremote` auto-connect first would talk to whichever board answers
  — the guess `SPEC.md` §8 forbids. `mpremote devs` lists *every* comport (32 legacy `/dev/ttyS*`
  on a typical Linux box), so `parse_devices` keeps only USB devices, matching mpremote's own
  auto-connect rule.
- **`=` vs `≈` is a real distinction.** `SameSize` means only that lengths match; `Identical`
  requires a sha256 check (`c`), device side via `mpremote fs sha256sum`, local side via `sha2`.

## Testing

The normal suite must run without hardware. `tests/fixtures/bin/` holds fake executables — a
`mpremote` reproducing the 1.28 output formats, plus `slow` and `noisy` for timeout/cancel/stderr
paths. Tests reference them by **absolute path** (`env!("CARGO_MANIFEST_DIR")`) and point the
browser at them with `Browser::set_tool_path`; nothing mutates `PATH`, so tests stay parallel-safe.
Add fakes for `esptool`, `west`, `cmake` and `ninja` the same way. Hardware tests stay separate and
explicitly documented.

If you change a fixture's canned sizes or digest, `tests/files_view.rs` asserts against them — the
`same.py` digest there is the real sha256 of the local fixture's contents, which is what makes the
`Identical` path meaningful.
