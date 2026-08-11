# ChipTUI --- Specification

## 1. Overview

**ChipTUI** is a terminal user interface written in Rust for
embedded development workflows.

The application is project-aware: it opens in a project directory,
detects the project type, selects an appropriate backend, and exposes
the operations that make sense for that project.

Initial backends:

-   **MicroPython** --- `mpremote` + `esptool`
-   **Zephyr** --- `west` + the Zephyr build/flash/monitor toolchain

The application is not intended to replace an editor or IDE. It is an
orchestration and visualization layer over existing embedded-development
tools.

The design should make future backends possible without making the MVP
unnecessarily complex.

## 2. Goals

### Primary goals

-   Provide a fast keyboard-driven embedded-development workflow.
-   Detect the current project automatically.
-   Detect and manage connected devices.
-   Present a consistent UI across different embedded ecosystems.
-   Use established tools instead of reimplementing their protocols.
-   Make common build, run, flash, monitor and device operations
    discoverable.
-   Provide useful progress, status and error information.
-   Keep the application small, reliable and scriptable.

### Secondary goals

-   Support multiple connected devices.
-   Allow manual project-type overrides.
-   Support external editors such as Neovim.
-   Make backend capabilities explicit.
-   Provide a clean foundation for future backends.

## 3. Non-goals

The MVP will not:

-   become a full IDE;
-   include a source-code editor;
-   replace `mpremote`, `esptool`, `west`, CMake or Ninja;
-   implement its own firmware flashing protocols;
-   implement a debugger;
-   manage arbitrary embedded toolchains;
-   provide a plugin marketplace;
-   automatically modify project files without explicit user action.

Debugging, additional frameworks and advanced automation may be
considered later.

## 4. Design Principles

1.  **Project first** --- the project determines the available workflow.
2.  **Backend abstraction** --- the UI should not contain
    framework-specific command logic.
3.  **Capabilities over conditionals** --- views and actions should be
    derived from backend capabilities.
4.  **Official tools first** --- delegate to established CLI tools
    whenever practical.
5.  **Explicit destructive actions** --- erase and flash operations
    require clear confirmation.
6.  **Fast feedback** --- build, flash and command progress should be
    visible immediately.
7.  **Terminal-native UX** --- keyboard navigation is primary; mouse
    support is optional.
8.  **No unnecessary abstraction** --- design for MicroPython and Zephyr
    first, future backends second.

## 5. Architecture

``` text
                    ┌─────────────────────┐
                    │         TUI         │
                    │  Ratatui/Crossterm  │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │   Application Core  │
                    │ state/events/actions │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │   Project Manager   │
                    │ detection + config  │
                    └──────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
       ┌──────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐
       │ MicroPython │  │   Zephyr    │  │   Future    │
       │   Backend   │  │   Backend   │  │   Backend   │
       └──────┬──────┘  └──────┬──────┘  └─────────────┘
              │                │
        mpremote/esptool   west/cmake/ninja
              │                │
              └───────┬────────┘
                      │
                 USB / Serial
                      │
                 Embedded board
```

### Core components

-   **App** --- application lifecycle and global state.
-   **UI** --- Ratatui views, layouts, dialogs and key handling.
-   **Project Manager** --- project root discovery and backend
    detection.
-   **Backend Registry** --- available backend implementations.
-   **Device Manager** --- serial/USB device discovery and selection.
-   **Process Manager** --- execution, cancellation, output capture and
    exit status.
-   **Backend implementations** --- framework-specific operations.
-   **Configuration** --- user and project configuration.
-   **Event system** --- converts process/device/UI events into
    application state changes.

## 6. Backend Model

A backend should conceptually expose operations such as:

``` text
detect(project)
capabilities()
discover_devices()
device_info(device)
build()
clean()
flash(device)
run(device)
monitor(device)
logs()
```

Backends do not need to implement every operation.

A capability model should determine which actions appear in the UI.

Example:

``` text
MicroPython
  build       no
  upload      yes
  download    yes
  filesystem  yes
  repl        yes
  monitor     yes
  flash       yes

Zephyr
  build       yes
  upload      no
  filesystem  no
  repl        no
  monitor     yes
  flash       yes
```

The exact Rust trait/API should be decided during implementation based
on the needs of both backends.

## 7. Project Detection

Project detection happens when the application starts and may also be
triggered manually.

Detection should search from the current directory upward until a
suitable project root is found.

### MicroPython indicators

Potential indicators include:

-   `pyproject.toml` containing MicroPython-specific dependencies or
    configuration;
-   `main.py`;
-   `boot.py`;
-   MicroPython-specific configuration;
-   `mpremote` configuration;
-   project metadata identifying MicroPython.

`pyproject.toml` alone must not identify a project as MicroPython
because it is also used by normal Python projects.

### Zephyr indicators

Strong indicators include:

-   `.west/`;
-   `west.yml`;
-   `CMakeLists.txt`;
-   `prj.conf`;
-   `app.overlay`;
-   board-related directories/configuration;
-   other Zephyr-specific metadata.

Zephyr detection should distinguish a normal CMake project from a Zephyr
application where possible.

### Detection strategy

Use weighted signals rather than a single filename.

Example conceptual result:

``` text
MicroPython  confidence: 0.92
Zephyr       confidence: 0.03
```

If confidence is ambiguous, the UI should ask the user to select the
backend.

### Manual override

The user must be able to override detection.

A project-local configuration file should be supported, for example:

``` toml
project_type = "micropython"
```

or:

``` toml
project_type = "zephyr"
```

The exact filename should be chosen during implementation.

## 8. Device Management

The Device Manager should:

-   enumerate serial devices;
-   identify USB VID/PID when available;
-   track device connection/disconnection;
-   allow explicit device selection;
-   support multiple connected devices;
-   avoid assuming `/dev/ttyACM0`;
-   expose backend-specific device information when available.

On Linux, common serial devices include `/dev/ttyACM*` and
`/dev/ttyUSB*`.

The architecture should remain portable to macOS and Windows.

### MicroPython

`mpremote` already supports automatic USB serial discovery and explicit
port selection. It can identify devices by path or USB serial identity.
The TUI should prefer explicit device selection once multiple devices
are present.

### Zephyr

Device selection may depend on the board and flash/debug mechanism. The
backend should not assume every Zephyr board is flashed through the same
serial port.

## 9. MicroPython Backend

### Tools

Primary tools:

-   `mpremote`
-   `esptool`

### Operations

The backend should expose:

-   device selection;
-   device information;
-   filesystem listing;
-   upload;
-   download;
-   delete;
-   mkdir;
-   run script;
-   `exec`;
-   `eval`;
-   REPL/serial monitor;
-   soft reset;
-   reset;
-   package installation via `mip`;
-   mount/unmount;
-   filesystem statistics;
-   firmware/flash operations through `esptool`.

`mpremote` should remain the primary abstraction for MicroPython
interaction.

The TUI should avoid reimplementing the MicroPython serial protocol in
the MVP.

### Filesystem

The UI should present a remote filesystem explorer:

``` text
/
├── boot.py
├── main.py
├── config.py
└── lib/
```

Actions:

-   navigate;
-   upload;
-   download;
-   delete;
-   create directory;
-   execute;
-   refresh.

Destructive remote filesystem operations require confirmation where
appropriate.

### REPL / Monitor

The REPL view must support interactive input without buffering the
session through a normal line-oriented command abstraction.

The implementation should treat the session as an interactive
terminal/serial stream and preserve terminal input/output semantics.

### Flashing

`esptool` operations should be presented separately from normal
MicroPython filesystem operations.

Potential actions:

-   chip information;
-   flash information;
-   erase flash;
-   write/flash firmware;
-   verify;
-   reset.

`erase_flash` must require explicit confirmation.

## 10. Zephyr Backend

### Tools

Primary tools:

-   `west`
-   CMake
-   Ninja where used by the generated build system.

`west` is the primary Zephyr orchestration interface.

### Operations

The initial backend should support:

-   board selection;
-   project information;
-   build;
-   clean;
-   flash;
-   serial monitor;
-   build output/logs.

Potential future operations:

-   `west update`;
-   debug;
-   signing;
-   device-tree inspection;
-   configuration helpers.

### Board selection

The backend should discover or expose the configured Zephyr board.

If the board cannot be determined unambiguously, the user should be able
to select it.

The board selection should not silently modify project configuration.

### Build

The UI should provide:

``` text
Build
Clean
Rebuild
```

Build output should stream into a log pane and show:

-   running state;
-   elapsed time;
-   success/failure;
-   exit code.

### Flash

The backend should delegate flashing to Zephyr's supported mechanisms
rather than assuming a single programmer.

The TUI should expose a simple `Flash` action while preserving
backend-specific configuration.

### Monitor

Provide a serial monitor where appropriate.

The monitor should be independent from the build process so that a
build/flash failure does not corrupt the terminal state.

## 11. UI / UX

The application should use a contextual dashboard.

### Common layout

``` text
┌─────────────────────────────────────────────────────────────┐
│ ChipTUI │ Project │ Backend │ Device                   │
├─────────────────────────────────────────────────────────────┤
│ PROJECT / DEVICE │ MAIN VIEW              │ LOG / OUTPUT    │
│                   │                        │                 │
│                   │                        │                 │
├───────────────────┴────────────────────────┴────────────────┤
│ Contextual keyboard shortcuts                                │
└─────────────────────────────────────────────────────────────┘
```

The exact layout is not fixed.

### Required UX

-   keyboard-first navigation;
-   clear focus indicator;
-   command/status bar;
-   contextual shortcuts;
-   modal confirmation for dangerous operations;
-   scrollable logs;
-   progress indicators;
-   non-blocking long-running operations;
-   clear error messages;
-   terminal resize support;
-   graceful shutdown.

A command palette may be added if it improves discoverability.

The UI should use terminal colors by default rather than imposing a
heavy theme.

## 12. Process Management

External tools are long-running and must not block the UI event loop.

The Process Manager should provide:

-   command construction;
-   environment handling;
-   stdout/stderr capture;
-   streaming output;
-   exit status;
-   cancellation;
-   timeout where appropriate;
-   process cleanup;
-   command history for diagnostics.

Commands should be represented structurally rather than assembled as
unsafe shell strings.

Avoid invoking a shell unless explicitly required.

## 13. Configuration

Support two levels:

### User configuration

Potentially:

``` toml
[tools]
mpremote = "mpremote"
esptool = "esptool"
west = "west"
cmake = "cmake"

[ui]
log_panel = true
mouse = false
```

### Project configuration

Used primarily for:

-   backend override;
-   default device;
-   board;
-   project-specific tool options.

Do not duplicate configuration already managed by the underlying
framework.

The application should detect missing tools and present actionable
installation/configuration errors.

## 14. Error Handling

Errors should be categorized:

-   project detection;
-   missing executable;
-   invalid configuration;
-   device unavailable;
-   permission denied;
-   process failure;
-   build failure;
-   flash failure;
-   serial failure;
-   timeout;
-   cancellation.

Every external command should retain enough information to diagnose
failure.

The UI should show a concise error first, with access to detailed
output.

## 15. Safety

Potentially destructive operations:

-   erase flash;
-   flash firmware;
-   remote file deletion;
-   recursive remote deletion;
-   clean operations that remove build artifacts.

These should have appropriate confirmation.

Never automatically erase flash as part of a normal flash operation
unless explicitly configured and confirmed.

The application must not hide the fact that a command is destructive.

## 16. Testing

### Unit tests

Test:

-   project detection;
-   confidence scoring;
-   configuration;
-   capability mapping;
-   command construction;
-   output parsing;
-   state transitions.

### Integration tests

Use fake executables for:

-   `mpremote`;
-   `esptool`;
-   `west`;
-   `cmake`;
-   `ninja`.

The fakes should simulate:

-   success;
-   failure;
-   slow operations;
-   malformed output;
-   cancellation.

### Hardware tests

Hardware tests should be optional and separated from the normal test
suite.

At minimum, document manual test matrices for:

-   MicroPython USB device;
-   ESP32 flash;
-   Zephyr build;
-   Zephyr flash;
-   serial monitor.

## 17. MVP

### Phase 1 --- Core

-   Rust application;
-   Ratatui UI;
-   project root detection;
-   backend registry;
-   process manager;
-   configuration;
-   basic logs.

### Phase 2 --- MicroPython

-   detection;
-   device selection;
-   `mpremote`;
-   filesystem;
-   run;
-   REPL;
-   reset;
-   `esptool` information and flash.

### Phase 3 --- Zephyr

-   detection;
-   board selection;
-   build;
-   clean;
-   flash;
-   monitor.

At the end of the MVP the application must provide a useful end-to-end
workflow for both ecosystems.

## 18. Roadmap

Possible later phases:

### Advanced MicroPython

-   richer synchronization;
-   project dependency management;
-   editor integration;
-   multiple devices;
-   improved firmware management.

### Advanced Zephyr

-   `west update`;
-   debug integration;
-   signing;
-   device-tree/configuration helpers;
-   multiple boards/profiles.

### Additional backends

Potential future backends:

-   ESP-IDF;
-   Arduino CLI;
-   PlatformIO;
-   CircuitPython.

These should only be implemented when a real use case exists.

## 19. Acceptance Criteria

### Project detection

-   Starting in a MicroPython project selects MicroPython automatically
    when evidence is sufficient.
-   Starting in a Zephyr project selects Zephyr automatically when
    evidence is sufficient.
-   Ambiguous projects can be selected manually.
-   Detection never relies on `pyproject.toml` alone.

### MicroPython

-   User can select a connected device.
-   User can browse the remote filesystem.
-   User can upload/download files.
-   User can run a script.
-   User can enter/leave REPL.
-   User can reset the device.
-   User can obtain ESP information.
-   User can perform a confirmed flash operation.

### Zephyr

-   User can identify the Zephyr project.
-   User can select/configure a board.
-   User can build.
-   User can clean.
-   User can flash.
-   User can monitor serial output when supported.

### General

-   Long operations do not freeze the TUI.
-   Errors remain visible and diagnosable.
-   Destructive operations require confirmation.
-   The application exits cleanly without leaving terminal settings
    corrupted.

## 20. Project Structure

Suggested initial structure:

``` text
chiptui/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── app.rs
│   ├── event.rs
│   ├── config.rs
│   ├── project/
│   │   ├── mod.rs
│   │   ├── detector.rs
│   │   └── types.rs
│   ├── backend/
│   │   ├── mod.rs
│   │   ├── micropython/
│   │   └── zephyr/
│   ├── device/
│   │   ├── mod.rs
│   │   └── discovery.rs
│   ├── process/
│   │   └── mod.rs
│   └── ui/
│       ├── mod.rs
│       ├── layout.rs
│       ├── components/
│       └── views/
├── tests/
└── docs/
```

This structure is a starting point, not a requirement. Avoid creating
modules before they are needed.

## 21. Recommended Technology

Rust is the preferred implementation language.

Recommended initial stack:

-   Rust stable;
-   Ratatui;
-   Crossterm;
-   a maintained Rust serial communication crate if direct serial access
    is required;
-   standard process APIs for external commands;
-   standard filesystem APIs;
-   a lightweight serialization/configuration format such as TOML.

Async should only be introduced where it materially improves the
architecture.

A simple event-driven synchronous architecture may be preferable for the
first implementation if it can keep long-running processes off the UI
thread.

## 22. Important Technical Decisions

### Delegate rather than reimplement

The first implementation should invoke:

``` text
MicroPython → mpremote / esptool
Zephyr      → west / CMake / Ninja
```

rather than duplicating their protocols.

This reduces maintenance and keeps behavior aligned with the official
ecosystems.

### Backend capabilities

The UI must consume capabilities rather than hard-code
framework-specific menus.

### Project detection

Detection should be heuristic but explainable, with manual override.

### Hardware independence

The normal test suite must work without physical hardware.

## 23. Risks

### External CLI changes

Underlying tools can change their output or arguments.

Mitigation:

-   centralize command construction;
-   minimize parsing of human-readable output;
-   prefer machine-readable output where available;
-   test supported versions.

### Device ambiguity

Multiple boards may be connected.

Mitigation:

-   explicit device selection;
-   stable identifiers where available;
-   show port and device metadata.

### Terminal state corruption

Interactive serial sessions can interfere with the TUI.

Mitigation:

-   isolate monitor sessions;
-   carefully manage raw terminal mode;
-   restore terminal state on every exit path.

### Overengineering

Supporting many embedded ecosystems too early could make the core
architecture complex.

Mitigation:

-   MVP limited to MicroPython and Zephyr;
-   capability-based backend abstraction;
-   no plugin system until required.

## 24. References

The implementation should consult the current official documentation
during development:

-   MicroPython documentation:
    https://micropython.org/resources/docs/en/latest/
-   `mpremote`:
    https://micropython.org/resources/docs/en/latest/reference/mpremote.html
-   MicroPython packages:
    https://micropython.org/resources/docs/en/latest/reference/packages.html
-   Zephyr documentation: https://docs.zephyrproject.org/latest/
-   West: https://docs.zephyrproject.org/latest/develop/west/index.html
-   Ratatui: https://ratatui.rs/

The documentation for the exact versions used by the implementation
takes precedence over this document.

## 25. Final Product Principle

The application should feel like:

> **"Open the embedded project and immediately see what I can do with
> it."**

The user should not need to remember whether an operation requires
`mpremote`, `esptool`, `west`, CMake or another command.

The TUI discovers the project, exposes its capabilities, delegates the
work to the appropriate tools, and presents the result clearly.

The project should remain a **focused embedded-development TUI**, not
become a general-purpose IDE.
