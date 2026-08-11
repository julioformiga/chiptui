# ChipTUI

**ChipTUI** is a terminal user interface (TUI) written in Rust for embedded development workflows.

It is designed as an orchestration and visualization layer over existing embedded-development tools, providing a fast, keyboard-driven environment without attempting to replace your editor or IDE.

## Features

- **Project-Aware**: Automatically detects your current project type and selects the appropriate backend.
- **Backend Support**:
  - **MicroPython** (via `mpremote` and `esptool`)
  - **Zephyr** (via `west`, `CMake`, and `Ninja`)
- **Device Management**: Detect and manage connected devices easily.
- **Consistent UI**: A unified interface across different embedded ecosystems, built with `ratatui` and `crossterm`.
- **Fast Feedback**: Immediate visibility for build, flash, and command progress.
- **Non-Intrusive**: Delegates to established tools rather than reimplementing their protocols.

## Installation

Ensure you have Rust installed on your system. You can build the project from source:

```bash
git clone <repository-url>
cd chiptui
cargo build --release
```

The compiled binary will be located in `target/release/chiptui`.

### System Dependencies

Depending on the backend you are using, you will need the corresponding CLI tools available in your `PATH`:

- **For MicroPython**: `mpremote`, `esptool`
- **For Zephyr**: `west`, `cmake`, `ninja`, and the appropriate Zephyr toolchain.

## Usage

Navigate to your embedded project directory and run ChipTUI:

```bash
cd /path/to/your/embedded/project
chiptui
```

ChipTUI will automatically detect the project type and present the available actions (such as build, flash, or monitor) based on the current backend's capabilities.

## Documentation

- [`SPEC.md`](./SPEC.md): Product and architecture specification.
- [`AGENTS.md`](./AGENTS.md): Implementation rules and development workflow guidelines.

## Contributing

Before contributing, please read both `SPEC.md` and `AGENTS.md` to understand the core principles and rules. All changes should ensure that backends remain isolated and that the UI remains responsive and keyboard-first.
