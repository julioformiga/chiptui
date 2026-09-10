//! The devicetree's own memory regions --- the extra Memory tabs.
//!
//! `dashboard.py` gives every devicetree node carrying
//! `compatible = "zephyr,memory-region"` its own report and its own tab in
//! the Memory view (`_create_memory_reports`): on the ESP32-C3 that means
//! `SRAM1` and `RTC_FAST_RAM` beside the three fixed ones, while `SRAM0`
//! is dropped because no allocated ELF section lands in it. This module
//! answers the same two questions that code asks, from the two artifacts
//! the window already reads --- the regions from `zephyr.dts`
//! ([`super::devicetree`], where `dashboard.py` reads them out of
//! `edt.pickle`), the overlap test from `zephyr.stat` ([`super::elf_stat`],
//! where `dashboard.py` opens the ELF with `pyelftools`).
//!
//! Rule for rule, so the two dashboards never disagree about which tabs a
//! build has:
//!
//! * a region node is `status = "okay"` (absent means okay), carries a
//!   `zephyr,memory-region` string and at least one `reg`; the first reg's
//!   address and size are the region's;
//! * a region is *reportable* when the ELF holds an allocated, non-empty
//!   section that overlaps `[addr, addr+size)`.

use super::devicetree::DtNode;
use super::elf_stat::Section;

/// One devicetree memory region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion {
    /// The `zephyr,memory-region` value --- also the report file's stem
    /// (`SRAM1_report.json`) and the tab's title.
    pub name: String,
    pub addr: u64,
    pub size: u64,
    /// The node's own path, which the details pane shows.
    pub path: String,
}

/// Every `zephyr,memory-region` node in devicetree order --- the order the
/// tabs follow, `dashboard.py`'s own iteration order.
///
/// `#address-cells`/`#size-cells` are read from the parent node the same
/// way `dtlib` resolves them, defaulting to the spec's `2`/`1` when the
/// parent declares neither. Only the first reg is taken, as
/// `dashboard.py`'s `regs[0]` does.
pub fn discover(nodes: &[DtNode]) -> Vec<MemoryRegion> {
    let mut regions = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if node.disabled() {
            continue;
        }
        if !compatible_is_memory_region(node) {
            continue;
        }
        let Some(name) = prop(node, "zephyr,memory-region")
            .and_then(|value| quoted_values(value).first().map(|name| name.to_string()))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let (address_cells, size_cells) = cells_of(parent(nodes, index));
        let Some((addr, size)) = first_reg(node, address_cells, size_cells) else {
            continue;
        };
        regions.push(MemoryRegion {
            name,
            addr,
            size,
            path: node.path.clone(),
        });
    }
    regions
}

/// The reportable subset: regions with at least one allocated, non-empty
/// ELF section inside `[addr, addr+size)`. An empty section list --- no
/// `zephyr.stat` --- answers nothing, the same way `dashboard.py`'s overlap
/// test over an unopenable ELF answers False for every region.
pub fn reportable(regions: &[MemoryRegion], sections: &[Section]) -> Vec<MemoryRegion> {
    regions
        .iter()
        .filter(|region| has_sections_in_range(sections, region.addr, region.size))
        .cloned()
        .collect()
}

/// `dashboard.py::_has_sections_in_range`, verbatim in effect: any section
/// with `SHF_ALLOC`, a non-zero size, and `sec_start < end && sec_end >
/// addr`.
pub fn has_sections_in_range(sections: &[Section], addr: u64, size: u64) -> bool {
    let end = addr.saturating_add(size);
    sections.iter().any(|section| {
        section.size > 0
            && section.allocated()
            && section.addr < end
            && section.addr.saturating_add(section.size) > addr
    })
}

/// Whether `compatible` lists `"zephyr,memory-region"`. The value is a
/// string list, and a compatible string may itself hold a comma --- the
/// one in `zephyr,memory-region` --- so the elements are read as quoted
/// strings rather than split apart on commas.
fn compatible_is_memory_region(node: &DtNode) -> bool {
    prop(node, "compatible")
        .is_some_and(|value| quoted_values(value).contains(&"zephyr,memory-region"))
}

/// The quoted strings of a string-list value: `"a", "b"` answers
/// `["a", "b"]`. An unquoted or empty value answers nothing, which is the
/// correct reading of a value that is not a string list.
fn quoted_values(value: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('"') {
        let Some(close) = rest[open + 1..].find('"') else {
            break;
        };
        items.push(&rest[open + 1..open + 1 + close]);
        rest = &rest[open + close + 2..];
    }
    items
}

fn prop<'a>(node: &'a DtNode, name: &str) -> Option<&'a str> {
    node.props
        .iter()
        .find(|prop| prop.name == name)
        .map(|prop| prop.value.as_str())
}

/// The nearest open ancestor's node --- pre-order's answer to "parent",
/// the same walk [`super::devicetree`]'s visibility rule uses.
fn parent(nodes: &[DtNode], index: usize) -> Option<&DtNode> {
    let depth = nodes[index].depth;
    if depth == 0 {
        return None;
    }
    nodes[..index].iter().rev().find(|node| node.depth < depth)
}

/// `#address-cells`/`#size-cells` off the parent, with the devicetree
/// spec's defaults (`2`/`1`) where it declares neither --- `dtlib`'s own
/// fallback, and therefore `edt.pickle`'s.
fn cells_of(parent: Option<&DtNode>) -> (u64, u64) {
    let cell = |name: &str, default: u64| {
        parent
            .and_then(|node| prop(node, name))
            .and_then(first_cell)
            .filter(|count| *count > 0 && *count <= 4)
            .unwrap_or(default)
    };
    (cell("#address-cells", 2), cell("#size-cells", 1))
}

/// The first reg tuple's `(addr, size)` --- `< 0x3fc80000 0x60000 >` with
/// one cell each is `(0x3fc80000, 0x60000)`. `None` when the node has no
/// `reg` or the cells do not cover a whole tuple.
fn first_reg(node: &DtNode, address_cells: u64, size_cells: u64) -> Option<(u64, u64)> {
    let value = prop(node, "reg")?;
    let cells = angle_cells(value);
    let address_cells = address_cells as usize;
    let size_cells = size_cells as usize;
    let tuple = address_cells + size_cells;
    if tuple == 0 || cells.len() < tuple {
        return None;
    }
    let join = |parts: &[u64]| parts.iter().fold(0u64, |acc, cell| acc << 32 | cell);
    Some((
        join(&cells[..address_cells]),
        join(&cells[address_cells..tuple]),
    ))
}

/// The numbers inside a `< … >` property value, each `0x…` or decimal.
/// `dtlib` writes every cell separately (`< 0x1 0x2 >`), so this is a
/// split on whitespace inside the angle brackets.
fn angle_cells(value: &str) -> Vec<u64> {
    let Some(start) = value.find('<') else {
        return Vec::new();
    };
    let Some(end) = value[start..].find('>') else {
        return Vec::new();
    };
    value[start + 1..start + end]
        .split_whitespace()
        .filter_map(|cell| {
            let cell = cell.trim_start_matches("0x");
            u64::from_str_radix(cell, 16)
                .ok()
                .or_else(|| cell.parse::<u64>().ok())
        })
        .collect()
}

/// The first number inside a `< … >` value, for the cell-count properties.
fn first_cell(value: &str) -> Option<u64> {
    angle_cells(value).first().copied()
}

#[cfg(test)]
mod tests {
    use super::super::devicetree;
    use super::*;

    /// Cut from a real ESP32-C3 `zephyr.dts` (`/soc` and its three memory
    /// nodes, with the parent's cell counts), plus two shapes the real tree
    /// also holds: a disabled region and a node that is compatible but
    /// carries no `zephyr,memory-region` name.
    const DTS: &str = "\
/* node '/' defined in esp32c3_common.dtsi:15 */
/ {
\t#address-cells = < 0x1 >;
\t#size-cells = < 0x1 >;

\t/* node '/soc' defined in esp32c3_common.dtsi:20 */
\tsoc {
\t\t#address-cells = < 0x1 >;
\t\t#size-cells = < 0x1 >;

\t\t/* node '/soc/memory@4037c000' defined in esp32c3_common.dtsi:95 */
\t\tsram0: memory@4037c000 {
\t\t\tcompatible = \"zephyr,memory-region\",
\t\t\t             \"mmio-sram\";      /* in esp32c3_common.dtsi:96 */
\t\t\treg = < 0x4037c000 0x4000 >;
\t\t\tzephyr,memory-region = \"SRAM0\"; /* in esp32c3_common.dtsi:98 */
\t\t};

\t\t/* node '/soc/memory@3fc80000' defined in esp32c3_common.dtsi:101 */
\t\tsram1: memory@3fc80000 {
\t\t\tcompatible = \"zephyr,memory-region\",
\t\t\t             \"mmio-sram\";      /* in esp32c3_common.dtsi:102 */
\t\t\treg = < 0x3fc80000 0x60000 >;
\t\t\tzephyr,memory-region = \"SRAM1\"; /* in esp32c3_common.dtsi:104 */
\t\t};

\t\t/* node '/soc/memory@50000000' defined in esp32c3_common.dtsi:107 */
\t\trtc: memory@50000000 {
\t\t\tcompatible = \"zephyr,memory-region\",
\t\t\t             \"mmio-sram\";        /* in esp32c3_common.dtsi:108 */
\t\t\treg = < 0x50000000 0x2000 >;
\t\t\tzephyr,memory-region = \"RTC_FAST_RAM\"; /* in esp32c3_common.dtsi:110 */
\t\t};

\t\t/* node '/soc/rtc_slow@50002000' defined in esp32c3_common.dtsi:113 */
\t\trtc_slow: rtc_slow@50002000 {
\t\t\tcompatible = \"zephyr,memory-region\";
\t\t\treg = < 0x50002000 0x2000 >;
\t\t\tstatus = \"disabled\";
\t\t\tzephyr,memory-region = \"RTC_SLOW_RAM\"; /* never written */
\t\t};

\t\t/* node '/soc/unnamed@60000000' defined in esp32c3_common.dtsi:120 */
\t\tunnamed: unnamed@60000000 {
\t\t\tcompatible = \"zephyr,memory-region\";
\t\t\treg = < 0x60000000 0x1000 >;
\t\t};

\t\t/* node '/soc/i2c@60013000' defined in esp32c3_common.dtsi:125 */
\t\ti2c0: i2c@60013000 {
\t\t\tcompatible = \"espressif,esp32c3-i2c\";
\t\t\treg = < 0x60013000 0x4000 >;
\t\t};
\t};
};
";

    fn regions() -> Vec<MemoryRegion> {
        discover(&devicetree::parse(DTS))
    }

    /// The three enabled regions in devicetree order --- which is the tab
    /// order --- with the first reg's address and size. The disabled node
    /// and the nameless one are not regions.
    #[test]
    fn regions_read_in_devicetree_order() {
        assert_eq!(
            regions(),
            vec![
                MemoryRegion {
                    name: "SRAM0".into(),
                    addr: 0x4037_c000,
                    size: 0x4000,
                    path: "/soc/memory@4037c000".into(),
                },
                MemoryRegion {
                    name: "SRAM1".into(),
                    addr: 0x3fc8_0000,
                    size: 0x60000,
                    path: "/soc/memory@3fc80000".into(),
                },
                MemoryRegion {
                    name: "RTC_FAST_RAM".into(),
                    addr: 0x5000_0000,
                    size: 0x2000,
                    path: "/soc/memory@50000000".into(),
                },
            ]
        );
    }

    /// A section table shaped like the real ESP32-C3 one: `.iram0.text`
    /// and `.dram0.*` inside SRAM1's range, a `NOBITS` inside
    /// RTC_FAST_RAM, and nothing at all inside SRAM0 --- which is exactly
    /// why the Zephyr dashboard has no SRAM0 tab.
    fn sections() -> Vec<Section> {
        let row =
            |index: usize, name: &str, kind: &str, flags: &str, addr: u64, size: u64| Section {
                index,
                name: name.into(),
                kind: kind.into(),
                flags: flags.into(),
                addr,
                size,
            };
        vec![
            row(0, "", "NULL", "", 0, 0),
            row(1, ".iram0.text", "PROGBITS", "AX", 0x4038_0000, 0xfd58),
            row(2, ".dram0.dummy", "NOBITS", "WA", 0x3fc8_0000, 0x10640),
            row(3, ".rtc.force_slow", "PROGBITS", "WA", 0x5000_0000, 0x24),
            row(4, ".debug_info", "PROGBITS", "", 0, 0x3e_9fce),
            row(5, ".flash.rodata", "PROGBITS", "A", 0x3c00_0000, 0xe_0000),
        ]
    }

    /// SRAM1 and RTC_FAST_RAM have allocated sections inside them; SRAM0
    /// does not (`.iram0.text` starts at `0x40380000`, past its
    /// `0x4037c000+0x4000` end), so it is not a tab --- the same three-tab
    /// shape the real project's dashboard shows.
    #[test]
    fn only_regions_the_elf_lands_in_are_reportable() {
        let reportable = reportable(&regions(), &sections());
        let names: Vec<&str> = reportable
            .iter()
            .map(|region| region.name.as_str())
            .collect();
        assert_eq!(names, vec!["SRAM1", "RTC_FAST_RAM"]);
    }

    /// The overlap test is `sec_start < end && sec_end > addr` --- a
    /// section that only touches an edge is not inside, and a section
    /// that spans the whole region is.
    #[test]
    fn the_overlap_test_takes_the_open_interval() {
        let row = |addr: u64, size: u64| Section {
            index: 1,
            name: ".x".into(),
            kind: "PROGBITS".into(),
            flags: "A".into(),
            addr,
            size,
        };
        // Ends exactly where the region starts.
        assert!(!has_sections_in_range(&[row(0xf00, 0x100)], 0x1000, 0x100));
        // Starts exactly where the region ends.
        assert!(!has_sections_in_range(&[row(0x1100, 0x100)], 0x1000, 0x100));
        // Spans it whole.
        assert!(has_sections_in_range(&[row(0x0, 0x5000)], 0x1000, 0x100));
        // One byte inside.
        assert!(has_sections_in_range(&[row(0x10ff, 0x2)], 0x1000, 0x100));
    }

    /// Sections without the alloc flag --- the debug sections, the
    /// symtab --- never count, whatever their addresses; a zero-size one
    /// does not either.
    #[test]
    fn unallocated_and_empty_sections_never_count() {
        let unallocated = Section {
            index: 1,
            name: ".debug_info".into(),
            kind: "PROGBITS".into(),
            flags: String::new(),
            addr: 0x10,
            size: 0x100,
        };
        let empty = Section {
            flags: "A".into(),
            size: 0,
            addr: 0x10,
            ..unallocated.clone()
        };
        assert!(!has_sections_in_range(&[unallocated], 0, 0x1000));
        assert!(!has_sections_in_range(&[empty], 0, 0x1000));
    }

    /// A 64-bit board declares two address cells, and the cells join
    /// big-endian into one address --- the way `dtlib` reads them.
    #[test]
    fn address_cells_join_into_one_address() {
        let nodes = devicetree::parse(
            "/ {\n\t#address-cells = < 0x1 >;\n\t#size-cells = < 0x1 >;\n\
             \t/* node '/soc' defined in a.dtsi:1 */\n\tsoc {\n\
             \t\t#address-cells = < 0x2 >;\n\t\t#size-cells = < 0x1 >;\n\
             \t\t/* node '/soc/mem' defined in a.dtsi:2 */\n\t\tmem {\n\
             \t\t\tcompatible = \"zephyr,memory-region\";\n\
             \t\t\treg = < 0x1 0x0 0x2000 >;\n\
             \t\t\tzephyr,memory-region = \"BIG\";\n\t\t};\n\t};\n};\n",
        );
        assert_eq!(
            discover(&nodes),
            vec![MemoryRegion {
                name: "BIG".into(),
                addr: 0x1_0000_0000,
                size: 0x2000,
                path: "/soc/mem".into(),
            }]
        );
    }

    /// A tree with no memory regions --- most boards --- answers none, and
    /// so does a tree that does not parse.
    #[test]
    fn a_tree_without_regions_answers_none() {
        let nodes =
            devicetree::parse("/* node '/' defined in a.dts:1 */\n/ {\n\tmodel = \"x\";\n};\n");
        assert!(discover(&nodes).is_empty());
        assert!(discover(&devicetree::parse("")).is_empty());
    }
}
