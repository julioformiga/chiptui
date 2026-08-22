//! Guards `AGENTS.md`'s TUI Guidelines rule: no Private Use Area codepoints
//! anywhere in `src/`. A PUA glyph (Nerd Font icons and the like) renders as
//! tofu or blank space on a terminal without that exact font installed, with
//! no fallback --- `src/ui/home.rs` shipped one (`U+F11EC`) on the home
//! screen's primary action until this test was added.
//!
//! The one sanctioned PUA use, `src/icons.rs`' opt-in Nerd Font rendering,
//! writes its glyphs as `\u{…}` escapes rather than literal characters, so
//! even that file stays scannable here without an exception list; the
//! BMP-only, single-char contract those escapes must keep is pinned by
//! `src/icons.rs`' own unit tests.

use std::path::Path;

/// The three Unicode Private Use Area blocks (Basic Multilingual Plane, Plane
/// 15, Plane 16).
fn is_private_use(c: char) -> bool {
    matches!(c as u32,
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, violations);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (line_no, line) in contents.lines().enumerate() {
            for c in line.chars() {
                if is_private_use(c) {
                    violations.push(format!(
                        "{}:{}: U+{:04X}",
                        path.display(),
                        line_no + 1,
                        c as u32
                    ));
                }
            }
        }
    }
}

#[test]
fn source_carries_no_private_use_codepoints() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    scan_dir(&src, &mut violations);
    assert!(
        violations.is_empty(),
        "Private Use Area codepoints found (renders as tofu without the exact \
         font that defines them --- use standard Unicode instead):\n{}",
        violations.join("\n")
    );
}
