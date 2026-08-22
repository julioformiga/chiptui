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

### Where a session starts

ChipTUI is project-aware, so the working directory decides the opening
screen. In order:

1.  a directory (or an ancestor) whose backend is known --- named by the
    project registry (§13), by a project-local `chiptui.toml`, or by the
    evidence itself --- opens the dashboard directly;
2.  an *ambiguous* directory opens the dashboard too, so the prompt that
    resolves it appears where the user already is;
3.  an empty directory opens the dashboard so it can be scaffolded (below);
4.  anything else --- a directory with contents and no project in it or
    above it, `$HOME` being the usual case --- opens the **home screen**.

The home screen is the project list: create a new project, or search and
open a recorded one. It is backed entirely by the registry, shows each
project's backend, name and path, filters live as the user types, and can
forget an entry (the directory itself is never touched). It is also reachable
from the dashboard, so projects can be switched without restarting; anything
still running is named in a confirmation first, since leaving cancels it.

Creating a project asks for the folder it goes into, then the project's
name; the new directory is empty, so the flow continues into the
empty-project prompt below.

### Empty or unrecognized projects

When detection concludes `Unknown` or `Ambiguous`, no project-local
configuration file (below) is present and the registry does not name the
directory, the UI should ask which project type this directory is, offering
the currently supported backends (today: MicroPython, Zephyr). This is not
the same answer as the project-local `chiptui.toml` override below: it
fires automatically once, right after detection, instead of waiting for a
file the user has to write. (Re-running detection --- the Log pane's `r` ---
offers it again.)

Once the user answers, two things happen, neither needing its own
confirmation --- both are part of answering the prompt (§3: explicit, never
inferred):

-   the answer is recorded in the **user** configuration's project registry
    (§13), so the directory is recognized automatically on every later run
    and the home screen lists it;
-   the backend's starting layout is written into the directory, so the
    project is usable immediately. Nothing already there is overwritten.

MicroPython starts with `src/` for the sources kept in sync with the device
(§9's filesystem browser opens on this directory), `firmware/` for firmware
files (§9's discovery and download saves into it), and the two entry points
the board runs by name (`boot.py`, `main.py`). Zephyr starts with the three
files `west build` requires: a `CMakeLists.txt` calling `find_package(Zephyr)`,
an empty `prj.conf`, and `src/main.c`.

This is how a brand-new, otherwise-empty project directory gets a working
backend: the user is not required to create marker files like `boot.py` or
`west.yml` by hand before the TUI becomes useful.

The application does **not** write a configuration file into the project
directory. What ChipTUI knows about a project lives in the user
configuration; a project's own `chiptui.toml` is read when it exists (see
below) but is never created --- a directory the user did not ask ChipTUI to
modify stays as they left it.

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

The file is named `chiptui.toml` and lives at the project root. ChipTUI
reads it and lets it win over everything else --- it is the most specific
answer there is, it travels with the project, and it can be committed so a
team shares it. ChipTUI does not create it: the persisted counterpart of the
automatic prompt is the registry entry (§13), and the file is the one manual
override --- there is no dashboard action that swaps the backend of a session
anymore; a session that started on the wrong backend switches projects
(`shift+p`) instead. Writing this file is the user's decision, made by
putting it there.

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

### Projects folder and project selection

MicroPython makes the project a question the same way Zephyr does (under
`ProjectSelect`, §6): a **projects folder** (`[micropython] projects`, user
config only --- a MicroPython project pins no environment of its own) and a
**project** picked from its immediate subdirectories. MicroPython runs
source directly, so any subdirectory is a project: the picker marks none
and refuses none. The pick is session-only and re-roots the file browser's
local pane (§9's `src/` convention applies inside the picked project);
nothing is written.

### Filesystem

The local side of the dual-pane browser (§11) opens on the project's `src/`
directory rather than the project root, so what it shows is exactly what a
future upload would send to the device --- `firmware/` and any project
tooling files stay out of the way. A project without a `src/` (one that
predates it, or was never routed through the empty-project prompt above)
falls back to the project root.

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

### Running scripts and interruption

`mpremote` interrupts whatever user code is running (Ctrl-C, then raw REPL)
for *every* filesystem, `exec` or `df` command, so a board executing a
blocking `main.py` loop would have its script silently stopped by the first
listing. The TUI handles this in three parts:

-   **Probe first.** Before the first filesystem operation on a selected
    device, a short `mpremote repl` session watches the serial output:
    a `>>> ` prompt means the board is idle, output with no prompt means a
    script is running. The session is closed with mpremote's own escape
    (Ctrl-]) --- nothing is sent to the board, so the probe itself never
    interrupts. A script that never prints is indistinguishable from a
    silent idle board; the probe gives up and the operation proceeds
    ungated (the pre-probe behavior). The monitor view reaches the same
    verdict while it is open.
-   **Ask before interrupting.** While a script is believed running, device
    operations are held behind an explicit confirmation instead of silently
    stopping the board's work. Declining drops the held operations.
-   **Offer to restore.** After an accepted interruption finishes, the user
    chooses how to bring the script back: a hard reset (clean state,
    re-runs `boot.py` and `main.py`), restarting `main.py` without a reset
    (faster, but leftover state survives), or leaving the device stopped.

A script that swallows Ctrl-C (`except:` around its loop) can hold the REPL
even against confirmed commands; the resulting "could not enter raw repl"
failure is reported with the way out (interrupt it from the monitor, or
restart the board).

### REPL / Monitor

The REPL view must support interactive input without buffering the
session through a normal line-oriented command abstraction.

The implementation should treat the session as an interactive
terminal/serial stream and preserve terminal input/output semantics.

### Flashing

`esptool` operations should be presented separately from normal
MicroPython filesystem operations: the device pane's second tab,
**Project actions** (opened with `x`; the arrow keys switch between it
and **Device files** from either side while the pane has focus, the same
rule as the Log/Monitor strip), carries the
actions as the same stacked-button group the Zephyr project panel uses,
including its reserved state/`Stop` footer --- one action grammar across
backends. The per-action options screen and the online firmware screens
remain dialogs layered over the dashboard.

Potential actions:

-   chip information;
-   flash information;
-   erase flash;
-   write/flash firmware;
-   verify;
-   reset;
-   search firmware online;
-   paste a firmware URL.

`erase_flash` must require explicit confirmation.

### Firmware discovery and download

Flashing should not require the user to already have a firmware file on
disk. With a connected device and a known chip family (read from an
`esptool` banner, or picked manually), the TUI should be able to search
[micropython.org/download](https://micropython.org/download/) for candidate
firmware builds:

-   the search narrows by MCU (chip family) always;
-   it narrows by board vendor as well, but only when the connected
    device's USB vendor/product id identifies an actual board vendor rather
    than a generic USB-serial bridge chip (e.g. CP210x, FTDI, CH340 are used
    across many unrelated boards and must not be treated as a vendor
    filter);
-   results are presented as a list of candidate boards, and then a list of
    firmware builds for the chosen board (version, date, variant);
-   the user may paste a specific download URL directly instead of
    searching;
-   the search view names its source (the exact URL being queried) and
    states that a `.bin`/`.elf` added to the project's `firmware/`
    directory is picked from there first --- the online list is the
    fallback for an empty folder, never the silent winner over a local
    image. Selecting write/flash with no local firmware file opens this
    search straight away instead of dead-ending on a warning.

The chosen file is downloaded into the project's `firmware/` directory so it
becomes an ordinary local firmware candidate for the existing flash
operations above, which also look there.
Fetching the download page and downloading the firmware file are both
delegated to an external tool (`curl`) rather than adding a bundled HTTP
client, consistent with §22's "delegate rather than reimplement." This tool
is only required when this specific feature is used --- it must not be
reported as a missing requirement for the MicroPython backend in general.

After a successful download, the user is asked whether to proceed with
`erase_flash` and writing the new firmware, with the same explicit,
per-step confirmation as any other destructive flash operation (§15) ---
never a combined step that skips confirming each one.

## 10. Zephyr Backend

### Tools

Primary tools:

-   `west`
-   CMake
-   Ninja where used by the generated build system.

`west` is the primary Zephyr orchestration interface.

### Environment (workspace, venv, SDK)

A Zephyr machine has three pieces, and the backend locates all of them
before running anything:

1.  the **west workspace** (`west init`'s directory: `.west/`, the Zephyr
    checkout, and by convention `.venv/`);
2.  the **venv** where `west` is installed;
3.  the **Zephyr SDK** (toolchain), which CMake finds on its own unless a
    location is configured.

The location of the installation comes from configuration and nowhere
else --- no directory conventions are assumed, no environment variables are
consulted. The startup flow:

1.  read `[zephyr] workspace` from the project's `chiptui.toml`, then from
  the user config (`~/.config/chiptui/config.toml`);
2.  when neither file names a location, ask immediately: a directory picker
  (a real filesystem browser, starting at the user's home) where the user
  navigates to their installation and accepts it;
3.  validate whatever the config or the picker says through the same rules:
  a directory without `.west/` is not a Zephyr installation, and a
  workspace without its checkout is half of one. A failure keeps the
  picker open (or the pane red) with the reason and a link to the
  [installation guide](https://docs.zephyrproject.org/latest/develop/getting_started/index.html);
4.  a validated pick is saved to the config (the user config, or the
  project's `chiptui.toml` when the project pins its own location), so
  the file remains the single source of truth and later starts never
  re-ask.

The `west` executable is the configured `west` key when present, else the
workspace's `.venv/bin/west` when it exists, else `west` from `PATH`. No
venv activation is performed or needed: executing the venv's console
script directly is the activated environment, and the pieces `activate`
adds are injected per command (`ZEPHYR_BASE` always --- derived from the
installation, never set by the user --- so an application outside the
workspace still finds it; `ZEPHYR_SDK_INSTALL_DIR`, `PATH`
and `VIRTUAL_ENV` when a venv/SDK is known).

Every command still runs with the project root as its working directory;
only workspace-scoped operations (`west update`) run in the workspace
itself. Status reads are files, not subprocesses: the
Zephyr version comes from `zephyr/VERSION`, the SDK version from
`sdk_version`.

#### Installing Zephyr

A machine with no installation at all is a fifth answer the picker used to
have no room for: it could only refuse. The **installer** is that answer.
It is reached from the `Install Zephyr` button --- the row `Update Zephyr`
becomes while nothing is resolved, since the two are mutually exclusive ---
or with `i` from the picker that just refused a folder. Either door asks
*where* first, and creates the workspace as `zephyr/` inside the accepted
folder (a folder that already carries a `.west/` is resumed in place
instead, never nested inside a second one).

What it runs is the [getting started
guide](https://docs.zephyrproject.org/latest/develop/getting_started/index.html),
in order, through the ordinary process manager, with the output streaming
into the modal that shows it. What it does **not** do is install anything
system-wide. The distinction is the whole design:

-   **Prerequisites are reported, never installed.** `cmake` (≥ 3.28.0),
    `dtc` (≥ 1.4.6) and `pyenv` are queried for their versions, compared
    against Zephyr's documented minimums, and shown as a checklist. A row
    that fails names the command that would fix it --- one line per package
    manager the machine actually has (`pacman`, `apt`, `dnf`, `zypper`,
    `brew`), or the upstream page when none is recognised. While a failing
    row remains, the sequence cannot start: the action button is dimmed and
    `Enter` on it does nothing. `r` re-checks.
-   **The system Python is reported but never blocks.** It is shown with a
    `⚠` when it is not 3.12, because the workspace's interpreter does not
    come from it: the installer pins its own through pyenv. Blocking on a
    row the installer exists to satisfy would be a checkbox nobody could
    tick.
-   **Python is pinned, not assumed.** `pyenv install --list` picks the
    newest 3.12.x (pre-releases and alternative implementations excluded),
    `pyenv install --skip-existing` builds it and `pyenv local` writes
    `.python-version`. The venv is then created by that interpreter's
    *absolute* path, read from `pyenv root` --- `python3` off `PATH` only
    honours the pin through pyenv's shims, which may not be installed.
-   **Nothing is overwritten, and nothing is repeated.** Each step's result
    is detected from the filesystem (`.python-version`, `.venv/bin/west`,
    `.west/`, `zephyr/VERSION`, a `zephyr-sdk-*` directory), so a run
    interrupted by a failure, a `Stop` or a reboot resumes exactly where it
    stopped. `west packages pip --install` and `west zephyr-export` are the
    two exceptions: neither leaves a marker in the workspace, and both are
    idempotent, so they always run.
-   **The SDK is optional and selective, and its toolchains are a curated
    list.** Nothing can enumerate them beforehand: `west sdk list` reads the
    CMake user package registry and answers only once an SDK is *installed*
    (on a fresh machine it exits with `FATAL ERROR: No Zephyr SDK
    installed.`), and the valid names come from the GitHub release assets
    that `west sdk install` fetches for itself. So the picker offers a
    constant list, anchored to the workspace's own `SDK_VERSION`, and a
    stale name fails loudly --- `west sdk install` validates every one and
    dies printing the list it accepts. Picking is **required** --- with no
    `-t`, west passes `-t all` and pulls 35 toolchains, several GB, with no
    prompt --- but it is a question about the *last* step, so it never holds
    up the eleven before it: unanswered, the action button reads
    `Pick SDK toolchains` and opens the picker. `s` skips the SDK outright,
    which answers the question just as well. The bundle is placed with
    `-b <workspace root>` (absolute), which produces
    `<root>/zephyr-sdk-<version>` --- **not** `-d`, whose argument is the SDK
    directory's final *name* and which breaks `setup.sh` (see below). The
    step runs in the manifest checkout for the same reason the guide's `cd`
    does --- so west resolves the workspace and reads
    `${ZEPHYR_BASE}/SDK_VERSION` --- and `west sdk list` runs after it, as
    the confirmation it can actually be.
-   **The bundle detection is version-aware, and so is the toolchain
    detection.** The workspace pins its SDK version, and a bundle names
    itself with the version it is, so `<root>/zephyr-sdk-<pinned>` is the
    only one that counts --- a Zephyr version bump leaves the SDK step
    pending again rather than letting a build quietly use the wrong
    toolchains. Inside a bundle, what is *installed* is the directories
    under `gnu/` (the older layout keeps them in the bundle root), not the
    list of what the bundle offers.
-   **A toolchain can be added later, and `s` is the way there.** From the
    dashboard, `s` opens the toolchain picker over the configured
    installation directly --- the errand is routine (a new board needs a
    target the bundle was not unpacked with) and the path question the
    installer would otherwise re-ask is already answered in the config.
    Without a resolved installation the key invents nothing: it says so and
    points at `Zephyr path`. Picking a toolchain an installed SDK does not
    carry turns the action into `Add SDK toolchains`, and the command asks
    west only for the names that are absent. That costs a
    `setup.sh -t` per toolchain and no download of the bundle: with the
    version already registered, `west sdk install` reports it is using the
    existing SDK and goes straight to setup. The picker marks what is
    already there, so the two are never confused.
-   **The action button is one decision.** Its label, whether it is enabled,
    and what `Enter` does all come from a single answer to "what is this
    button now" --- start, pick toolchains, retry, adopt, install the SDK,
    stop. Only two states are inert, and both have their explanation already
    on screen: an unanswered prerequisite, and nothing left to run.
-   **A running installation is not left by reflex.** `esc` closes the modal
    only while nothing runs; `Stop` is the way out of a running step, and it
    is on screen.
-   **A failure costs only what it has to.** A step whose result nothing
    downstream needs --- the SDK confirmation --- is marked and stepped over
    rather than stopping the run. And when a step *does* stop the run, what
    already succeeded is still recorded: if `west init` and `west update`
    left a valid workspace, `[zephyr] workspace` is written anyway, with the
    log saying which step still needs to run. The modal stays open on the
    failed step, which `Retry` resumes from.

One confirmation covers the whole sequence, naming the target folder, the
cost (several GB) and the literal first command. A finished installation is
persisted the same way every environment answer is --- `[zephyr] workspace`,
plus `[zephyr] sdk` when a bundle landed --- and the flow continues into the
projects-folder question below, but only while that question is still open.

##### A second installation, and adopting an existing one

`Zephyr path` is always answerable --- that is how the answer is *changed* ---
so it is also how a machine that already has Zephyr installs another one
somewhere else. Accepting a directory the installation check refuses no longer
just states the reason: it offers the way forward, and the offer says what is
actually at the target rather than always the word "install".

-   nothing there --- *Install Zephyr in here?*
-   `.west/` without its checkout --- *Finish the installation in here?* The
    installer resumes it in place; a folder that already carries a `.west/` is
    the workspace, never the parent of a second one nested inside it.
-   a complete installation in the folder's `zephyr/` --- *Use the installation
    in here?* The picker validates the directory it was given and not its
    children, so this is the one case it cannot accept on its own. Answering
    yes opens the installer in **adopt** mode: the checklist is shown as
    evidence, the action reads `Use this installation`, and it records the
    location without running a single command. An installation missing only
    its SDK bundle --- skipped, or its step failed --- is adopted the same
    way but offers `Install the SDK`, which records the location first and
    then runs that one step; without it, skipping the SDK once made the
    installation impossible to finish. Adopting is deliberately not gated on
    the prerequisites --- those gate *building* Zephyr, not writing down
    where an existing one lives.

Declining any of these returns to the picker with the refusal still on screen.

A second installation replaces the first in the config: someone who just
installed Zephyr somewhere means to use it. The switch is named in the log
(`Zephyr installation switched from A to B`) rather than left to be noticed in
the pane.

#### Projects folder and project selection

Applications can live anywhere too, unrelated to the installation. Which
one is being built is never guessed:

1.  the **projects folder** (`[zephyr] projects`) holds the user's
    applications --- any directory, resolved from the same two config
    levels as `workspace`. Unset, the Project pane's chooser (a
    directory picker, validated by existence only) answers it and saves
    the pick the same way;
2.  the **project** is an immediate subdirectory of that folder, chosen in
    the project picker, which lists every subdirectory and marks whether
    it holds build elements (a `CMakeLists.txt` --- `west build`'s one
    hard requirement). A directory without them cannot be accepted: the
    picker stays open and says why. The choice is session-only; nothing
    is written. The header's `project` field follows it: it names the
    picked folder, and stays empty until a project is chosen (a launch
    directory that already is one fills it by itself);
3.  before any project command (build, clean, rebuild, menuconfig,
    flash, dashboard) runs, its working directory must hold those build elements.
    The launch directory passes the gate by itself when it is a project;
    otherwise the command is refused with the reason and the pickers
    above open --- folder first, then project. The accepted project
    re-roots every command and resets the per-project facts (build
    directory, cached board, saved board/shield, last report); a
    hand-picked board survives the re-root, and the new project's own
    saved answers (below) are re-applied. The lifecycle buttons stay
    dimmed in the project panel until both answers exist --- the questions
    themselves are asked in row 1's Project pane checklist, below
    `Projects base`.

The optional keys, shared by both config levels:

``` toml
[zephyr]
workspace = "~/zephyrproject"
projects = "~/zephyrapps"
# sdk = "~/zephyr-sdk-0.17.1"   # written by the installer when it installs one
# west = "/custom/venv/bin/west"
```

### Operations

The initial backend should support:

-   environment resolution (workspace/venv/SDK, above);
-   installing the environment when there is none (the installer above);
-   projects folder and project selection (the gate above);
-   board selection;
-   shield selection (optional, `--shield`);
-   project information;
-   build (targeting the conventional `build` directory in the project);
-   clean;
-   `menuconfig` (interactive: the TUI suspends, like `$EDITOR`);
-   the build dashboard (`west build -t dashboard`, Zephyr 4.4+: one HTML
    report over the configured build directory, opened in the browser);
-   flash;
-   serial monitor;
-   build output/logs;
-   `west update` (workspace-scoped).

Potential future operations:

-   debug;
-   signing;
-   device-tree inspection;
-   configuration helpers.

### Board selection

The backend should discover or expose the configured Zephyr board.

If the board cannot be determined unambiguously, the user should be able
to select it.

The board selection should not silently modify project configuration.

> **Status**: implemented. The configured board is read from
> `build/zephyr/CMakeCache.txt` (`build::cached_board`); the build panel's
> `Board` action opens a filterable picker over a background `west boards`
> fetch. A pick is saved in the project's registry entry (§13) and
> reloaded on every later open, outranking the cache; the panel header
> says which origin the answer has. Nothing is written into the project
> directory.

### Shield selection

A shield (an add-on board) is optional: the target builds without one.

When chosen, the shield enters the build's first configuration as
`--shield`; a pick must not silently modify project configuration.

> **Status**: implemented. The Project pane's target row --- the board
> with the shield riding on the same line, `←`/`→` switching which half
> `Enter` acts on --- opens the same filterable picker over a background
> `west
> shields` fetch, with a leading `(none)` row --- that is how a pick
> clears. The answer is saved beside the board in the registry entry and
> reloads with it (clearing persists too), and `west build` /
> `west build --pristine=always` carry it only while one is set.

### Build

The UI should provide:

``` text
Build
Clean
Rebuild
Menuconfig
```

The lifecycle targets the conventional `build` directory inside the
project (`west`'s own default, so commands stay implicit); the panel's
list offers no directory picker --- keeping several parallel
configurations is the shell's job, not the TUI's.

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

> **Status**: implemented as the project panel's `Flash` button --- a plain
> `west flash`, which delegates to the board's own runner from the build
> directory's `runner.yml` (no port or programmer is ever assumed). The
> dashboard's `x` routes a build-panel backend here and a filesystem backend
> to the esptool dialog. Destructive (`SPEC.md` §15): it always runs through
> a confirm quoting the literal command.

### Monitor

Provide a serial monitor where appropriate.

The monitor should be independent from the build process so that a
build/flash failure does not corrupt the terminal state.

> **Status**: implemented for both backends, as one PTY session in the
> Monitor tab (`m`). MicroPython connects through `mpremote`; Zephyr runs
> `west monitor`, with the port named when discovery found one. Port
> discovery for a backend without `mpremote devs` is a plain USB serial
> walk (`device::usb_serial_ports`: `/dev/ttyACM*`, `ttyUSB*`, …) feeding
> the same selection/picker flow.

## 11. UI / UX

The application should use a contextual dashboard.

### Home screen

Shown when the working directory names no project (§7), and reachable from
the dashboard to switch projects. One centered panel: a create row, a search
field that filters as it is typed, and the recorded projects under it, each
row `<icon> <backend>  <name>  <path>`. A row is tinted with its backend's
color --- deepened, not reversed, under the cursor --- so the kinds separate
at a glance without a legend.

`↑/↓` moves, `enter` opens, `del` forgets an entry (never the directory),
`esc` clears the search and then leaves. Every printable key goes to the
search field, which is why the commands are the non-printing ones.

### Dashboard layout

The Dashboard view is three rows, stacked top to bottom, below a one-line header and above a
one-line contextual shortcut footer:

``` text
┌───────────────────────────────────────────────────────────────┐
│ ChipTUI Backend ◆ Zephyr      Project esp32c3_basic      ● /dev/ttyACM0 │
├───────────────────────────────┬───────────────────────────────┤
│ Project                       │ Device                        │
├───────────────────────────────┴───────────────────────────────┤
│ Files: Local           │ Files: Device                        │
│  (or, when the backend declares no Capability::Filesystem,    │
│   a single full-width placeholder pane)                       │
├───────────────────────────────────────────────────────────────┤
│ Log │ Monitor │ Terminal                                     │
├───────────────────────────────────────────────────────────────┤
│ Contextual keyboard shortcuts                                 │
└───────────────────────────────────────────────────────────────┘
```

- **Row 1** --- Project and Device, side by side, both a fixed four content rows (shorter
  content is padded with blanks) so the rows below never shift when a workspace resolves or
  device details accumulate. The Project pane is the checklist the environment's questions
  live in --- navigable through the shortcuts overlay's `e` letter (`ctrl+k`; a deliberate
  detour off the `Tab` tour: the pane holds questions, not work; `Tab` leaves it back onto the
  tour, and the cursor lands on the first question still open). Zephyr asks
  `Zephyr path`, `Projects base`, `Project path`, then `Board` with its optional `Shield`
  riding the same line (`←`/`→` switch which half `Enter` acts on); MicroPython (under
  `ProjectSelect`) asks `Projects base` (`[micropython] projects`) and `Project path` (a
  session-only pick that re-roots the file browser's local pane), then reports
  `Dependencies` (`requirements.txt`/`manifest.py` presence) and `Script` (whether the
  board is believed to be running user code right now). The board's firmware version rides
  the Device info pane's `Firmware` row instead, read from the same identification window
  that named the firmware: MicroPython and Zephyr compile their banners into the image
  (`Firmware: MicroPython v1.28.0`, `Firmware: Zephyr v4.0.0`), and a plain ESP-IDF app's
  descriptor carries its stamped build version (`Firmware: ESP-IDF v5.3.1`); MicroPython
  falls back to the REPL banner the probe/monitor already sees when the read found no
  version string, and a firmware that names no version stays bare rather than guessed.
  The one layout the window cannot date --- a Zephyr *simple boot* image, whose application
  banner sits deep in flash past it --- gets a follow-up read (the next 512 KiB) that only
  dates the verdict already standing; a hunt that finds nothing, or a board that went away,
  changes nothing. Whenever the flash contents change the verdict is re-read: after the
  esptool flow's erase/write the next listing re-identifies, and after a successful
  `west flash` from the build panel --- which no listing drives --- the identification runs
  again on its own once the port frees. Every row is a `□` while open,
  a `✓` once answered, a red `✗` when a configured answer fails validation. The
  environment's `versions` (Zephyr and venv Python, read from files) ride the pane's
  bottom border's right edge once a workspace resolves --- a late-arriving fact that
  costs no content row; missing tool
  availability shows in the header as a red `⚠ N` beside the backend name (names in the
  log warning).
- **Row 2** --- the dual-pane local/device file browser, shown whenever the selected backend
  declares `Capability::Filesystem`; for a backend that builds without a device filesystem
  (today: Zephyr) the whole row is the pair **Project files | Project actions**: the
  project's own file list (the pane's title carries the walked path,
  `Project files: name/src/`; no action menu --- `Enter` descends or hands a text file to
  `$EDITOR`, `v` views, `Del` asks, `a` creates, `r` renames) beside the
  project panel (the build lifecycle). The project panel is buttons only
  (`Zephyr Actions` --- a stacked-button submenu holding `west update`, adding
  SDK toolchains, and the build dashboard --- menuconfig, the lifecycle, flash)
  over a three-row footer
  that is always reserved --- the pane's height never changes when a command starts --- and
  splits horizontally while one runs: the build status on the left half, a `Stop` button
  (same widget, half the pane's width) on the right, with the stack's buttons dimmed for as
  long as the panel's one process slot is occupied. Stopping kills the command's whole
  process tree (children run in their own process group), so a delegating tool like `west`
  cannot leave its helpers running after the cancellation. A filesystem backend that can also
  flash (today: MicroPython) keeps the dual-pane browser and gains the same button-group
  grammar as the device pane's second tab: a `Project actions • Device files` strip on the
  pane's border, the esptool actions plus the online-firmware entries as the stacked
  buttons, the same reserved state/`Stop` footer, and row 2 sized to the stack while that
  tab is showing. No file
  listing of the environment itself for such a backend --- editing the project's own
  sources beyond the list is the user's editor's job; otherwise a single full-width
  placeholder while no pane exists yet.
- **Row 3** --- a one-line `Log`/`Monitor`/`Terminal` tab strip over the selected tab's
  body, full width. `Left`/`Right` switch tabs while row 3 has focus, one step per press
  and clamped at the ends. `Log` is the rolling status/notice feed
  (unchanged). `Monitor` shows whichever live process output the user last asked for: a
  running or just-finished flash/erase command (`esptool`), or a live device serial session
  once one exists; the tab itself only appears for a backend with
  `Capability::Monitor`. `Terminal` runs the user's own shell (`$SHELL`, `/bin/sh` as
  fallback) in a PTY inside the project directory --- always offered, because a local
  shell is a UI affordance, not a backend operation. It is a *terminal*, not a console:
  the pane is an emulated screen, so the user's own prompt arrives with its colours,
  its glyphs and its layout intact, and full-screen programs (`vim`, `less`, `htop`)
  work. The pane sizes the emulator and the child to itself, so a prompt that places a
  right-hand segment by column lands where it means to. Entering the tab starts the
  shell; while the tab holds focus the shell owns the keyboard (`ctrl+c` interrupts the
  shell's foreground command instead of quitting ChipTUI, and every editing, function
  and Meta key reaches its line editor), `shift+pgup`/`shift+pgdn` reach the scrollback
  the shell would otherwise consume, `ctrl+]` detaches --- the shell keeps running and
  streaming into the tab while the keyboard returns to the dashboard --- and the shell's
  own `exit`/`ctrl+d` ends the session, leaving its screen behind for scrolling.

The Flash view's options and online screens are dialogs layered over the dimmed dashboard
for choosing and configuring an action, never full-screen replacements. Starting an action
from the actions tab shows the Monitor tab without moving focus off the pane (the cursor
waits on `Stop`); starting one from a dialog closes it and moves focus to row 3's Monitor
tab, where its output streams --- there is no separate output screen.

> **Status**: implemented. Row 2 is capability-driven: the dual-pane file browser for
> MicroPython; for Zephyr the full row is Project files (the project's own listing, the
> checklist having moved up to row 1's Project pane) | Project actions (`src/build.rs`: the
> lifecycle buttons only, gated on both answers,
> streaming into
> the Monitor tab; commands are quoted by the confirm overlays, not on the rows). For
> MicroPython the device pane is a tabbed pane (`src/ui/files.rs`): **Project actions**
> (the esptool menu as the same button-stack widget, `src/ui/flash.rs`) • **Device files**,
> with `x` and the pane's arrow keys switching. The
> Monitor tab shows the device serial session (`m`), flash/erase output, and build output.

The exact proportions (row heights, column widths) are not fixed.

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

### Theming

Every visible color is derived from the active `ratatui-themes` palette
(default: Tokyo Night, overridable via `[ui] theme` in the user
config), so switching the theme recolors the whole application:

-   selected rows (lists, pickers, checklists, buttons) use the theme's
    `selection` background under its `fg` --- never `REVERSED`, which
    swaps the terminal's own defaults and ignores the theme;
-   secondary text (field labels, legends, hints, timestamps,
    placeholders) uses the theme's `muted` color;
-   state carries meaning through `error`/`warning`/`success`/`info`.

Besides the fixed themes, the picker (`t`) offers `Auto` (stored as
`theme = auto`): the theme follows the active backend --- Catppuccin
Mocha for a Zephyr project, Everforest for a MicroPython one, with
Tokyo Night standing in wherever no backend is active (the home
screen, an unresolved project). Any fixed pick applies to all projects
alike.

The one deliberate exception is the Monitor pane's fake terminal
cursor, which mimics the terminal's own reverse-video cursor rather
than reading as a selection. On the home screen, each backend's row
tint is its own semantic color (MicroPython `success`, Zephyr `info`)
blended toward the theme's background, so light themes get light tints.

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

[zephyr]
workspace = "~/zephyrproject"
projects = "~/zephyrapps"
# sdk = "~/zephyr-sdk-0.17.1"
# west = "~/zephyrproject/.venv/bin/west"

[ui]
log_panel = true
mouse = false
```

The `[zephyr]` keys are implemented (§10); the same section in a project's
`chiptui.toml` overrides the user-level values for that project.

### Project registry

The same file records which directories are ChipTUI projects --- the answer
that used to be a marker file inside each of them (§7):

``` toml
[projects]
last_parent = "~/zephyrapps"

[[project]]
path = "~/zephyrapps/blinky"
backend = "zephyr"
name = "blinky"
board = "nrf52840dk/nrf52840"
# shield = "nrf7002ek"
last_opened = "2026-08-16T14:03:11Z"
```

One block per project, written when a project is opened or created. It is
what the home screen lists (most recently opened first) and what detection
consults before falling back to evidence. `last_parent` is where the project
creator's folder picker starts. `board`/`shield` are the Zephyr pickers'
persisted answers: written when the user picks, re-applied every time the
project opens (outranking the build directory's cache), and a cleared
shield removes its line.

The blocks are machine-managed: they are rewritten as a whole, while
everything else in the file --- other sections, comments, unknown keys ---
is preserved. An entry whose directory no longer exists is not listed, and
is dropped on the next write.

### Project configuration

A project may carry its own `chiptui.toml`. ChipTUI reads it but never
writes it (§7), so it is only there because the user put it there ---
typically to commit it. Used primarily for:

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
-   clean operations that remove build artifacts;
-   workspace updates that rewrite shared checkouts (`west update`).

These should have appropriate confirmation, and every one of them follows
the same four-part grammar --- a dialog titled `Confirm` over a bare command
line satisfies none of it:

-   the **title** names the action as a question (`Erase the flash?`);
-   the **target** names what it happens to, in the warning colour: the
    board and its port, the workspace path, the project and its build
    directory. With two boards plugged in, a dialog naming neither is
    answered blind. What is unknown is said (`no board selected`), never
    filled in;
-   the **consequence** says what is lost, in one plain sentence;
-   the **command** itself is quoted underneath, muted.

`No` is the default in all of them.

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

### Firmware site markup changes

The MicroPython download site has no machine-readable API; firmware
discovery (§9) parses its HTML directly.

Mitigation:

-   isolate all parsing behind one tested module, fixture-driven so the
    normal test suite never depends on the live site;
-   treat a parse failure as "found nothing" rather than a crash, and always
    allow the user to paste a direct download URL instead.

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
