//! The micropython-lib package index, for the Dependencies row's search.
//!
//! `mip` itself has no search/list subcommand (mpremote 1.28's `do_mip`
//! accepts `install` only), but the index it installs from is plain files
//! under `https://micropython.org/pi/v2` (`_PACKAGE_INDEX` in mpremote's own
//! `mip.py`), and its root carries a machine-generated `index.json` listing
//! every package --- ~130 entries, tens of kilobytes, stable shape:
//!
//! ```json
//! { "packages": [ { "path": "micropython/urequests", "license": "MIT",
//!                   "version": "0.8.0", "description": "", "name": "urequests",
//!                   "author": "", "versions": { "6": [...], "py": [...] } } ] }
//! ```
//!
//! Fetched through `curl` like the firmware pages are (`SPEC.md` §9/§22: no
//! bundled HTTP client for this), parsed by a tolerant hand-rolled reader ---
//! same bias as the config parsers: one known shape, no serde dependency.
//! An entry the reader cannot make sense of is skipped, never fatal.

/// Where mip's own default index lives, and where the listing is read from.
pub const INDEX_URL: &str = "https://micropython.org/pi/v2/index.json";

/// One package of the index, reduced to what the search list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Reads the index listing. Tolerant by design: fields may be missing or
/// empty (half the index carries no description, four entries carry no
/// `path`), and only a package with a name becomes a row.
pub fn parse_index(text: &str) -> Vec<Package> {
    let Some(mut cursor) = text
        .find("\"packages\"")
        .and_then(|at| text[at..].find('[').map(|offset| at + offset))
    else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut packages = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut object_start: Option<usize> = None;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        match byte {
            b'\\' if in_string => cursor += 1,
            b'"' => in_string = !in_string,
            b'{' | b'[' if !in_string => {
                if byte == b'{' && depth == 1 && object_start.is_none() {
                    object_start = Some(cursor);
                }
                depth += 1;
            }
            b'}' | b']' if !in_string => {
                if byte == b'}'
                    && depth == 2
                    && let Some(start) = object_start
                {
                    if let Some(package) = read_package(&text[start..=cursor]) {
                        packages.push(package);
                    }
                    object_start = None;
                }
                // The `]` closing the packages array itself: depth 1 -> 0.
                if depth <= 1 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages.dedup_by(|a, b| a.name == b.name);
    packages
}

/// Extracts the three fields the list shows from one object's text.
fn read_package(object: &str) -> Option<Package> {
    let name = string_field(object, "name")?;
    if name.is_empty() {
        return None;
    }
    Some(Package {
        name,
        version: string_field(object, "version").unwrap_or_default(),
        description: string_field(object, "description").unwrap_or_default(),
    })
}

/// The string value of `"key"` inside `text`, unescaping what the index's
/// generator escapes. `None` when the key is absent or its value is not a
/// string --- the caller decides what that means.
fn string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value),
            '\\' => match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some(escape) => value.push(escape),
                None => break,
            },
            _ => value.push(c),
        }
    }
    None
}

/// The entries matching a query: case-insensitive substring over the name
/// and the description, every entry when the query is blank.
pub fn search<'a>(index: &'a [Package], query: &str) -> Vec<&'a Package> {
    let query = query.trim().to_lowercase();
    index
        .iter()
        .filter(|package| {
            query.is_empty()
                || package.name.to_lowercase().contains(&query)
                || package.description.to_lowercase().contains(&query)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slice of the real index's shape: nested `versions` map whose array
    /// elements must not be mistaken for fields, an empty description, a
    /// missing `path`, and a key whose name contains another key (`versions`
    /// contains `version`) --- `string_field` matches the exact quoted key
    /// followed by a colon, so it must not trip on the prefix.
    const SAMPLE: &str = r#"{
  "packages": [
    {
      "path": "micropython/urequests",
      "license": "MIT",
      "version": "0.8.0",
      "description": "HTTP client",
      "name": "urequests",
      "author": "",
      "versions": { "6": ["0.7.0"], "py": ["0.8.0"] }
    },
    {
      "license": "MIT",
      "version": "0.1.3",
      "description": "",
      "name": "collections-deque",
      "author": "",
      "versions": { "6": ["0.1.3"] }
    }
  ]
}"#;

    #[test]
    fn parses_the_index_shape() {
        let packages = parse_index(SAMPLE);
        assert_eq!(
            packages,
            [
                Package {
                    name: "collections-deque".into(),
                    version: "0.1.3".into(),
                    description: String::new(),
                },
                Package {
                    name: "urequests".into(),
                    version: "0.8.0".into(),
                    description: "HTTP client".into(),
                },
            ],
            "sorted by name, the versions map read as neither field nor entry"
        );
    }

    #[test]
    fn a_document_without_a_packages_array_is_empty_not_fatal() {
        assert!(parse_index("Not Found").is_empty());
        assert!(parse_index("{\"packages\": null}").is_empty());
        assert!(parse_index(r#"{"packages": [{"license": "MIT"}]}"#).is_empty());
    }

    #[test]
    fn escaped_values_are_unescaped() {
        let text = r#"{"packages": [{"name": "pkg", "version": "1.0", "description": "a \"quoted\" desc"}]}"#;
        let packages = parse_index(text);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].description, "a \"quoted\" desc");
    }

    #[test]
    fn search_matches_name_and_description_case_insensitively() {
        let index = parse_index(SAMPLE);
        assert_eq!(search(&index, "UREQ").len(), 1);
        assert_eq!(search(&index, "http").len(), 1);
        assert_eq!(search(&index, "").len(), 2);
        assert!(search(&index, "nope").is_empty());
    }
}
