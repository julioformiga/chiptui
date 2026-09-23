# AGENTS.md

## Project

**ChipTUI** is a project-aware Rust TUI for MicroPython and Zephyr.
`SPEC.md` defines product behavior and architecture; this file defines
implementation and development rules for every coding agent.

## Read First

Before modifying the project:

1.  Read the relevant sections of `SPEC.md`.
2.  Inspect the existing source tree.
3.  Identify the relevant backend and capability model.
4.  Check the current implementation before introducing new
    abstractions.

If implementation reality differs from the specification, document the
discrepancy before making a large architectural change.

## Core Principles

### 1. Keep the project focused

This is a TUI, not an IDE.

Avoid source-code editing, unnecessary project-management features, plugin
systems and abstractions without a current MicroPython/Zephyr use case.

### 2. Use existing tools

Prefer invoking established tools:

``` text
MicroPython → mpremote, esptool
Zephyr      → west, CMake, Ninja
```

Do not reimplement their protocols unless a demonstrated limitation requires
it. Construct commands as program and arguments, not shell strings; prefer
machine-readable output when available. Keep board-specific behavior in its
backend; Zephyr flash/debug mechanisms vary by board.

### 3. Backend capabilities

Do not scatter framework checks throughout the UI.

Let the UI ask the backend for capabilities and render supported actions;
do not branch on backend names when a capability answers the question.

### 4. Project detection

Use multiple weighted signals, explain the result and allow overrides.
`pyproject.toml` alone does not identify MicroPython; an ordinary
`CMakeLists.txt` alone does not identify a Zephyr application. See
`SPEC.md` §7 for the signals and startup routing.

### 5. External processes

Long-running commands must never block the TUI event loop.

All external process execution should support, where applicable:

-   stdout/stderr streaming;
-   exit status;
-   cancellation;
-   error reporting;
-   cleanup.

Avoid a shell when direct execution suffices. Cancellation must reach
delegating tools' children as well as the immediate process. For natural
exit, drain output before reporting completion; a cancelled process must
not hang waiting for a descendant that kept its pipes open.

### 6. Interactive serial sessions

REPL and serial monitor sessions are special.

Do not treat them as ordinary line-oriented subprocess output.

Preserve input, terminal behavior and output streaming, and restore terminal
state on every exit path, including errors and panics. A PTY session must
receive control keys as bytes; do not route it through line-oriented output.

## Rust Guidelines

Use stable Rust unless the project explicitly requires otherwise.

Prefer:

-   clear ownership;
-   small modules;
-   explicit types;
-   `Result`-based error handling;
-   meaningful error messages;
-   minimal cloning;
-   safe Rust.

Avoid:

-   unnecessary `unsafe`;
-   premature async;
-   excessive trait abstractions;
-   global mutable state.

Keep modules cohesive.

## TUI Guidelines

Use Ratatui/Crossterm.

The UI should be:

-   keyboard-first;
-   responsive;
-   contextual;
-   compact;
-   readable in normal terminal sizes.

Use the `ratatui-themes` crate for a consistent, swappable color theme
(default: Tokyo Night); the operator can override it via `[ui] theme` in the
user config.

Long-running operations should show progress/status without freezing
navigation.

Require the confirmation grammar in `SPEC.md` §15 for destructive actions.

No Private Use Area codepoints (Nerd Font icons and the like) in any glyph
the UI draws --- they render as tofu or blank space on a terminal without
that font installed, and there is no fallback. Stick to standard Unicode
(plain symbols or emoji); `tests/no_private_use_glyphs.rs` scans `src/` for
violations. The single sanctioned exception is `src/icons.rs`, the shared
glyph vocabulary, which may carry single-width BMP Private Use Area
codepoints (`nf-fa-*` and `nf-custom-*`) written as `\u{...}` escapes
so the scan still holds without an exception list --- and those glyphs ship
only behind the opt-in `[ui] icons = "nerd"` in the user config; the
default rendering stays plain Unicode (`"unicode"`), with `"none"` drawing
no glyphs at all, so a terminal without a Nerd Font never meets one.

Mouse support is opt-in (`[ui] mouse = true`, default off so the
terminal's own selection and scrollback keep working) and stays an
alternative trigger for actions the keyboard already owns: a gesture lands
through the same handlers `Enter`/arrows reach, never beside their gates.
A click that acts on a row (copying a log line or the MAC row, pressing a
stacked Actions button, opening an Environment row's dialog) additionally
requires its pane to already hold focus --- the position `Enter` is always
in --- so an unfocused click is spent on focus alone. Left clicks and
wheel steps only --- no motion, drag or hover. Hit-testing
recomputes the drawn geometry (`ui::layout`, the published frame area,
`ui::home::hit_areas`) rather than caching rects; a gesture that arrives
while reporting is off, under a modal, or before a frame is dropped.

## Testing

Every new feature should include tests where practical.

Prioritize tests for:

-   project detection;
-   capability mapping;
-   command construction;
-   process lifecycle;
-   output parsing;
-   state transitions;
-   error handling.

Do not require physical hardware for normal tests.

Use fake executables or fixtures for external tools (`mpremote`, `esptool`,
`west`, `cmake`, `ninja`, `smpmgr`). A fake must reproduce the tool rather
than assumptions about it: verify load-bearing CLI flags against the tool,
and make the fake reject incorrect invocations. Use absolute fixture paths
instead of changing global `PATH`, so tests remain parallel-safe.

Hardware tests should be separate and explicitly documented.

## Dependencies

Before adding a dependency:

1.  Confirm that the standard library or an existing dependency cannot
    reasonably solve the problem.
2.  Check whether the dependency is maintained.
3.  Consider compile time and binary size.
4.  Keep the dependency narrowly justified.

Do not add an async runtime simply because it is common in Rust.

## Changes

When implementing a feature:

1.  Understand the relevant part of `SPEC.md`.
2.  Make the smallest coherent change.
3.  Add/update tests.
4.  Run formatting and checks.
5.  Verify that unrelated backends remain unaffected.

Avoid large refactors while implementing unrelated features.

## Verification

Before considering a change complete, run the applicable checks, for
example:

``` bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

If a command is not applicable or cannot run in the current environment,
state why.

For UI changes, also verify:

-   terminal startup;
-   terminal resize;
-   keyboard navigation;
-   clean exit;
-   error paths;
-   terminal restoration.

## Iteration Discipline

Any repeated task --- running checks, searching, probing, retrying ---
starts with a declared stopping rule: what result ends the loop, and
how many rounds it may take at most. The default for verification is
one full pass over the affected checks once the change is in its final
state.

A round that answers the question ends the loop. Starting another needs
a reason the last one cannot provide:

-   the thing under test changed since the last round;
-   the last round failed, and the failure is being diagnosed.

Repeating a round that already succeeded "to be sure" spends the
operator's machine time answering a question that was answered. A dead
end that turns out to be environmental (external state, resource
exhaustion, tooling crashes outside the change) is documented once with
its observed cause and left to the operator --- not retried, and never
worked around by guessing at the cause.

Diagnostics added while investigating (temporary logging, scratch
tests, instrumented re-runs) are removed before reporting; their
findings, not their presence, are the deliverable.

## Configuration

Keep user configuration separate from project configuration.

Do not duplicate settings already owned by Zephyr, west, CMake or
MicroPython unless the TUI needs a user-facing override.

Project-specific overrides should be explicit.

## Error Messages

Errors should tell the user:

1.  what failed;
2.  which operation was being performed;
3.  the relevant command/tool;
4.  what the user can do next.

Avoid exposing only raw subprocess errors when a useful explanation can
be provided.

Keep detailed command output available in the log view.

## Documentation

Update documentation when behavior changes.

Keep `SPEC.md` focused on product and architecture.

Keep `AGENTS.md` focused on implementation rules and development
workflow.

Do not duplicate large sections between the two files.
