//! What a brand-new project starts with.
//!
//! `SPEC.md` §7: answering the configuration screen is what makes an empty
//! directory usable, so the answer has to leave behind something the backend
//! can actually operate on --- a MicroPython project with the two
//! directories the file browser and the firmware downloader expect, a Zephyr
//! application `west build` accepts without further editing.
//!
//! Each backend declares its own layout ([`crate::backend::Backend::scaffold`]);
//! this module only writes it. That is the same split detection has: the
//! backend knows what its projects look like, the shared code never branches
//! on which backend it is (`AGENTS.md` §3).
//!
//! Writing never overwrites: a file already in the directory is left exactly
//! as it is and reported as skipped, so re-running the prompt on a
//! half-created project completes it instead of resetting it.

use std::io;
use std::path::{Path, PathBuf};

/// One file a new project starts with, at a path relative to the project
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldFile {
    pub path: PathBuf,
    pub contents: String,
}

impl ScaffoldFile {
    pub fn new(path: impl Into<PathBuf>, contents: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            contents: contents.into(),
        }
    }
}

/// A backend's starting layout: directories that must exist even when empty
/// (MicroPython's `firmware/` holds downloads, not sources), plus files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scaffold {
    pub dirs: Vec<PathBuf>,
    pub files: Vec<ScaffoldFile>,
}

impl Scaffold {
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.files.is_empty()
    }
}

/// What [`create`] did, for the log line that follows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Created {
    /// Paths written, relative to the project root.
    pub written: Vec<PathBuf>,
    /// Files that were already there and were left untouched.
    pub skipped: Vec<PathBuf>,
}

/// Writes `scaffold` into `dir`, creating parent directories as needed.
///
/// A relative path that tries to climb out of the project root
/// (`..`, or an absolute path) is refused --- scaffolds are backend-declared
/// data, and this is the one place that turns them into filesystem writes.
pub fn create(dir: &Path, scaffold: &Scaffold) -> io::Result<Created> {
    let mut created = Created::default();
    for relative in &scaffold.dirs {
        let target = resolve(dir, relative)?;
        std::fs::create_dir_all(&target)?;
    }
    for file in &scaffold.files {
        let target = resolve(dir, &file.path)?;
        if target.exists() {
            created.skipped.push(file.path.clone());
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &file.contents)?;
        created.written.push(file.path.clone());
    }
    Ok(created)
}

/// Joins `relative` onto `dir`, refusing anything that would leave the
/// project. `pub(crate)` because every writer into a project directory owes
/// the same guarantee, not just [`create`].
pub(crate) fn resolve(dir: &Path, relative: &Path) -> io::Result<PathBuf> {
    let sane = relative.is_relative()
        && !relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir));
    if !sane {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("scaffold path escapes the project: {}", relative.display()),
        ));
    }
    Ok(dir.join(relative))
}

/// A managed block in a file the project already has.
///
/// [`create`] never overwrites, which is right for a file a project *starts*
/// with. A file the project already carries --- a Kconfig fragment, a
/// `sysbuild.conf` written for another reason --- must instead be
/// *extended*, and only ever inside guarded markers:
///
/// ```text
/// # >>> chiptui:ota --- managed block; edit outside these markers
/// CONFIG_BOOTLOADER_MCUBOOT=y
/// # <<< chiptui:ota
/// ```
///
/// The markers are `#` comments, so the file still builds with the block in
/// it. A format whose comments look different (a devicetree overlay's
/// `/* */`) would need a per-format leader --- add it when such a file
/// actually needs a block, not before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedBlock {
    /// Relative to the project root; [`resolve`]'s guard applies.
    pub path: PathBuf,
    pub tag: &'static str,
    pub body: String,
}

/// What [`apply_block`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The file was not there; it is now exactly the block.
    Created,
    /// The file was there with no such block; the block was appended.
    Appended,
    /// The block was there with different content; it now carries `body`.
    /// Everything outside the markers is byte-for-byte what it was.
    Replaced,
    /// The block was already exactly this; nothing was written, not even a
    /// metadata touch.
    Unchanged,
}

fn begin_marker(tag: &str) -> String {
    format!("# >>> chiptui:{tag} --- managed block; edit outside these markers")
}

fn end_marker(tag: &str) -> String {
    format!("# <<< chiptui:{tag}")
}

/// Whether `line` opens/closes the `tag` block. The character after the tag
/// must be whitespace or the line's end --- `ota` must not match
/// `ota-netshell`'s markers.
fn is_marker(line: &str, prefix: &str) -> bool {
    let line = line.trim_end();
    line.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// The block as written: marker lines around the body, which is stored with
/// exactly one trailing newline so a write-then-read round trip compares
/// equal.
fn render_block(tag: &str, body: &str) -> String {
    let body = body.trim_end_matches('\n');
    let mut out = begin_marker(tag);
    out.push('\n');
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    out.push_str(&end_marker(tag));
    out.push('\n');
    out
}

/// The byte ranges of the `tag` block's marker lines (begin, end), each
/// including its line ending.
///
/// Every corruption is a named `Err`, never a guess: a begin with no end
/// (rewriting to end-of-file would silently eat whatever the user wrote
/// after it), an end with no begin, and two blocks carrying the same tag.
fn find_block(text: &str, tag: &str) -> Result<Option<(usize, usize, usize, usize)>, String> {
    let begin = begin_marker(tag);
    let end = end_marker(tag);
    let mut found: Option<(usize, usize)> = None; // begin line's byte range
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let range = (offset, offset + line.len());
        offset += line.len();
        if is_marker(line, &begin) {
            if found.is_some() {
                return Err(format!(
                    "two begin markers for a '{tag}' block and no end between them"
                ));
            }
            found = Some(range);
        } else if is_marker(line, &end) {
            let Some((begin_start, begin_end)) = found else {
                return Err(format!("a '{tag}' block's end marker with no begin"));
            };
            return match find_second_begin(text, tag, range.1) {
                true => Err(format!(
                    "two '{tag}' blocks --- refusing to guess which to keep"
                )),
                false => Ok(Some((begin_start, begin_end, range.0, range.1))),
            };
        }
    }
    if found.is_some() {
        return Err(format!("a '{tag}' block that is never closed"));
    }
    Ok(None)
}

/// Whether another `tag` block opens after `offset` --- the duplicate case
/// [`find_block`] refuses.
fn find_second_begin(text: &str, tag: &str, offset: usize) -> bool {
    let begin = begin_marker(tag);
    text[offset..]
        .split_inclusive('\n')
        .any(|line| is_marker(line, &begin))
}

/// `text` with the `tag` block set to `body` --- replaced in place if one is
/// there, appended otherwise. Pure; [`apply_block`] is the io half.
///
/// When a block already carries exactly this `body` the text comes back
/// unchanged, which is what makes a re-run byte-for-byte stable. An append
/// adds the smallest separator that keeps the markers on lines of their
/// own: a newline to terminate a last line that misses one, then one blank
/// line if the file does not already end in one.
pub fn upsert_block(text: &str, tag: &str, body: &str) -> Result<String, String> {
    // A body carrying marker lines would make the next read see a corrupt
    // file, so the write refuses it instead of producing one.
    if body.contains("# >>> chiptui:") || body.contains("# <<< chiptui:") {
        return Err("a block's body cannot carry marker lines".to_string());
    }
    match find_block(text, tag)? {
        Some((begin_start, _, _, end_end)) => {
            if block_matches(text, tag, body) {
                return Ok(text.to_string());
            }
            let mut out = String::with_capacity(text.len() + body.len());
            out.push_str(&text[..begin_start]);
            out.push_str(&render_block(tag, body));
            out.push_str(&text[end_end..]);
            Ok(out)
        }
        None => {
            let mut out = String::from(text);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&render_block(tag, body));
            Ok(out)
        }
    }
}

/// Whether `text` carries a block for `tag` at all, whatever its body ---
/// the question "is this already written" as distinct from
/// [`block_matches`]' "is this written *and* current".
pub fn find_tag(text: &str, tag: &str) -> Result<bool, String> {
    Ok(find_block(text, tag)?.is_some())
}

/// Removes the block `tag` owns, leaving everything outside its markers
/// byte-identical --- the exact counterpart of [`upsert_block`], and
/// possible only because the markers say precisely where the region ends.
///
/// A file with no such block is returned unchanged (removal is idempotent,
/// the way the write is); an unclosed or duplicated marker is the same
/// `Err` [`find_block`] gives every other caller, never a guess.
///
/// The separator the append added comes back off with it: a blank line left
/// where the block was would accumulate one per add/remove cycle, and
/// "byte-identical outside the markers" has to survive round trips to mean
/// anything.
pub fn remove_block(text: &str, tag: &str) -> Result<String, String> {
    let Some((begin_start, _, _, end_end)) = find_block(text, tag)? else {
        return Ok(text.to_string());
    };
    let mut head = &text[..begin_start];
    let tail = &text[end_end..];
    // Give back the blank line the append put in front of the block, but
    // only when there is something before it to separate from.
    if tail.is_empty() {
        while head.ends_with("\n\n") {
            head = &head[..head.len() - 1];
        }
    }
    Ok(format!("{head}{tail}"))
}

/// Whether `text` already carries exactly this block --- the `already_done`
/// predicate, read off content and never off a record of a previous run.
/// Compared with trailing newlines normalized, the same form
/// [`render_block`] writes.
pub fn block_matches(text: &str, tag: &str, body: &str) -> bool {
    let Ok(Some((_, begin_end, end_start, _))) = find_block(text, tag) else {
        return false;
    };
    text[begin_end..end_start].trim_end_matches('\n') == body.trim_end_matches('\n')
}

/// Writes `block` into the project at `root`, creating the file (and its
/// parent directories) when absent.
///
/// Writes go through `settings::write_config`, the atomicity guarantee the
/// config writers already share (a temporary file beside the target, then a
/// rename): a Kconfig fragment and a `chiptui.toml` deserve the same
/// never-half-written file.
pub fn apply_block(root: &Path, block: &GuardedBlock) -> io::Result<Applied> {
    let path = resolve(root, &block.path)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let Some(text) = existing else {
        crate::settings::write_config(&path, &render_block(block.tag, &block.body))?;
        return Ok(Applied::Created);
    };
    if block_matches(&text, block.tag, &block.body) {
        return Ok(Applied::Unchanged);
    }
    let had_block = matches!(find_block(&text, block.tag), Ok(Some(_)));
    let updated = upsert_block(&text, block.tag, &block.body)
        .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
    crate::settings::write_config(&path, &updated)?;
    Ok(if had_block {
        Applied::Replaced
    } else {
        Applied::Appended
    })
}

/// A project name reduced to what CMake and shell-free tooling accept:
/// letters, digits, `_` and `-`, with everything else folded to `_`. Empty
/// input (a root directory, a name of only separators) becomes `app`.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-scaffold-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scaffold() -> Scaffold {
        Scaffold {
            dirs: vec![PathBuf::from("firmware")],
            files: vec![
                ScaffoldFile::new("src/main.py", "print('hi')\n"),
                ScaffoldFile::new("README.md", "docs\n"),
            ],
        }
    }

    #[test]
    fn create_writes_files_and_directories() {
        let dir = temp_dir("write");
        let created = create(&dir, &scaffold()).unwrap();

        let main = std::fs::read_to_string(dir.join("src/main.py")).unwrap();
        let firmware = dir.join("firmware").is_dir();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(main, "print('hi')\n");
        assert!(firmware, "an empty declared directory is still created");
        assert_eq!(created.written.len(), 2);
        assert!(created.skipped.is_empty());
    }

    #[test]
    fn create_never_overwrites_an_existing_file() {
        let dir = temp_dir("keep");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.py"), "mine\n").unwrap();

        let created = create(&dir, &scaffold()).unwrap();

        let main = std::fs::read_to_string(dir.join("src/main.py")).unwrap();
        let readme = dir.join("README.md").exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(main, "mine\n", "an existing file survives untouched");
        assert!(readme, "the missing half is still completed");
        assert_eq!(created.skipped, vec![PathBuf::from("src/main.py")]);
        assert_eq!(created.written, vec![PathBuf::from("README.md")]);
    }

    #[test]
    fn a_path_leaving_the_project_is_refused() {
        let dir = temp_dir("escape");
        let escaping = Scaffold {
            dirs: Vec::new(),
            files: vec![ScaffoldFile::new("../outside.txt", "no\n")],
        };
        let result = create(&dir, &escaping);
        let leaked = dir.parent().unwrap().join("outside.txt").exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(!leaked);
    }

    #[test]
    fn safe_name_keeps_what_cmake_accepts() {
        assert_eq!(safe_name("blinky"), "blinky");
        assert_eq!(safe_name("my sensor.app"), "my_sensor_app");
        assert_eq!(safe_name("../.."), "app");
    }

    fn block(body: &str) -> GuardedBlock {
        GuardedBlock {
            path: PathBuf::from("boards/xiao_esp32c3.conf"),
            tag: "ota",
            body: body.to_string(),
        }
    }

    const BODY: &str = "CONFIG_BOOTLOADER_MCUBOOT=y\nCONFIG_MCUMGR=y\n";

    #[test]
    fn upsert_appends_a_block_to_a_file_that_has_none() {
        let text = "CONFIG_WIFI=y\n";
        let out = upsert_block(text, "ota", BODY).unwrap();
        assert_eq!(
            out,
            "CONFIG_WIFI=y\n\n# >>> chiptui:ota --- managed block; edit outside these markers\n\
             CONFIG_BOOTLOADER_MCUBOOT=y\nCONFIG_MCUMGR=y\n# <<< chiptui:ota\n"
        );
    }

    #[test]
    fn upsert_replaces_a_blocks_body_in_place() {
        let once = upsert_block("CONFIG_WIFI=y\n", "ota", "CONFIG_MCUMGR=y\n").unwrap();
        let twice = upsert_block(&once, "ota", BODY).unwrap();
        assert_eq!(
            twice,
            "CONFIG_WIFI=y\n\n# >>> chiptui:ota --- managed block; edit outside these markers\n\
             CONFIG_BOOTLOADER_MCUBOOT=y\nCONFIG_MCUMGR=y\n# <<< chiptui:ota\n",
            "the second run replaces, never duplicates"
        );
    }

    #[test]
    fn a_second_run_with_the_same_body_is_byte_identical() {
        let once = upsert_block("CONFIG_WIFI=y\n", "ota", BODY).unwrap();
        assert_eq!(upsert_block(&once, "ota", BODY).unwrap(), once);
        assert!(block_matches(&once, "ota", BODY));
        assert!(!block_matches(&once, "ota", "CONFIG_MCUMGR=y\n"));
        assert!(!block_matches("CONFIG_WIFI=y\n", "ota", BODY));
    }

    #[test]
    fn text_outside_the_markers_survives_byte_for_byte() {
        // Trailing whitespace, no final newline, a comment that happens to
        // mention the markers' shape in prose: none of it is the writer's
        // business.
        let text = "CONFIG_WIFI=y  \n# CONFIG_DNS_RESOLVER=y, maybe\n# tail";
        let out = upsert_block(text, "ota", BODY).unwrap();
        assert!(out.starts_with("CONFIG_WIFI=y  \n# CONFIG_DNS_RESOLVER=y, maybe\n# tail\n\n"));

        let once = upsert_block(text, "ota", BODY).unwrap();
        let twice = upsert_block(&once, "ota", "CONFIG_MCUMGR=y\n").unwrap();
        assert!(twice.starts_with("CONFIG_WIFI=y  \n# CONFIG_DNS_RESOLVER=y, maybe\n# tail\n\n"));
        assert!(twice.ends_with("# <<< chiptui:ota\n"));
    }

    #[test]
    fn an_unclosed_block_is_an_error_not_a_rewrite_to_eof() {
        let text = "# >>> chiptui:ota --- managed block; edit outside these markers\nCONFIG_X=y\n# user's own tail\n";
        assert!(upsert_block(text, "ota", BODY).is_err());
        assert!(!block_matches(text, "ota", BODY));
    }

    #[test]
    fn two_blocks_with_the_same_tag_are_an_error() {
        let once = upsert_block("", "ota", BODY).unwrap();
        let doubled = format!("{once}\n{once}");
        assert!(upsert_block(&doubled, "ota", BODY).is_err());
        assert!(!block_matches(&doubled, "ota", BODY));
    }

    #[test]
    fn an_end_marker_with_no_begin_is_an_error() {
        assert!(upsert_block("CONFIG_X=y\n# <<< chiptui:ota\n", "ota", BODY).is_err());
    }

    #[test]
    fn a_tags_markers_are_not_another_tags_prefix() {
        // `ota` and `ota-netshell` live in the same file by design; the
        // shorter tag must not match the longer's markers.
        let both = upsert_block("", "ota", BODY).unwrap();
        let both = upsert_block(&both, "ota-netshell", "CONFIG_NET_SHELL=y\n").unwrap();
        assert!(block_matches(&both, "ota", BODY));
        assert!(block_matches(&both, "ota-netshell", "CONFIG_NET_SHELL=y\n"));
        let out = upsert_block(&both, "ota", "CONFIG_MCUMGR=y\n").unwrap();
        assert!(block_matches(&out, "ota", "CONFIG_MCUMGR=y\n"));
        assert!(
            block_matches(&out, "ota-netshell", "CONFIG_NET_SHELL=y\n"),
            "replacing one block leaves the other's bytes alone"
        );
    }

    #[test]
    fn a_body_carrying_markers_is_refused() {
        assert!(upsert_block("", "ota", "CONFIG_X=y\n# <<< chiptui:ota\n").is_err());
    }

    #[test]
    fn apply_block_creates_appends_replaces_and_reports_unchanged() {
        let dir = temp_dir("block");
        let path = dir.join("boards/xiao_esp32c3.conf");

        let applied = apply_block(&dir, &block(BODY)).unwrap();
        assert_eq!(applied, Applied::Created);
        assert!(block_matches(
            &std::fs::read_to_string(&path).unwrap(),
            "ota",
            BODY
        ));

        std::fs::write(&path, "CONFIG_WIFI=y\n").unwrap();
        let applied = apply_block(&dir, &block(BODY)).unwrap();
        assert_eq!(applied, Applied::Appended);

        let applied = apply_block(&dir, &block("CONFIG_MCUMGR=y\n")).unwrap();
        assert_eq!(applied, Applied::Replaced);

        let before = std::fs::read_to_string(&path).unwrap();
        let applied = apply_block(&dir, &block("CONFIG_MCUMGR=y\n")).unwrap();
        assert_eq!(applied, Applied::Unchanged);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removal is the write's exact counterpart: everything outside the
    /// markers survives byte for byte, and an add/remove/add round trip
    /// reproduces the file the first write made --- no blank line
    /// accumulating at the seam.
    #[test]
    fn remove_block_takes_only_its_own_region() {
        let original = "CONFIG_WIFI=y\n# a comment the user wrote\n";
        let added = upsert_block(original, "ota", BODY).unwrap();
        assert!(block_matches(&added, "ota", BODY));

        let removed = remove_block(&added, "ota").unwrap();
        assert_eq!(removed, original, "every byte outside the markers survives");
        assert_eq!(
            upsert_block(&removed, "ota", BODY).unwrap(),
            added,
            "the round trip is stable"
        );

        // Two blocks in one file: removing one leaves the other whole.
        let both = upsert_block(&added, "ota-netshell", "CONFIG_NET_SHELL=y\n").unwrap();
        let one = remove_block(&both, "ota-netshell").unwrap();
        assert_eq!(one, added);

        // Idempotent, and it never invents a file's worth of content.
        assert_eq!(remove_block(original, "ota").unwrap(), original);
        assert_eq!(remove_block("", "ota").unwrap(), "");

        // A block that is the whole file leaves nothing behind.
        let alone = upsert_block("", "ota", BODY).unwrap();
        assert_eq!(remove_block(&alone, "ota").unwrap(), "");

        // And a corrupt region is refused rather than guessed at, the way
        // every other reader of it is.
        let unclosed = concat!(
            "# >>> chiptui:ota --- managed block; edit outside these markers\n",
            "CONFIG_X=y\n"
        );
        assert!(remove_block(unclosed, "ota").is_err());
    }

    #[test]
    fn find_tag_answers_presence_not_currency() {
        let written = upsert_block("", "ota", BODY).unwrap();
        assert!(find_tag(&written, "ota").unwrap());
        assert!(!find_tag(&written, "ota-netshell").unwrap());
        // A stale body is still a block that is *there* --- which is the
        // distinction from `block_matches`, and what lets the net shell
        // toggle know which way it moves.
        let stale = upsert_block(&written, "ota", "CONFIG_OTHER=y\n").unwrap();
        assert!(find_tag(&stale, "ota").unwrap());
        assert!(!block_matches(&stale, "ota", BODY));
    }

    #[test]
    fn apply_block_refuses_a_path_leaving_the_project() {
        let dir = temp_dir("block-escape");
        let escaping = GuardedBlock {
            path: PathBuf::from("../outside.conf"),
            ..block(BODY)
        };
        let result = apply_block(&dir, &escaping);
        let leaked = dir.parent().unwrap().join("outside.conf").exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err());
        assert!(!leaked);
    }
}
