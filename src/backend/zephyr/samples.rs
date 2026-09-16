//! The Zephyr tree's samples as starting points for a new project.
//!
//! A workspace checkout carries hundreds of ready applications under
//! `<zephyr_base>/samples/` (`basic/blinky`, `subsys/shell/shell_module`,
//! ...), each one `west build`-ready. When a brand-new, empty directory is
//! given the Zephyr backend, one of them is a better starting point than
//! the minimal scaffold for anyone who means to study a subsystem --- so
//! after the backend answer is applied, the user is offered the list and
//! may pick one as the project's starting layout (`SPEC.md` §7).
//!
//! The listing's bar for "a sample" is the build's own bar: a directory
//! holding a `CMakeLists.txt` that calls `find_package(Zephyr)` --- the
//! same test [`super::projects::is_buildable`] applies to the user's
//! applications. Anything else under `samples/` (documentation, Kconfig
//! fragments shared between samples) is not a choice, so it is not listed.
//!
//! Copying follows [`crate::project::scaffold`]'s rule: a file already in
//! the destination is never overwritten. Sample trees in a used checkout
//! may carry local build output, so directories named `build*` are left
//! behind.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::project::scaffold::Created;

/// One buildable sample under the tree's `samples/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The path relative to `samples/`, exactly what the picker lists and
    /// the log names (`basic/blinky`).
    pub rel: String,
    /// The sample's absolute directory --- the copy's source.
    pub dir: PathBuf,
    /// The first readable README in the supported preference order, cleaned
    /// for a terminal rather than carrying reStructuredText/Markdown marks.
    pub description: Option<Description>,
}

/// Text shown beside a sample row, plus the file that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub source: &'static str,
    pub text: Arc<str>,
}

impl Sample {
    pub fn new(rel: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let description = read_description(&dir);
        Self {
            rel: rel.into(),
            dir,
            description,
        }
    }
}

/// Every buildable sample under `samples_dir`, sorted by relative path.
/// An unreadable or absent `samples/` simply lists nothing: the caller's
/// fallback is the minimal scaffold, so a checkout problem never becomes
/// an error the project creation has to survive.
pub fn list_samples(samples_dir: &Path) -> Vec<Sample> {
    let mut samples = Vec::new();
    collect(samples_dir, samples_dir, &mut samples);
    samples.sort_by(|a, b| a.rel.cmp(&b.rel));
    samples
}

/// Recursive half of [`list_samples`]. A directory that is itself a sample
/// is a leaf: its subdirectories are its sources and fixtures, not further
/// choices, so the walk does not descend past one.
fn collect(root: &Path, dir: &Path, samples: &mut Vec<Sample>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if super::projects::is_buildable(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            samples.push(Sample::new(rel, path));
            continue;
        }
        collect(root, &path, samples);
    }
}

/// Reads the selected sample's local documentation once, while the samples
/// tree is discovered. Zephyr's canonical name leads; Markdown and plain
/// text are useful fallbacks for out-of-tree samples following a different
/// convention.
fn read_description(dir: &Path) -> Option<Description> {
    for source in ["README.rst", "README.md", "README.txt"] {
        let Ok(text) = std::fs::read_to_string(dir.join(source)) else {
            continue;
        };
        let text = clean_readme(&text);
        if !text.is_empty() {
            return Some(Description {
                source,
                text: Arc::from(text),
            });
        }
    }
    None
}

/// Removes the common presentation syntax from a README while preserving
/// its prose and paragraph breaks. This is deliberately not a complete reST
/// parser: the picker needs readable local context, not document rendering.
fn clean_readme(text: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = true;
    for raw in text.lines() {
        let mut line = raw.trim();
        if is_heading_rule(line) || line.starts_with(".. _") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(".. note::") {
            line = rest.trim();
            let note = if line.is_empty() {
                "Note:".to_string()
            } else {
                format!("Note: {line}")
            };
            lines.push(note);
            previous_blank = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix(".. warning::") {
            line = rest.trim();
            let warning = if line.is_empty() {
                "Warning:".to_string()
            } else {
                format!("Warning: {line}")
            };
            lines.push(warning);
            previous_blank = false;
            continue;
        }
        if line.starts_with(".. ") && line.contains("::") {
            continue;
        }
        line = line.trim_start_matches('#').trim();
        let line = clean_inline_markup(line);
        let blank = line.is_empty();
        if blank && previous_blank {
            continue;
        }
        lines.push(line);
        previous_blank = blank;
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn is_heading_rule(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(mark) = chars.next() else {
        return false;
    };
    matches!(mark, '=' | '-' | '~' | '^' | '`' | '"' | '#' | '*' | '+')
        && line.chars().count() >= 3
        && chars.all(|ch| ch == mark)
}

fn clean_inline_markup(line: &str) -> String {
    let mut line = line
        .replace(":ref:", "")
        .replace(":doc:", "")
        .replace(":file:", "")
        .replace(":command:", "")
        .replace(":option:", "")
        .replace("**", "")
        .replace("``", "");

    while let Some(open) = line.find('`') {
        let Some(close_rel) = line[open + 1..].find('`') else {
            break;
        };
        let close = open + 1 + close_rel;
        let contents = &line[open + 1..close];
        let label = contents
            .rsplit_once(" <")
            .and_then(|(label, target)| target.ends_with('>').then_some(label))
            .unwrap_or(contents)
            .to_string();
        let suffix = usize::from(line[close + 1..].starts_with('_'));
        line.replace_range(open..close + 1 + suffix, &label);
    }
    line
}

/// The samples whose relative path contains `input` (case-insensitive) ---
/// the picker's live filter, the same matching grammar the board and
/// shield lists use. Row 0 of that picker is the fixed minimal-layout
/// answer, so it never appears in this list.
pub fn filtered<'a>(samples: &'a [Sample], input: &str) -> Vec<&'a Sample> {
    let input = input.to_lowercase();
    samples
        .iter()
        .filter(|sample| sample.rel.to_lowercase().contains(&input))
        .collect()
}

/// Copies the sample's whole tree into `dest`, the new project's root.
/// Directories named `build*` --- output a used checkout may carry inside a
/// sample --- are left behind; everything else (README, `tests.yaml`,
/// board fragments) is part of the sample and goes with it. Nothing
/// already in `dest` is overwritten: the rule is [`crate::project::scaffold`]'s,
/// and the report it returns is the same [`Created`], so the log line reads
/// the same whichever starting layout ran.
///
/// The destination file names come from the source's own `read_dir`
/// entries, so nothing here joins a path the workspace did not hand us ---
/// the escape guard [`crate::project::scaffold::create`] needs cannot
/// trigger on a name that contains no separator.
pub fn copy_sample(sample: &Path, dest: &Path) -> io::Result<Created> {
    let mut created = Created::default();
    copy_into(sample, dest, dest, &mut created)?;
    Ok(created)
}

/// The recursive half of [`copy_sample`]. `root` is `dest`'s top level, so
/// reported paths stay relative to the project rather than to wherever the
/// recursion currently is.
fn copy_into(src: &Path, dst: &Path, root: &Path, created: &mut Created) -> io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let name = entry.file_name();
        let to = dst.join(&name);
        if from.is_dir() {
            if is_build_output(&name) {
                continue;
            }
            std::fs::create_dir_all(&to)?;
            copy_into(&from, &to, root, created)?;
            continue;
        }
        if !from.is_file() {
            continue;
        }
        let rel = to.strip_prefix(root).unwrap_or(to.as_path()).to_path_buf();
        if to.exists() {
            created.skipped.push(rel);
            continue;
        }
        std::fs::copy(&from, &to)?;
        created.written.push(rel);
    }
    Ok(())
}

/// Whether a directory name is Zephyr build output: `build` or any of the
/// per-variant names the convention produces (`build_sim`, `build_1`, ...).
fn is_build_output(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with("build")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chiptui-samples-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_sample(samples_dir: &Path, rel: &str) {
        let dir = samples_dir.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("CMakeLists.txt"),
            "find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\nproject(sample)\n",
        )
        .unwrap();
        std::fs::write(dir.join("prj.conf"), "CONFIG_NOTHING=y\n").unwrap();
    }

    #[test]
    fn lists_buildable_directories_by_relative_path() {
        let dir = scratch("list");
        let samples = dir.join("samples");
        write_sample(&samples, "basic/blinky");
        write_sample(&samples, "hello_world");
        write_sample(&samples, "boards/st/power_mgmt/blinky");
        // Not a sample: a CMakeLists.txt without the Zephyr package call.
        let not = samples.join("basic/not_a_sample");
        std::fs::create_dir_all(&not).unwrap();
        std::fs::write(not.join("CMakeLists.txt"), "# a module hook\n").unwrap();
        // Not a sample: documentation only.
        std::fs::create_dir_all(samples.join("basic/doc_only")).unwrap();

        let found = list_samples(&samples);
        let rels: Vec<&str> = found.iter().map(|s| s.rel.as_str()).collect();
        assert_eq!(
            rels,
            ["basic/blinky", "boards/st/power_mgmt/blinky", "hello_world"]
        );
        assert!(found[0].dir.ends_with("basic/blinky"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_descend_past_a_sample() {
        let dir = scratch("leaf");
        let samples = dir.join("samples");
        write_sample(&samples, "outer");
        // A buildable directory *inside* a sample is its fixture, not a
        // second choice.
        write_sample(&samples, "outer/tests/extra");

        let found = list_samples(&samples);
        let rels: Vec<&str> = found.iter().map(|s| s.rel.as_str()).collect();
        assert_eq!(rels, ["outer"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_matches_the_path_case_insensitively() {
        let samples = vec![
            Sample::new("basic/blinky", "/s/basic/blinky"),
            Sample::new(
                "boards/st/power_mgmt/blinky",
                "/s/boards/st/power_mgmt/blinky",
            ),
            Sample::new("hello_world", "/s/hello_world"),
        ];
        let found = filtered(&samples, "BLINKY");
        assert_eq!(found.len(), 2);
        assert_eq!(filtered(&samples, "st/").len(), 1);
        assert_eq!(filtered(&samples, "").len(), 3);
        assert!(filtered(&samples, "nothing").is_empty());
    }

    #[test]
    fn missing_samples_directory_lists_nothing() {
        let dir = scratch("missing");
        assert!(list_samples(&dir.join("samples")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn description_prefers_rst_and_cleans_common_markup() {
        let dir = scratch("description-rst");
        std::fs::write(
            dir.join("README.rst"),
            "Blinky\n======\n\nUses **LEDs** and :ref:`GPIO <gpio_api>`.\n\n.. note::\n   Set ``CONFIG_GPIO`` first.\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "# Wrong fallback\n").unwrap();

        let sample = Sample::new("basic/blinky", &dir);
        let description = sample.description.expect("the rst file is read");
        assert_eq!(description.source, "README.rst");
        assert_eq!(
            description.text.as_ref(),
            "Blinky\n\nUses LEDs and GPIO.\n\nNote:\nSet CONFIG_GPIO first."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn description_falls_back_to_markdown_then_plain_text() {
        let dir = scratch("description-fallback");
        std::fs::write(dir.join("README.md"), "# Markdown sample\n\nUseful text.\n").unwrap();
        std::fs::write(dir.join("README.txt"), "Wrong second fallback\n").unwrap();

        let sample = Sample::new("fallback", &dir);
        let description = sample.description.expect("the markdown fallback is read");
        assert_eq!(description.source, "README.md");
        assert_eq!(description.text.as_ref(), "Markdown sample\n\nUseful text.");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn copy_brings_the_tree_and_leaves_build_output() {
        let dir = scratch("copy");
        let samples = dir.join("samples");
        write_sample(&samples, "basic/blinky");
        let src = samples.join("basic/blinky");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/main.c"), "int main(void) {}\n").unwrap();
        std::fs::write(src.join("README.rst"), "Blinky\n======\n").unwrap();
        std::fs::create_dir_all(src.join("build")).unwrap();
        std::fs::write(src.join("build/zephyr.elf"), "output").unwrap();

        let dest = dir.join("project");
        std::fs::create_dir_all(&dest).unwrap();
        let created = copy_sample(&src, &dest).unwrap();

        assert!(dest.join("src/main.c").is_file());
        assert!(dest.join("README.rst").is_file());
        assert!(dest.join("prj.conf").is_file());
        assert!(!dest.join("build").exists());
        assert_eq!(created.skipped, Vec::<PathBuf>::new());
        assert!(created.written.contains(&PathBuf::from("src/main.c")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_never_overwrites_an_existing_file() {
        let dir = scratch("keep");
        let samples = dir.join("samples");
        write_sample(&samples, "hello_world");
        let src = samples.join("hello_world");

        let dest = dir.join("project");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("prj.conf"), "# mine\n").unwrap();

        let created = copy_sample(&src, &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("prj.conf")).unwrap(),
            "# mine\n"
        );
        assert_eq!(created.skipped, vec![PathBuf::from("prj.conf")]);
        assert!(created.written.contains(&PathBuf::from("CMakeLists.txt")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
