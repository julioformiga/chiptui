//! The narrow YAML reader Zephyr's own generated files need.
//!
//! Two files in this backend are YAML, both machine-shaped and both read
//! for a handful of known paths: `<build>/build_info.yml`, which CMake
//! writes on every configure, and a module's `zephyr/module.yml`, which
//! declares the roots an out-of-tree module contributes. Nothing here is a
//! general YAML reader --- no anchors, no flow collections, no block
//! scalars, no multi-document files --- because nothing writes those into
//! either file. It is the same bias as the config parsers
//! ([`crate::settings`]): one known shape, hand-rolled, no dependency.
//!
//! The document is flattened into `("dotted.path", value)` pairs in source
//! order, so a caller asks for `build.settings.board_root` rather than
//! walking a tree.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Scalar(String),
    Sequence(Vec<String>),
}

/// Flattens the document into `("dotted.path", value)` pairs in source
/// order. A pair whose value is empty on its key line and is followed by
/// deeper `- ` lines becomes a [`Entry::Sequence`]; a key with neither is
/// simply a parent and contributes nothing of its own.
pub fn read_entries(text: &str) -> Vec<(String, Entry)> {
    let mut entries: Vec<(String, Entry)> = Vec::new();
    // The open path: one (indent, key) per level currently in scope.
    let mut stack: Vec<(usize, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();

        // A sequence item belongs to the key most recently opened, whatever
        // its exact indentation --- CMake writes these one space deeper than
        // the key, which is legal YAML but not what a naive `indent + 2`
        // rule expects.
        if let Some(item) = trimmed.strip_prefix("- ").or_else(|| {
            // A bare `-` with the value on the same line but no space is not
            // written here; a lone `-` (empty item) is, in principle.
            (trimmed == "-").then_some("")
        }) {
            let Some((key_indent, _)) = stack.last() else {
                continue;
            };
            if indent <= *key_indent {
                continue;
            }
            let path = dotted(&stack);
            let value = unquote(item.trim());
            match entries.iter_mut().find(|(name, _)| *name == path) {
                Some((_, Entry::Sequence(items))) => items.push(value),
                Some(_) => {}
                None => entries.push((path, Entry::Sequence(vec![value]))),
            }
            continue;
        }

        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        while stack.last().is_some_and(|(open, _)| *open >= indent) {
            stack.pop();
        }
        stack.push((indent, key.trim().to_string()));
        let rest = rest.trim();
        if !rest.is_empty() {
            entries.push((dotted(&stack), Entry::Scalar(unquote(rest))));
        }
    }
    entries
}

fn dotted(stack: &[(usize, String)]) -> String {
    stack
        .iter()
        .map(|(_, key)| key.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// The scalar at `path`, `None` when absent or empty. An empty string is
/// treated as absent on purpose: `revision: ''` is how CMake writes "this
/// board has no revision", and a row reading `Revision: ` says less than no
/// row at all.
pub fn scalar(entries: &[(String, Entry)], path: &str) -> Option<String> {
    entries.iter().find_map(|(name, entry)| match entry {
        Entry::Scalar(value) if name == path && !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

pub fn sequence(entries: &[(String, Entry)], path: &str) -> Vec<String> {
    entries
        .iter()
        .find_map(|(name, entry)| match entry {
            Entry::Sequence(items) if name == path => Some(items.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Strips the quoting CMake writes. Single quotes are what `build_info.yml`
/// uses; double quotes are accepted because nothing costs, and a YAML `''`
/// inside a single-quoted scalar is the one escape that form has.
pub fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return inner.replace("''", "'");
    }
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `zephyr/module.yml` carries: the settings that matter are
    /// nested under `build:`, and a top-level `settings:` block is a
    /// different path entirely --- which is precisely the trap west's
    /// `scripts/zephyr_module.py::process_settings` sets, since it parses
    /// the misplaced block without error and then ignores it.
    #[test]
    fn a_modules_nested_settings_are_addressed_by_their_whole_path() {
        let entries = read_entries(
            "name: ttgo-t-display-s3\nbuild:\n  cmake: .\n  kconfig: Kconfig\n  \
             settings:\n    board_root: .\n    dts_root: .\n",
        );
        assert_eq!(
            scalar(&entries, "build.settings.board_root").as_deref(),
            Some(".")
        );
        assert_eq!(
            scalar(&entries, "name").as_deref(),
            Some("ttgo-t-display-s3")
        );
        // A top-level `settings:` is a different key and must not answer
        // for the nested one.
        let entries = read_entries("name: m\nsettings:\n  board_root: .\n");
        assert_eq!(scalar(&entries, "build.settings.board_root"), None);
        assert_eq!(
            scalar(&entries, "settings.board_root").as_deref(),
            Some(".")
        );
    }
}
