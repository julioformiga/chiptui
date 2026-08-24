//! `requirements.txt` interpretation for the Project pane's Dependencies
//! row.
//!
//! mpremote 1.28's `mip install` takes package *specifications* as positional
//! arguments (`name`, `name@version`, `github:org/repo`, URLs) --- it has no
//! `-r` flag and does not read manifests, so "install from
//! requirements.txt" means parsing the file on the host and passing every
//! line's specification through (`mpremote/mip.py`, verified against the
//! installed 1.28.0). Lines whose pin mpremote cannot express (`pkg>=1.0`)
//! degrade to the bare name rather than being dropped silently.

/// What a freshly created `requirements.txt` starts from: a comment header
/// that documents the one-per-line grammar, so the file explains itself
/// before it has any content. Shared by the scaffold and the Dependencies
/// row's create-on-`Enter`.
pub const REQUIREMENTS_TEMPLATE: &str = "\
# MicroPython package requirements, installed to the board's /lib by mip.
# One package per line: urequests, umqtt.simple, name@version, github:org/repo.
";

/// The package name a specification refers to, for coverage counting.
///
/// URL/git specifications (`github:org/repo`, `https://…`) name no `/lib`
/// entry the line itself can predict --- mip derives it from the remote
/// manifest --- so they count as `None` and stay out of the fraction.
pub fn spec_name(spec: &str) -> Option<&str> {
    if spec.contains(':') || spec.contains('/') {
        return None;
    }
    let name = spec
        .split(['@', '=', '<', '>', '!', ';'])
        .next()
        .unwrap_or("");
    (!name.is_empty()).then_some(name)
}

/// Parses a `requirements.txt` into the specifications `mip install` accepts.
///
/// pip grammar the lines may carry and what happens to it:
///
/// - `#` starts a comment; blank lines are skipped.
/// - Leading `-` options (`--index-url`, `-e …`) are pip directives mip has
///   no equivalent for, and are skipped.
/// - `pkg` passes through; `pkg@1.2.3` passes through (mip's own pin
///   syntax); `pkg==1.2.3` becomes `pkg@1.2.3` (an exact pin mip can carry).
/// - `pkg>=1.2.3` / `pkg~=…` / `pkg!=…` degrade to `pkg` --- mip has no
///   range syntax, and the newest version is a better guess than refusing.
/// - Anything with a scheme or a slash (`github:org/repo`,
///   `https://…/pkg.py`) passes through verbatim.
pub fn parse_requirements(text: &str) -> Vec<String> {
    text.lines()
        .map(strip_comment)
        .filter_map(|line| spec_for_line(line.trim()))
        .collect()
}

fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or("")
}

fn spec_for_line(line: &str) -> Option<String> {
    if line.is_empty() || line.starts_with('-') {
        return None;
    }
    let head = line.split_whitespace().next()?;
    if head.contains(':') || head.contains('/') {
        return Some(head.to_string());
    }
    // The specifier may continue past the head token (`pkg == 1.0` is legal
    // pip), so the rest of the line rides along.
    let tail = line.strip_prefix(head).unwrap_or("").trim_start();
    let Some(split) = head.find(|c: char| "@=<>!;~".contains(c)) else {
        return exact_pin("", tail)
            .map(|version| format!("{head}@{version}"))
            .or_else(|| Some(head.to_string()));
    };
    let (name, rest) = head.split_at(split);
    if name.is_empty() {
        return None;
    }
    // `@version` keeps a clean tag; `==version` is rewritable into one;
    // ranges cannot be expressed at all.
    let version = rest
        .strip_prefix('@')
        .map(str::to_string)
        .or_else(|| exact_pin(rest, tail));
    match version {
        Some(version) if !version.is_empty() && !version.contains(['=', '<', '>', '!']) => {
            Some(format!("{name}@{version}"))
        }
        _ => Some(name.to_string()),
    }
}

/// The version of an `==` pin, whether it rode the same token
/// (`pkg==1.2.3`) or the next one (`pkg == 1.2.3`).
fn exact_pin(rest: &str, tail: &str) -> Option<String> {
    let pin = rest
        .strip_prefix("==")
        .or_else(|| tail.strip_prefix("=="))?
        .trim_start();
    Some(pin.split([';', ' ', '\t']).next().unwrap_or("").to_string())
}

use crate::backend::micropython::parse::RemoteEntry;

/// The device directory `mip` installs into.
pub const LIB_ROOT: &str = "/lib";

/// Where a package name lands under `/lib`, in mip's own convention.
///
/// mip's package manifest lists *file targets*, and a dotted name is a
/// path: `umqtt.simple` ships `umqtt/simple.mpy`, not a file called
/// `umqtt.simple` (verified against the installed `mpremote/mip.py` 1.28,
/// whose `_install_json` joins `target + "/" + target_path`). Matching the
/// dotted name flat --- what [`coverage`] used to do --- reported the
/// template's own example as missing forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibTarget {
    /// The directory the candidates live in: `/lib` for a plain name,
    /// `/lib/umqtt` for `umqtt.simple`.
    pub dir: crate::device::DevicePath,
    /// The entry names that would satisfy the package, in the order mip
    /// may have written them: a package directory, then the two
    /// single-module spellings.
    pub candidates: Vec<String>,
}

pub fn lib_target(name: &str) -> LibTarget {
    let mut segments: Vec<&str> = name.split('.').filter(|part| !part.is_empty()).collect();
    let leaf = segments.pop().unwrap_or(name);
    let mut dir = crate::device::DevicePath::new(LIB_ROOT);
    for segment in segments {
        dir = dir.join(segment);
    }
    LibTarget {
        dir,
        candidates: vec![
            leaf.to_string(),
            format!("{leaf}.py"),
            format!("{leaf}.mpy"),
        ],
    }
}

/// How the coverage walk reaches the listing cache: one directory at a
/// time, answering `None` for a directory nothing has listed yet. A
/// closure rather than a slice because a dotted name needs its *package*
/// directory too, which is a second cache entry.
pub type LibLookup<'a> = &'a dyn Fn(&crate::device::DevicePath) -> Option<Vec<RemoteEntry>>;

/// Whether a package is on the board --- with a third answer, because the
/// listing cache holds one directory level at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    Yes,
    No,
    /// The directory the package would live in exists but has not been
    /// listed yet. This is what bounds the extra listings: a `/lib` that
    /// *was* listed and holds no `umqtt/` answers `No` outright, so only a
    /// prefix that really exists on the board ever costs a request.
    Unknown,
}

/// Resolves one package name against the listing cache.
///
/// `lookup` answers `None` for a directory that has not been listed. The
/// entry's kind is consulted, not just its name: a stray *file* called
/// `urequests` is not the package directory, and a *directory* called
/// `urequests.py` is not the module.
pub fn installed(name: &str, lookup: LibLookup<'_>) -> Installed {
    let target = lib_target(name);
    let Some(entries) = lookup(&target.dir) else {
        // The parent has not been listed. Whether it is even worth listing
        // depends on its own parent, which `pending_listing` answers.
        return Installed::Unknown;
    };
    let found = entries.iter().any(|entry| {
        if entry.name == target.candidates[0] {
            entry.is_dir
        } else {
            !entry.is_dir && target.candidates[1..].contains(&entry.name)
        }
    });
    if found { Installed::Yes } else { Installed::No }
}

/// The directory a package's answer is still waiting on, if listing it
/// would settle the question --- `None` when the answer is already known,
/// or when the parent listing proves the directory is not there.
pub fn pending_listing(name: &str, lookup: LibLookup<'_>) -> Option<crate::device::DevicePath> {
    let target = lib_target(name);
    if lookup(&target.dir).is_some() {
        return None;
    }
    // Walk up to the nearest listed ancestor: the directory is worth
    // listing only if that ancestor says it exists.
    let parent = target.dir.parent()?;
    let entries = lookup(&parent)?;
    let exists = entries
        .iter()
        .any(|entry| entry.is_dir && entry.name == target.dir.name());
    exists.then_some(target.dir)
}

/// How much of the requirements file the device's `/lib` already covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coverage {
    pub total: usize,
    pub installed: usize,
    /// Declared packages whose directory has not been listed yet --- held
    /// out of `installed` so the fraction never claims what it has not
    /// checked.
    pub unknown: usize,
}

impl Coverage {
    pub const fn is_complete(self) -> bool {
        self.installed >= self.total
    }

    /// Fraction text for the row: `1/2 in /lib`.
    pub fn text(self) -> String {
        format!("{}/{} in /lib", self.installed, self.total)
    }
}

pub fn coverage(specs: &[String], lookup: LibLookup<'_>) -> Coverage {
    let mut coverage = Coverage::default();
    for spec in specs {
        let Some(name) = spec_name(spec) else {
            continue;
        };
        coverage.total += 1;
        match installed(name, lookup) {
            Installed::Yes => coverage.installed += 1,
            Installed::Unknown => coverage.unknown += 1,
            Installed::No => {}
        }
    }
    coverage
}

// ---- editing the file ----------------------------------------------------
//
// Line-level, in the same spirit as `settings.rs`'s config merge: only the
// one line is touched, and every comment, blank line and ordering the user
// put there survives. Both return `None` when nothing would change, so the
// caller can skip the write (and the log line) entirely.

/// Appends `spec`, or replaces the line already declaring the same package.
///
/// De-duplication is by [`spec_name`], so `pkg`, `pkg@1.2.3` and
/// `pkg==1.2.3` are the same declaration --- re-picking a package with a
/// different version *updates* its line rather than adding a second one.
/// A spec with no name of its own (a `github:`/URL line) matches on its
/// whole text.
pub fn add_line(text: &str, spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let key = line_key(spec);
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        match declared_key(line) {
            Some(found) if found == key => {
                if line.trim() == spec {
                    return None;
                }
                out.push(spec.to_string());
                replaced = true;
            }
            _ => out.push(line.to_string()),
        }
    }
    if !replaced {
        if out.is_empty() {
            out.extend(REQUIREMENTS_TEMPLATE.lines().map(str::to_string));
        }
        out.push(spec.to_string());
    }
    Some(joined(out))
}

/// Drops the line declaring `target` --- a package name or a whole
/// `github:`/URL spec. Comments that merely *mention* the name are never
/// touched: only a declaration line is matched.
pub fn remove_line(text: &str, target: &str) -> Option<String> {
    let key = line_key(target.trim());
    let mut removed = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        match declared_key(line) {
            Some(found) if found == key => removed = true,
            _ => out.push(line.to_string()),
        }
    }
    removed.then(|| joined(out))
}

/// What a line declares, for matching: the package name when it has one,
/// the whole specification otherwise. `None` for anything that is not a
/// declaration (comments, blanks, pip options).
fn declared_key(line: &str) -> Option<String> {
    let spec = spec_for_line(strip_comment(line).trim())?;
    Some(line_key(&spec))
}

fn line_key(spec: &str) -> String {
    spec_name(spec).unwrap_or(spec).to_string()
}

fn joined(lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            size: 0,
            is_dir,
        }
    }

    #[test]
    fn plain_names_pass_through() {
        let specs = parse_requirements("# a comment\nurequests\n\nmachine\n");
        assert_eq!(specs, ["urequests", "machine"]);
    }

    #[test]
    fn exact_pins_become_mip_syntax() {
        let specs = parse_requirements("pkg==1.2.3\nother == 2.0.0\n");
        assert_eq!(specs, ["pkg@1.2.3", "other@2.0.0"]);
    }

    #[test]
    fn mip_pins_survive_and_ranges_degrade() {
        let specs = parse_requirements("pkg@1.2.3\npkg2>=1.0\npkg3~=2\npkg4!=3\npkg@>=1.0\n");
        assert_eq!(specs, ["pkg@1.2.3", "pkg2", "pkg3", "pkg4", "pkg"]);
    }

    #[test]
    fn urls_and_git_specs_pass_verbatim_and_pip_options_are_skipped() {
        let specs = parse_requirements(
            "--index-url https://example.com\n-e .\ngithub:org/repo\ngithub:org/repo@v2\nhttps://example.com/pkg.py\n",
        );
        assert_eq!(
            specs,
            [
                "github:org/repo",
                "github:org/repo@v2",
                "https://example.com/pkg.py"
            ]
        );
    }

    /// A cache seeded with `(path, entries)` pairs, in the shape the
    /// browser's own listing cache has.
    fn cache<'a>(
        listed: &'a [(&'a str, Vec<RemoteEntry>)],
    ) -> impl Fn(&crate::device::DevicePath) -> Option<Vec<RemoteEntry>> + 'a {
        move |path| {
            listed
                .iter()
                .find(|(name, _)| crate::device::DevicePath::new(name) == *path)
                .map(|(_, entries)| entries.clone())
        }
    }

    #[test]
    fn a_plain_name_lands_beside_lib_and_a_dotted_one_lands_under_it() {
        let flat = lib_target("urequests");
        assert_eq!(flat.dir, crate::device::DevicePath::new("/lib"));
        assert_eq!(
            flat.candidates,
            ["urequests", "urequests.py", "urequests.mpy"]
        );

        let dotted = lib_target("umqtt.simple");
        assert_eq!(
            dotted.dir,
            crate::device::DevicePath::new("/lib/umqtt"),
            "a dotted name is a path, not a filename"
        );
        assert_eq!(dotted.candidates, ["simple", "simple.py", "simple.mpy"]);

        let deep = lib_target("a.b.c");
        assert_eq!(deep.dir, crate::device::DevicePath::new("/lib/a/b"));
        assert_eq!(deep.candidates[0], "c");
    }

    #[test]
    fn a_dotted_package_installed_by_mip_reads_as_installed() {
        // The regression this mapping exists for: `umqtt.simple` is the
        // template's own example, and mip writes it to `/lib/umqtt/simple.mpy`.
        // Matched flat, it read as missing forever and pinned the row at ⚠.
        let listed = [
            ("/lib", vec![entry("umqtt", true)]),
            ("/lib/umqtt", vec![entry("simple.mpy", false)]),
        ];
        assert_eq!(installed("umqtt.simple", &cache(&listed)), Installed::Yes);
    }

    #[test]
    fn the_entry_kind_is_consulted_not_just_the_name() {
        // A *file* called `urequests` is not the package directory, and a
        // *directory* called `urequests.py` is not the module.
        let as_file = [("/lib", vec![entry("urequests", false)])];
        assert_eq!(installed("urequests", &cache(&as_file)), Installed::No);
        let as_dir = [("/lib", vec![entry("urequests.py", true)])];
        assert_eq!(installed("urequests", &cache(&as_dir)), Installed::No);

        let right = [("/lib", vec![entry("urequests", true)])];
        assert_eq!(installed("urequests", &cache(&right)), Installed::Yes);
        let module = [("/lib", vec![entry("urequests.py", false)])];
        assert_eq!(installed("urequests", &cache(&module)), Installed::Yes);
    }

    #[test]
    fn an_unlisted_directory_is_unknown_and_a_listed_parent_settles_it() {
        // Nothing listed at all: the answer waits.
        assert_eq!(installed("urequests", &cache(&[])), Installed::Unknown);

        // `/lib` listed and holding no `umqtt/`: a definite No, and no
        // request --- this is what bounds the extra listings.
        let empty = [("/lib", Vec::new())];
        assert_eq!(
            installed("umqtt.simple", &cache(&empty)),
            Installed::Unknown
        );
        assert_eq!(pending_listing("umqtt.simple", &cache(&empty)), None);

        // `/lib` listed and holding `umqtt/`: worth one listing.
        let present = [("/lib", vec![entry("umqtt", true)])];
        assert_eq!(
            pending_listing("umqtt.simple", &cache(&present)),
            Some(crate::device::DevicePath::new("/lib/umqtt"))
        );
        // Once it is listed, nothing is pending.
        let both = [
            ("/lib", vec![entry("umqtt", true)]),
            ("/lib/umqtt", vec![entry("simple.mpy", false)]),
        ];
        assert_eq!(pending_listing("umqtt.simple", &cache(&both)), None);
    }

    #[test]
    fn coverage_counts_packages_directories_and_modules() {
        let specs: Vec<String> = ["pkg-a", "pkg-b", "pkg-c", "github:org/repo", "pkg-d"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let listed = [(
            "/lib",
            vec![
                entry("pkg-a", true),
                entry("pkg-b.py", false),
                entry("pkg-c.mpy", false),
            ],
        )];
        let coverage = coverage(&specs, &cache(&listed));
        assert_eq!(
            (coverage.total, coverage.installed, coverage.unknown),
            (4, 3, 0),
            "the git spec counts as neither installed nor missing"
        );
        assert_eq!(coverage.text(), "3/4 in /lib");
    }

    #[test]
    fn a_name_shared_with_an_unrelated_suffix_is_not_installed() {
        let specs = vec!["pkg".to_string()];
        let listed = [("/lib", vec![entry("pkgx.py", false)])];
        let coverage = coverage(&specs, &cache(&listed));
        assert_eq!(coverage.total, 1);
        assert_eq!(coverage.installed, 0);
    }

    #[test]
    fn an_unchecked_package_is_held_out_of_the_installed_count() {
        let specs = vec!["umqtt.simple".to_string()];
        let listed = [("/lib", vec![entry("umqtt", true)])];
        let coverage = coverage(&specs, &cache(&listed));
        assert_eq!(
            (coverage.total, coverage.installed, coverage.unknown),
            (1, 0, 1),
            "the fraction never claims what it has not checked"
        );
    }

    // ---- the line editor -------------------------------------------------

    #[test]
    fn adding_appends_and_seeds_an_empty_file_from_the_template() {
        let seeded = add_line("", "urequests").unwrap();
        assert!(seeded.starts_with("# MicroPython package requirements"));
        assert!(seeded.ends_with("urequests\n"));

        let appended = add_line("# a header\nurequests\n", "umqtt.simple").unwrap();
        assert_eq!(appended, "# a header\nurequests\numqtt.simple\n");

        // A file the user left without a trailing newline still gets one.
        let ragged = add_line("urequests", "machine").unwrap();
        assert_eq!(ragged, "urequests\nmachine\n");
    }

    #[test]
    fn adding_a_package_that_is_already_declared_changes_nothing_or_updates_it() {
        assert_eq!(
            add_line("urequests\n", "urequests"),
            None,
            "the same declaration twice is not a change"
        );
        assert_eq!(
            add_line("# keep me\nurequests\nmachine\n", "urequests@1.2.3").as_deref(),
            Some("# keep me\nurequests@1.2.3\nmachine\n"),
            "a different version updates the line in place rather than duplicating it"
        );
        assert_eq!(
            add_line("urequests==0.8.0\n", "urequests@0.9.0").as_deref(),
            Some("urequests@0.9.0\n"),
            "pip and mip spellings of one package are one declaration"
        );
    }

    #[test]
    fn removing_drops_only_the_declaration_line() {
        let text = "# urequests is nice\nurequests  # the http client\n\nmachine\n";
        assert_eq!(
            remove_line(text, "urequests").as_deref(),
            Some("# urequests is nice\n\nmachine\n"),
            "a comment that merely mentions the name survives"
        );
        assert_eq!(
            remove_line(text, "absent"),
            None,
            "no match is not a rewrite"
        );
        assert_eq!(
            remove_line("pkg==1.2.3\nother\n", "pkg").as_deref(),
            Some("other\n"),
            "a pinned line is matched by its name"
        );
        assert_eq!(
            remove_line("github:org/repo\nother\n", "github:org/repo").as_deref(),
            Some("other\n"),
            "a spec with no name of its own matches on its whole text"
        );
    }
}
