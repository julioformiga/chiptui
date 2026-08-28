//! `<build>/zephyr/zephyr.dts` --- the devicetree as the build resolved it.
//!
//! `dashboard.py` builds its devicetree browser from `edt.pickle`, a pickled
//! Python object no Rust reader can open, and uses this file only for the
//! syntax-highlighted verbatim view. Reading the `.dts` *as the tree* is
//! therefore a substitution, not the same source --- and it costs one thing
//! (a binding's `description` text, which lives only in the pickle) while
//! gaining another: this file records, per node and per property, the source
//! file and line the value came from, which the pickle's browser shows only
//! for nodes.
//!
//! The format is not hand-written DTS. It is `dtlib.Node.__str__`'s output,
//! and it is regular by construction:
//!
//! ```text
//! /* node '/cpus/cpu@0' defined in zephyr/dts/riscv/.../esp32c3_common.dtsi:34 */
//! cpu0: cpu@0 {
//!         device_type = "cpu";                    /* in ….dtsi:35 */
//!         compatible = "espressif,riscv",
//!                      "riscv";                   /* in ….dtsi:36 */
//! };
//! ```
//!
//! Three properties of that shape are load-bearing:
//!
//! * **Every node is preceded by its annotation comment**, carrying the
//!   node's *full path* --- on the real project, 76 annotations for 76
//!   opening braces. So the path never has to be reconstructed, and a
//!   mismatch would show up as a node with a derived path rather than as
//!   silence.
//! * **A property can span several lines** (`riscv,isa-extensions` spans
//!   five), with the `/* in file:line */` comment only on the last. Lines
//!   are accumulated until one whose payload ends in `;`.
//! * **Nesting is one tab per level**, but structure is read from the
//!   braces rather than the indentation, so a reformatted file still parses.

/// Where a node or a property came from, as the file's own comments record
/// it. Both halves are optional: a comment can be absent entirely, and a
/// path on a strange filesystem may not end in `:<line>`.
///
/// [`DtNode`] and [`DtProp`] flatten it into their own `file`/`line` fields
/// --- a consumer wants `node.line`, not `node.source.line` --- so this type
/// exists to carry the pair between the readers below.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SourceRef {
    file: Option<String>,
    line: Option<u64>,
}

/// One property of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtProp {
    pub name: String,
    /// The value as written, with a multi-line property joined into one
    /// line. Empty for a boolean property, which is written as a bare name.
    pub value: String,
    /// Where the value came from, as the trailing `/* in file:line */`
    /// comment records it. Paths are relative to the workspace top, which is
    /// how `dtlib` writes them.
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl DtProp {
    /// Whether this is a bare flag (`interrupt-controller;`) rather than an
    /// assignment.
    pub fn is_flag(&self) -> bool {
        self.value.is_empty()
    }
}

/// One node, flattened out of the tree.
///
/// The nodes are a `Vec` in pre-order with an explicit `depth` rather than a
/// nested structure: the pane renders a list, and a flat vector is what both
/// the row walk and the expansion set index into. It is also what keeps the
/// type free of indirection --- there is no `Box<DtNode>` anywhere here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtNode {
    /// The full path from the annotation comment, e.g. `/soc/i2c@60013000`.
    pub path: String,
    /// The node's own name, e.g. `i2c@60013000`. `/` for the root.
    pub name: String,
    /// The labels written before the name, in source order. A node may carry
    /// several (`label1: label2: name {`).
    pub labels: Vec<String>,
    /// How deep the node sits; the root is 0.
    pub depth: usize,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub props: Vec<DtProp>,
    /// Whether any node below this one exists --- what the tree marker needs
    /// without walking the rest of the vector.
    pub has_children: bool,
}

impl DtNode {
    /// The row label: the labels and the name as the source writes them.
    pub fn label(&self) -> String {
        if self.labels.is_empty() {
            return self.name.clone();
        }
        format!("{}: {}", self.labels.join(": "), self.name)
    }

    /// The node's `status` property, when it declares one. A node that does
    /// not is enabled by default, so `None` and `Some("okay")` mean the same
    /// thing and only a *different* value is worth marking in the list.
    pub fn status(&self) -> Option<&str> {
        self.props
            .iter()
            .find(|prop| prop.name == "status")
            .map(|prop| prop.value.trim_matches('"'))
    }

    /// Whether the node is disabled --- the one status worth a mark.
    pub fn disabled(&self) -> bool {
        self.status().is_some_and(|status| status != "okay")
    }
}

/// Reads `zephyr.dts` into a pre-order list of nodes.
///
/// Returns an empty vector for anything that is not a devicetree --- a
/// missing or truncated file --- which the tab reports as a named state.
pub fn parse(text: &str) -> Vec<DtNode> {
    let mut nodes: Vec<DtNode> = Vec::new();
    // Indices into `nodes` of the currently open ancestors.
    let mut open: Vec<usize> = Vec::new();
    // The annotation comment seen but not yet consumed by a node opening.
    let mut pending: Option<(String, SourceRef)> = None;
    // A property being accumulated across continuation lines.
    let mut partial: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(annotation) = node_annotation(trimmed) {
            pending = Some(annotation);
            continue;
        }

        // A node closes.
        if trimmed == "};" || trimmed == "}" {
            open.pop();
            partial = None;
            continue;
        }

        // A node opens: the line ends with `{`, after the labels and name.
        // A property line always ends with `;` or a comment, never `{`, so
        // this cannot be confused with one.
        if let Some(head) = trimmed.strip_suffix('{') {
            let (labels, name) = split_labels(head.trim());
            let (path, source) = match pending.take() {
                Some((path, source)) => (path, source),
                // No annotation: derive the path from the open ancestors so
                // the node still has an identity.
                None => (derived_path(&nodes, &open, &name), SourceRef::default()),
            };
            if let Some(&parent) = open.last() {
                nodes[parent].has_children = true;
            }
            open.push(nodes.len());
            nodes.push(DtNode {
                path,
                name,
                labels,
                depth: open.len() - 1,
                file: source.file,
                line: source.line,
                props: Vec::new(),
                has_children: false,
            });
            continue;
        }

        // Anything else inside a node is a property, possibly continued
        // from the line before.
        let Some(&current) = open.last() else {
            continue;
        };
        let accumulated = match partial.take() {
            Some(started) => format!("{started} {trimmed}"),
            None => trimmed.to_string(),
        };
        let (payload, source) = split_source_comment(&accumulated);
        if !payload.trim_end().ends_with(';') {
            // Still open: keep accumulating, comment and all --- the comment
            // only ever rides the final line.
            partial = Some(accumulated);
            continue;
        }
        if let Some(prop) = read_prop(payload.trim(), source) {
            nodes[current].props.push(prop);
        }
    }
    nodes
}

/// The `/* node '<path>' defined in <file>:<line> */` line, if this is one.
fn node_annotation(trimmed: &str) -> Option<(String, SourceRef)> {
    let rest = trimmed.strip_prefix("/* node '")?;
    let (path, rest) = rest.split_once('\'')?;
    let rest = rest.trim().strip_prefix("defined in ")?;
    let rest = rest.trim().strip_suffix("*/")?;
    Some((path.to_string(), split_file_line(rest.trim())))
}

/// Splits `labels: name` into its parts. `dtlib` writes every label with its
/// own colon, so `a: b: node` carries two.
fn split_labels(head: &str) -> (Vec<String>, String) {
    let mut parts: Vec<&str> = head.split(':').map(str::trim).collect();
    let name = parts.pop().unwrap_or_default().to_string();
    let labels = parts
        .into_iter()
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect();
    (labels, name)
}

/// A path for a node whose annotation is missing: the open ancestors' path
/// plus this node's name.
fn derived_path(nodes: &[DtNode], open: &[usize], name: &str) -> String {
    match open.last() {
        None => name.to_string(),
        Some(&parent) => {
            let base = nodes[parent].path.trim_end_matches('/');
            format!("{base}/{name}")
        }
    }
}

/// Separates a line's payload from its trailing `/* in file:line */`
/// comment. Returns the payload and the parsed source, if any.
fn split_source_comment(line: &str) -> (&str, Option<SourceRef>) {
    let Some(at) = line.rfind("/* in ") else {
        return (line, None);
    };
    let Some(comment) = line[at..].strip_prefix("/* in ") else {
        return (line, None);
    };
    let Some(comment) = comment.trim_end().strip_suffix("*/") else {
        return (line, None);
    };
    (&line[..at], Some(split_file_line(comment.trim())))
}

/// `path/to/file.dtsi:34` --- the file and the line. The path may itself
/// contain colons on a strange filesystem, so the split is from the right
/// and only counts when the tail is a number.
fn split_file_line(text: &str) -> SourceRef {
    match text.rsplit_once(':') {
        Some((file, line)) => match line.trim().parse::<u64>() {
            Ok(line) => SourceRef {
                file: Some(file.trim().to_string()),
                line: Some(line),
            },
            Err(_) => SourceRef {
                file: Some(text.to_string()),
                line: None,
            },
        },
        None if text.is_empty() => SourceRef::default(),
        None => SourceRef {
            file: Some(text.to_string()),
            line: None,
        },
    }
}

/// One complete property statement, its trailing `;` already present.
fn read_prop(payload: &str, source: Option<SourceRef>) -> Option<DtProp> {
    let statement = payload.trim().strip_suffix(';')?.trim();
    if statement.is_empty() {
        return None;
    }
    let SourceRef { file, line } = source.unwrap_or_default();
    let (name, value) = match statement.split_once('=') {
        Some((name, value)) => (name.trim(), collapse_spaces(value.trim())),
        // A bare name is a boolean property.
        None => (statement, String::new()),
    };
    Some(DtProp {
        name: name.to_string(),
        value,
        file,
        line,
    })
}

/// Squeezes the runs of spaces a joined multi-line value carries, so
/// `"i",           "m",   "c"` reads as `"i", "m", "c"`.
fn collapse_spaces(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut from a real `zephyr.dts`. It carries the shapes that matter: the
    /// root with no annotation of its own kind, a labelled node, a nested
    /// node three deep, a property spanning five lines whose source comment
    /// rides only the last, a boolean property with no `=`, and a disabled
    /// node.
    const DTS: &str = "\
/dts-v1/;

/* node '/' defined in zephyr/dts/riscv/espressif/esp32c3/esp32c3_common.dtsi:15 */
/ {
\t#address-cells = < 0x1 >;          /* in zephyr/dts/riscv/esp32c3_common.dtsi:16 */
\tmodel = \"Seeed XIAO ESP32C3\";      /* in zephyr/boards/seeed/xiao_esp32c3.dts:15 */

\t/* node '/cpus' defined in zephyr/dts/riscv/esp32c3_common.dtsi:30 */
\tcpus {
\t\t#size-cells = < 0x0 >; /* in zephyr/dts/riscv/esp32c3_common.dtsi:32 */

\t\t/* node '/cpus/cpu@0' defined in zephyr/dts/riscv/esp32c3_common.dtsi:34 */
\t\tcpu0: cpu@0 {
\t\t\tdevice_type = \"cpu\";                             /* in zephyr/dts/riscv/esp32c3_common.dtsi:35 */
\t\t\triscv,isa-extensions = \"i\",
\t\t\t                       \"m\",
\t\t\t                       \"c\",
\t\t\t                       \"zicsr\",
\t\t\t                       \"zifencei\";               /* in zephyr/dts/riscv/esp32c3_common.dtsi:38 */
\t\t};
\t};

\t/* node '/soc/i2c@60013000' defined in zephyr/dts/riscv/esp32c3_common.dtsi:99 */
\ti2c0: alt0: i2c@60013000 {
\t\tinterrupt-controller;                            /* in zephyr/dts/riscv/esp32c3_common.dtsi:100 */
\t\tstatus = \"disabled\";                             /* in zephyr/dts/riscv/esp32c3_common.dtsi:101 */
\t};
};
";

    fn node<'a>(nodes: &'a [DtNode], path: &str) -> &'a DtNode {
        nodes
            .iter()
            .find(|node| node.path == path)
            .unwrap_or_else(|| panic!("{path} is missing"))
    }

    #[test]
    fn every_node_is_read_in_pre_order_with_its_depth() {
        let nodes = parse(DTS);
        let seen: Vec<(&str, usize)> = nodes
            .iter()
            .map(|node| (node.path.as_str(), node.depth))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("/", 0),
                ("/cpus", 1),
                ("/cpus/cpu@0", 2),
                ("/soc/i2c@60013000", 1),
            ]
        );
    }

    /// The path comes from the annotation comment, not from the nesting ---
    /// which is what lets `/soc/i2c@60013000` be right even though the
    /// fixture (like the real file) does not nest it under a `/soc` node.
    #[test]
    fn the_path_comes_from_the_annotation_not_the_nesting() {
        let nodes = parse(DTS);
        let i2c = node(&nodes, "/soc/i2c@60013000");
        assert_eq!(i2c.depth, 1, "nesting and path are separate facts");
        assert_eq!(i2c.name, "i2c@60013000");
        assert_eq!(
            i2c.file.as_deref(),
            Some("zephyr/dts/riscv/esp32c3_common.dtsi")
        );
        assert_eq!(i2c.line, Some(99));
    }

    #[test]
    fn labels_are_kept_in_source_order() {
        let nodes = parse(DTS);
        assert_eq!(node(&nodes, "/cpus/cpu@0").labels, vec!["cpu0".to_string()]);
        let i2c = node(&nodes, "/soc/i2c@60013000");
        assert_eq!(i2c.labels, vec!["i2c0".to_string(), "alt0".to_string()]);
        assert_eq!(i2c.label(), "i2c0: alt0: i2c@60013000");
        assert_eq!(node(&nodes, "/cpus").label(), "cpus");
    }

    /// A five-line property is one property, and its source comment --- on
    /// the last line only --- belongs to the whole of it.
    #[test]
    fn a_multi_line_property_is_joined_into_one() {
        let nodes = parse(DTS);
        let cpu = node(&nodes, "/cpus/cpu@0");
        assert_eq!(cpu.props.len(), 2, "not five, and not one giant blob");
        let isa = &cpu.props[1];
        assert_eq!(isa.name, "riscv,isa-extensions");
        assert_eq!(isa.value, "\"i\", \"m\", \"c\", \"zicsr\", \"zifencei\"");
        assert_eq!(isa.line, Some(38));
        assert_eq!(
            isa.file.as_deref(),
            Some("zephyr/dts/riscv/esp32c3_common.dtsi")
        );
    }

    #[test]
    fn properties_carry_their_own_source_line() {
        let nodes = parse(DTS);
        let root = node(&nodes, "/");
        assert_eq!(root.props.len(), 2);
        assert_eq!(root.props[0].name, "#address-cells");
        assert_eq!(root.props[0].value, "< 0x1 >");
        assert_eq!(root.props[0].line, Some(16));
        assert_eq!(root.props[1].name, "model");
        assert_eq!(root.props[1].value, "\"Seeed XIAO ESP32C3\"");
        assert_eq!(
            root.props[1].file.as_deref(),
            Some("zephyr/boards/seeed/xiao_esp32c3.dts")
        );
    }

    /// A bare name is a boolean property, not a malformed assignment.
    #[test]
    fn a_flag_property_has_no_value() {
        let nodes = parse(DTS);
        let flag = &node(&nodes, "/soc/i2c@60013000").props[0];
        assert_eq!(flag.name, "interrupt-controller");
        assert_eq!(flag.value, "");
        assert!(flag.is_flag());
        assert_eq!(flag.line, Some(100));
    }

    /// Only a status that is not `okay` is worth marking: a node with no
    /// `status` at all is enabled, and drawing both the same way would put a
    /// warning on most of the tree.
    #[test]
    fn only_an_explicit_non_okay_status_counts_as_disabled() {
        let nodes = parse(DTS);
        let i2c = node(&nodes, "/soc/i2c@60013000");
        assert_eq!(i2c.status(), Some("disabled"));
        assert!(i2c.disabled());

        let cpus = node(&nodes, "/cpus");
        assert_eq!(cpus.status(), None);
        assert!(!cpus.disabled());

        let okay = parse("/* node '/x' defined in a.dts:1 */\n/ {\n\tstatus = \"okay\";\n};\n");
        assert!(!okay[0].disabled());
    }

    #[test]
    fn a_node_knows_whether_anything_sits_below_it() {
        let nodes = parse(DTS);
        assert!(node(&nodes, "/").has_children);
        assert!(node(&nodes, "/cpus").has_children);
        assert!(!node(&nodes, "/cpus/cpu@0").has_children);
        assert!(!node(&nodes, "/soc/i2c@60013000").has_children);
    }

    /// A node without its annotation still gets an identity, built from the
    /// ancestors that are open --- the tree must not lose a branch because
    /// one comment is missing.
    #[test]
    fn a_node_missing_its_annotation_gets_a_derived_path() {
        let nodes = parse("/ {\n\tchild {\n\t\tfoo = < 1 >;\n\t};\n};\n");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].path, "/");
        assert_eq!(nodes[1].path, "/child");
        assert_eq!(nodes[1].props.len(), 1);
        assert_eq!(nodes[1].file, None);
    }

    /// A value that itself contains a brace must not be read as a node
    /// opening --- the reason structure is taken from lines that *end* in
    /// `{` rather than from a running brace count.
    #[test]
    fn a_brace_inside_a_value_does_not_open_a_node() {
        let nodes = parse("/ {\n\tlabel = \"a { b\";\n\tother = < 1 >;\n};\n");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].props.len(), 2);
        assert_eq!(nodes[0].props[0].value, "\"a { b\"");
    }

    #[test]
    fn a_missing_or_foreign_file_yields_no_nodes() {
        for text in ["", "/dts-v1/;\n", "not a devicetree\n"] {
            assert!(parse(text).is_empty(), "expected nothing for {text:?}");
        }
    }

    /// A file cut off mid-node keeps the nodes that opened, so the tab shows
    /// what it can rather than nothing.
    #[test]
    fn a_truncated_file_keeps_what_opened() {
        let cut = DTS.split_once("\t\t};").expect("has a close").0;
        let nodes = parse(cut);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[2].path, "/cpus/cpu@0");
        assert_eq!(nodes[2].props.len(), 2);
    }
}
