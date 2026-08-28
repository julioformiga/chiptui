//! `<build>/dashboard/{all,ram,rom}_report.json` --- the per-symbol memory
//! tree, and the one artifact that has to be *produced*.
//!
//! `scripts/footprint/size_report` writes it, and `dashboard.py` runs that
//! script (`_create_memory_reports`) whenever `all_report.json` is missing or
//! older than the ELF. It is the only page here that a tool must generate,
//! because mapping symbols back to source files needs the ELF's DWARF --- so
//! it is also the only place this feature spends a subprocess. The file is
//! JSON, which is what makes reading it a parse rather than a
//! reimplementation.
//!
//! The shape is `anytree`'s `DictExporter` over the script's `TreeNode`,
//! with a `k.lstrip('_')` attribute renamer:
//!
//! ```json
//! { "symbols": { "name": "Root", "identifier": "root", "size": 12,
//!                "loc": [],
//!                "children": [ { "name": "kernel", "identifier": "kernel",
//!                                "size": 12, "loc": [], "children": [...] } ] },
//!   "total_size": 40 }
//! ```
//!
//! Two details come from `size_report`'s own code rather than from the
//! shape: `address` and `section` are set **only on terminal symbol nodes**
//! (`_insert_one_elem` assigns them after the walk, and uses
//! `hasattr(item, 'address')` to tell a symbol from a directory), and
//! `total_size` is the *region's* size, not the tree's --- so a percentage
//! is a share of the region, which is the number worth showing.
//!
//! Nodes are flattened into a pre-order `Vec` with an explicit `depth`, the
//! same choice [`super::devicetree`] makes and for the same reason: the pane
//! renders a list, and a flat vector is what the row walk and the expansion
//! set index into.

use super::json::Json;

/// One node of the tree: a directory, a file, or a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNode {
    /// The last path segment --- what the row shows.
    pub name: String,
    /// The full path from the tree's root, and `size_report`'s own stable
    /// key for the node. This is what the expansion set stores.
    pub identifier: String,
    /// Bytes. On a parent this is the sum of its children.
    pub size: u64,
    /// How deep the node sits; the root is 0.
    pub depth: usize,
    /// The symbol's address --- present only on a terminal symbol node.
    pub address: Option<u64>,
    /// The ELF section the symbol landed in, likewise terminal-only.
    pub section: Option<String>,
    /// Which region(s) the symbol belongs to, e.g. `["ROM"]`.
    pub loc: Vec<String>,
    pub has_children: bool,
}

impl MemoryNode {
    /// Whether this is a symbol rather than a directory or file node.
    /// `size_report` marks exactly the terminal nodes with an address, which
    /// is the test its own code uses.
    pub fn is_symbol(&self) -> bool {
        self.address.is_some()
    }

    /// Whether the node is one of the script's synthetic groupings ---
    /// `(no paths)`, `(hidden)` and friends, which it names in parentheses
    /// and which `dashboard.py` excludes from the largest-symbols list.
    pub fn is_group(&self) -> bool {
        self.name.starts_with('(')
    }
}

/// One report file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryReport {
    /// The size of the region the report covers, from the script's
    /// `addr_ranges[f'{t}_total_size']`. Percentages are shares of this,
    /// not of the tree.
    pub total_size: u64,
    /// Every node, pre-order, root first.
    pub nodes: Vec<MemoryNode>,
}

impl MemoryReport {
    /// The share of the region a size represents, 0 when the region has no
    /// declared size.
    pub fn percent(&self, size: u64) -> f64 {
        if self.total_size == 0 {
            return 0.0;
        }
        size as f64 * 100.0 / self.total_size as f64
    }

    /// The largest symbols, biggest first --- `dashboard.py`'s
    /// `_symbols_by_size`: leaves only, and never one of the synthetic
    /// parenthesised groups.
    pub fn largest(&self, limit: usize) -> Vec<&MemoryNode> {
        let mut symbols: Vec<&MemoryNode> = self
            .nodes
            .iter()
            .filter(|node| !node.has_children && !node.is_group() && node.depth > 0)
            .collect();
        symbols.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        symbols.truncate(limit);
        symbols
    }
}

/// Reads a `*_report.json`. `None` when the text is not one --- a truncated
/// write, or a file from some other tool.
pub fn parse(text: &str) -> Option<MemoryReport> {
    let value = super::json::parse(text)?;
    let root = value.get("symbols")?;
    let total_size = value.get("total_size").and_then(Json::as_u64).unwrap_or(0);
    let mut nodes = Vec::new();
    // Pre-order with an explicit stack: the tree is as deep as the source
    // tree it mirrors, and recursion here would put that depth on the
    // program's stack for no gain.
    let mut stack = vec![(root, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        let Some(node) = read_node(value, depth) else {
            continue;
        };
        nodes.push(node);
        if let Some(children) = value.get("children").and_then(Json::as_array) {
            // Reversed, so popping yields source order.
            for child in children.iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    Some(MemoryReport { total_size, nodes })
}

fn read_node(value: &Json, depth: usize) -> Option<MemoryNode> {
    let name = value.get("name")?.as_str()?.to_string();
    let children = value.get("children").and_then(Json::as_array);
    Some(MemoryNode {
        name,
        identifier: value
            .get("identifier")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string(),
        size: value.get("size").and_then(Json::as_u64).unwrap_or(0),
        depth,
        address: value.get("address").and_then(Json::as_u64),
        section: value
            .get("section")
            .and_then(Json::as_str)
            .map(str::to_string),
        loc: value
            .get("loc")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        has_children: children.is_some_and(|children| !children.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real file's shape in miniature: a `Root` whose children are the
    /// script's own groupings, a directory chain, terminal symbols carrying
    /// `address`/`section` that the parents do not, a parenthesised
    /// grouping, and a `loc` naming the region.
    const REPORT: &str = r#"{
  "symbols": {
    "name": "Root", "size": 300, "identifier": "root", "loc": [],
    "children": [
      { "name": "(no paths)", "size": 40, "identifier": ":", "loc": [],
        "children": [
          { "name": "CSWTCH.6", "size": 40, "identifier": ":/CSWTCH.6",
            "loc": ["ram"], "address": 1070146940, "section": ".dram0.data" }
        ] },
      { "name": "ZEPHYR_BASE", "size": 260, "identifier": "/z", "loc": [],
        "children": [
          { "name": "kernel", "size": 260, "identifier": "kernel", "loc": [],
            "children": [
              { "name": "mempool.c", "size": 260, "identifier": "kernel/mempool.c",
                "loc": [],
                "children": [
                  { "name": "kheap__system_heap", "size": 200,
                    "identifier": "kernel/mempool.c/kheap__system_heap",
                    "loc": ["ram"], "address": 1070000000,
                    "section": ".dram0.noinit" },
                  { "name": "z_sys_heap", "size": 60,
                    "identifier": "kernel/mempool.c/z_sys_heap",
                    "loc": ["rom"], "address": 1006632960,
                    "section": ".flash.rodata" }
                ] }
            ] }
        ] }
    ]
  },
  "total_size": 600
}"#;

    fn node<'a>(report: &'a MemoryReport, identifier: &str) -> &'a MemoryNode {
        report
            .nodes
            .iter()
            .find(|node| node.identifier == identifier)
            .unwrap_or_else(|| panic!("{identifier} is missing"))
    }

    #[test]
    fn the_tree_flattens_pre_order_with_depths() {
        let report = parse(REPORT).expect("parses");
        let seen: Vec<(&str, usize)> = report
            .nodes
            .iter()
            .map(|node| (node.name.as_str(), node.depth))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("Root", 0),
                ("(no paths)", 1),
                ("CSWTCH.6", 2),
                ("ZEPHYR_BASE", 1),
                ("kernel", 2),
                ("mempool.c", 3),
                ("kheap__system_heap", 4),
                ("z_sys_heap", 4),
            ],
            "source order, parents before their children"
        );
    }

    /// `size_report` sets `address` and `section` only on terminal symbol
    /// nodes (`_insert_one_elem` assigns them after the walk). That is the
    /// test for "is this a symbol or a directory", and the pane's details
    /// depend on getting it right.
    #[test]
    fn only_terminal_nodes_carry_an_address_and_a_section() {
        let report = parse(REPORT).expect("parses");
        let symbol = node(&report, "kernel/mempool.c/kheap__system_heap");
        assert!(symbol.is_symbol());
        assert_eq!(symbol.address, Some(1_070_000_000));
        assert_eq!(symbol.section.as_deref(), Some(".dram0.noinit"));
        assert_eq!(symbol.loc, vec!["ram".to_string()]);
        assert!(!symbol.has_children);

        let directory = node(&report, "kernel/mempool.c");
        assert!(!directory.is_symbol());
        assert_eq!(directory.address, None);
        assert_eq!(directory.section, None);
        assert!(directory.has_children);
    }

    /// Percentages are shares of the *region*, which is what `total_size`
    /// holds --- not of the tree, whose root here sums to half of it.
    #[test]
    fn percentages_are_shares_of_the_region_not_the_tree() {
        let report = parse(REPORT).expect("parses");
        assert_eq!(report.total_size, 600);
        assert_eq!(report.nodes[0].size, 300);
        assert!((report.percent(300) - 50.0).abs() < f64::EPSILON);
        assert!((report.percent(150) - 25.0).abs() < f64::EPSILON);
    }

    /// A report whose region size is missing must not divide by zero.
    #[test]
    fn a_report_without_a_region_size_answers_zero_percent() {
        let report = parse(r#"{"symbols":{"name":"Root","size":8,"identifier":"r","loc":[]}}"#)
            .expect("parses");
        assert_eq!(report.total_size, 0);
        assert_eq!(report.percent(8), 0.0);
    }

    /// `dashboard.py::_symbols_by_size`: leaves only, biggest first, and
    /// never one of the parenthesised groupings the script invents.
    #[test]
    fn the_largest_list_is_leaves_only_biggest_first() {
        let report = parse(REPORT).expect("parses");
        let largest: Vec<(&str, u64)> = report
            .largest(10)
            .iter()
            .map(|node| (node.name.as_str(), node.size))
            .collect();
        assert_eq!(
            largest,
            vec![
                ("kheap__system_heap", 200),
                ("z_sys_heap", 60),
                ("CSWTCH.6", 40),
            ],
            "no directories, no Root, no `(no paths)` group"
        );
    }

    #[test]
    fn the_largest_list_honours_its_limit() {
        let report = parse(REPORT).expect("parses");
        assert_eq!(report.largest(2).len(), 2);
        assert_eq!(report.largest(0).len(), 0);
    }

    /// A grouping node is named in parentheses; it is a real row in the
    /// tree but never a "largest symbol".
    #[test]
    fn parenthesised_groupings_are_recognised() {
        let report = parse(REPORT).expect("parses");
        assert!(node(&report, ":").is_group());
        assert!(!node(&report, "kernel").is_group());
    }

    /// An empty `children` array means a leaf, not a parent with nothing in
    /// it --- the marker column would otherwise offer an expansion that
    /// reveals no rows.
    #[test]
    fn an_empty_children_array_is_a_leaf() {
        let report = parse(
            r#"{"symbols":{"name":"Root","size":0,"identifier":"r","loc":[],"children":[]},
                "total_size":4}"#,
        )
        .expect("parses");
        assert_eq!(report.nodes.len(), 1);
        assert!(!report.nodes[0].has_children);
    }

    #[test]
    fn a_file_that_is_not_a_report_answers_none() {
        for text in ["", "{}", r#"{"total_size": 4}"#, "[1,2,3]", "{\"symbols\":"] {
            assert!(parse(text).is_none(), "expected None for {text:?}");
        }
    }

    /// A node with no name is not a row; the walk must skip it and still
    /// deliver everything around it.
    #[test]
    fn a_nameless_node_is_skipped_not_fatal() {
        let report = parse(
            r#"{"symbols":{"name":"Root","size":2,"identifier":"r","loc":[],
                 "children":[{"size":1},{"name":"ok","size":1,"identifier":"ok","loc":[]}]},
                "total_size":2}"#,
        )
        .expect("parses");
        let names: Vec<&str> = report.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["Root", "ok"]);
    }
}
