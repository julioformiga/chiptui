# AGENTS.md

## Project

This repository contains **ChipTUI**, a Rust terminal UI for
embedded development.

The application is project-aware and initially supports:

-   MicroPython
-   Zephyr

It orchestrates existing tools instead of reimplementing their
protocols.

## Read First

Before modifying the project:

1.  Read `SPEC.md`.
2.  Inspect the existing source tree.
3.  Identify the relevant backend and capability model.
4.  Check the current implementation before introducing new
    abstractions.

`SPEC.md` is the product/architecture reference. If implementation
reality differs from the specification, document the discrepancy before
making a large architectural change.

## Core Principles

### 1. Keep the project focused

This is a TUI, not an IDE.

Do not add:

-   source-code editing;
-   unnecessary project-management features;
-   a plugin marketplace;
-   unrelated embedded frameworks;
-   complex abstractions without a concrete use case.

### 2. Use existing tools

Prefer invoking established tools:

``` text
MicroPython → mpremote, esptool
Zephyr      → west, CMake, Ninja
```

Do not reimplement their protocols unless there is a demonstrated
limitation that requires it.

### 3. Backend capabilities

Do not scatter framework checks throughout the UI.

Avoid patterns such as:

``` rust
if project.is_micropython() { ... }
else if project.is_zephyr() { ... }
```

when the decision can be represented through backend capabilities.

The UI should ask the backend what operations are supported and render
the appropriate actions.

### 4. Project detection

Detection must use multiple signals.

Do not identify MicroPython solely from:

``` text
pyproject.toml
```

A normal Python project can contain that file.

Zephyr detection should consider strong indicators such as:

``` text
.west/
west.yml
prj.conf
app.overlay
CMakeLists.txt
```

Detection should be explainable and overridable.

### 5. External processes

Long-running commands must never block the TUI event loop.

All external process execution should support, where applicable:

-   stdout/stderr streaming;
-   exit status;
-   cancellation;
-   error reporting;
-   cleanup.

Avoid shell invocation when direct process execution is sufficient.

Do not construct commands by concatenating untrusted strings into shell
commands.

### 6. Interactive serial sessions

REPL and serial monitor sessions are special.

Do not treat them as ordinary line-oriented subprocess output.

Preserve:

-   interactive input;
-   terminal behavior;
-   output streaming;
-   clean exit;
-   terminal restoration.

Always ensure the terminal is restored if a monitor or REPL session
fails.

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

Destructive actions such as:

-   flash;
-   erase;
-   recursive remote delete;

must require appropriate confirmation.

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

Use fake executables or fixtures to simulate:

-   `mpremote`;
-   `esptool`;
-   `west`;
-   `cmake`;
-   `ninja`.

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

## Backend Rules

### MicroPython

Use:

``` text
mpremote
esptool
```

for the relevant operations.

Do not duplicate MicroPython filesystem or REPL protocols in the MVP.

### Zephyr

Use:

``` text
west
cmake
ninja
```

as appropriate.

Do not assume all Zephyr boards use the same flash/debug mechanism.

Board-specific behavior belongs in the Zephyr backend rather than the
generic UI.

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

## Security / Safety

Treat external command execution and device operations as potentially
destructive.

Never silently run:

``` text
erase flash
recursive remote delete
```

without the appropriate confirmation.

Do not hide destructive consequences from the user.

## Documentation

Update documentation when behavior changes.

Keep `SPEC.md` focused on product and architecture.

Keep `AGENTS.md` focused on implementation rules and development
workflow.

Do not duplicate large sections between the two files.

## Future Backends

Possible future backends include:

-   ESP-IDF;
-   Arduino CLI;
-   PlatformIO;
-   CircuitPython.

Do not implement infrastructure solely for these future backends unless
it also solves a current MicroPython/Zephyr problem.

The architecture should permit future backends, but the current
implementation should remain simple.

## Definition of Done

A feature is complete when:

-   it follows the specification;
-   the relevant backend remains isolated;
-   the UI remains responsive;
-   errors are handled;
-   tests are added or updated;
-   formatting/checks pass;
-   no unnecessary dependencies were introduced;
-   terminal state is restored on exit paths;
-   documentation is updated when necessary.
