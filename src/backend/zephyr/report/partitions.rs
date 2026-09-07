//! The flash layout, read out of the build's own devicetree.
//!
//! Flashing an MCUboot project means writing two images to two addresses,
//! and the addresses are not a property of the SoC family --- they are
//! whatever the board's devicetree says. `boot_partition` is at `0x0` on the
//! Espressif 4 MB layout and somewhere else on a board that reserves a
//! region below it; `slot0_partition` follows a `sys` partition here and
//! nothing there. Tabulating them per family would be the mechanism-specific
//! guess `SPEC.md` §10 forbids, and it would be wrong on the first
//! out-of-tree board.
//!
//! So they are read. `<build>/zephyr/zephyr.dts` records the partitions the
//! build actually resolved, with their addresses:
//!
//! ```text
//! boot_partition: partition@0 {
//!         label = "mcuboot";              /* in …/partitions_0x0_default_4M.dtsi:15 */
//!         reg = < 0x0 0x10000 >;          /* in …/partitions_0x0_default_4M.dtsi:16 */
//! };
//! ```
//!
//! The file is there after an **ordinary** build --- no sysbuild, no MCUboot,
//! no instrumentation --- which is what makes it usable as a precondition
//! check: whether a board can do A/B updates at all is answerable from a
//! build the user has already done, before anything is changed in the
//! project.
//!
//! Node labels are the identity here, not the `label` property. Zephyr's own
//! `FIXED_PARTITION_ID()` and every Kconfig choice name the node
//! (`slot0_partition`); the `label` string (`"image-0"`) is what MCUboot
//! prints. Matching on the node keeps this aligned with what the rest of the
//! toolchain calls them.

use super::devicetree::DtNode;

/// The node labels a two-slot layout is made of. Order matters only for the
/// refusal message, which lists them as they are missing.
const BOOT: &str = "boot_partition";
const SLOT0: &str = "slot0_partition";
const SLOT1: &str = "slot1_partition";

/// One partition, as the devicetree resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// The node label, e.g. `slot0_partition` --- the name Zephyr's own
    /// `FIXED_PARTITION_ID()` and the MCUboot Kconfig use.
    pub node: String,
    /// The `label` property, e.g. `image-0`. Absent on a partition that
    /// declares none; it is the string MCUboot prints, not an identity.
    pub label: Option<String>,
    /// Offset from the start of flash --- the address an external flasher
    /// writes to.
    pub address: u64,
    pub size: u64,
}

/// Every partition the build resolved, in devicetree order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashLayout {
    pub partitions: Vec<Partition>,
}

impl FlashLayout {
    /// Collects the partitions out of a parsed devicetree.
    ///
    /// A node counts as a partition when it carries a `reg` with both cells
    /// and sits under a `partitions` node --- the second test is what keeps
    /// a peripheral's own `reg` out of the list. Anything unreadable is
    /// skipped rather than fatal: a layout missing one entry produces a
    /// named refusal downstream, which is a better failure than refusing to
    /// read the file at all.
    pub fn read(nodes: &[DtNode]) -> Self {
        let partitions = nodes
            .iter()
            .filter(|node| node.path.contains("/partitions/"))
            .filter_map(Partition::from_node)
            .collect();
        Self { partitions }
    }

    /// The partition with this node label, e.g. `slot0_partition`.
    pub fn node(&self, label: &str) -> Option<&Partition> {
        self.partitions
            .iter()
            .find(|partition| partition.node == label)
    }

    /// Where MCUboot itself is written.
    pub fn boot(&self) -> Option<&Partition> {
        self.node(BOOT)
    }

    /// Where the running image lives --- the address the signed application
    /// is flashed to.
    pub fn slot0(&self) -> Option<&Partition> {
        self.node(SLOT0)
    }

    /// Where an update is staged before the swap. Never written by the
    /// flasher; it is here because its presence is what makes the board
    /// updatable at all.
    pub fn slot1(&self) -> Option<&Partition> {
        self.node(SLOT1)
    }

    /// Whether this board can do A/B updates: two slots to swap between.
    pub fn supports_ab(&self) -> bool {
        self.missing_for_ab().is_empty()
    }

    /// The nodes an A/B layout needs and this one lacks, so the refusal can
    /// name them instead of saying the board is unsupported.
    ///
    /// `boot_partition` is included: a board with two slots and nowhere to
    /// put the bootloader cannot run MCUboot either.
    pub fn missing_for_ab(&self) -> Vec<&'static str> {
        [BOOT, SLOT0, SLOT1]
            .into_iter()
            .filter(|label| self.node(label).is_none())
            .collect()
    }
}

impl Partition {
    /// Reads one partition out of a node, or `None` if it is not one.
    fn from_node(node: &DtNode) -> Option<Self> {
        let reg = node.props.iter().find(|prop| prop.name == "reg")?;
        let (address, size) = split_reg(&reg.value)?;
        Some(Self {
            // A partition always carries exactly one label in practice; the
            // first is the one every reference uses.
            node: node.labels.first()?.clone(),
            label: node
                .props
                .iter()
                .find(|prop| prop.name == "label")
                .map(|prop| prop.value.trim_matches('"').to_string()),
            address,
            size,
        })
    }
}

/// Reads the address and size out of a `reg` value.
///
/// The devicetree writer collapses the cells to `< 0x20000 0x1c0000 >`, and
/// a partition's parent declares one address cell and one size cell, so the
/// first two numbers are the pair. A `reg` with more cells (a peripheral
/// with several windows) yields its first pair and is filtered out by the
/// caller's path test rather than here.
fn split_reg(value: &str) -> Option<(u64, u64)> {
    let mut cells = value
        .trim_matches(|c| c == '<' || c == '>')
        .split_whitespace()
        .filter_map(parse_cell);
    Some((cells.next()?, cells.next()?))
}

/// One devicetree cell: hexadecimal with the `0x` the writer always emits,
/// decimal otherwise.
fn parse_cell(token: &str) -> Option<u64> {
    match token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => token.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::devicetree;
    use super::*;

    /// A trimmed copy of the real `zephyr.dts` this was written against ---
    /// a plain `west build` of an ESP32-C3 application, with no sysbuild and
    /// no MCUboot anywhere in the project.
    const DTS: &str = r#"
/* node '/soc/flash-controller@60002000/flash@0/partitions' defined in esp32c3.dtsi:10 */
partitions {
        compatible = "fixed-partitions";        /* in esp32c3.dtsi:11 */

        /* node '/soc/flash-controller@60002000/flash@0/partitions/partition@0' defined in partitions_0x0_default_4M.dtsi:13 */
        boot_partition: partition@0 {
                compatible = "zephyr,mapped-partition"; /* in partitions_0x0_default_4M.dtsi:14 */
                label = "mcuboot";                      /* in partitions_0x0_default_4M.dtsi:15 */
                reg = < 0x0 0x10000 >;                  /* in partitions_0x0_default_4M.dtsi:16 */
        };

        /* node '/soc/flash-controller@60002000/flash@0/partitions/partition@20000' defined in partitions_0x0_default_4M.dtsi:25 */
        slot0_partition: partition@20000 {
                label = "image-0";                      /* in partitions_0x0_default_4M.dtsi:27 */
                reg = < 0x20000 0x1c0000 >;             /* in partitions_0x0_default_4M.dtsi:28 */
        };

        /* node '/soc/flash-controller@60002000/flash@0/partitions/partition@1e0000' defined in partitions_0x0_default_4M.dtsi:31 */
        slot1_partition: partition@1e0000 {
                label = "image-1";                      /* in partitions_0x0_default_4M.dtsi:33 */
                reg = < 0x1e0000 0x1c0000 >;            /* in partitions_0x0_default_4M.dtsi:34 */
        };

        /* node '/soc/flash-controller@60002000/flash@0/partitions/partition@3b0000' defined in partitions_0x0_default_4M.dtsi:49 */
        storage_partition: partition@3b0000 {
                label = "storage";                      /* in partitions_0x0_default_4M.dtsi:50 */
                reg = < 0x3b0000 0x30000 >;             /* in partitions_0x0_default_4M.dtsi:51 */
        };
};
"#;

    fn layout() -> FlashLayout {
        FlashLayout::read(&devicetree::parse(DTS))
    }

    #[test]
    fn reads_the_addresses_a_flasher_needs() {
        let layout = layout();

        let boot = layout.boot().expect("boot_partition");
        let slot0 = layout.slot0().expect("slot0_partition");

        // The two addresses the ESP32 two-image write uses, neither of them
        // hard-coded anywhere.
        assert_eq!(boot.address, 0x0);
        assert_eq!(boot.size, 0x10000);
        assert_eq!(slot0.address, 0x20000);
        assert_eq!(slot0.label.as_deref(), Some("image-0"));
    }

    #[test]
    fn a_two_slot_board_supports_ab() {
        let layout = layout();

        assert!(layout.supports_ab());
        assert!(layout.missing_for_ab().is_empty());
        assert_eq!(layout.slot1().map(|slot| slot.address), Some(0x1e0000));
    }

    #[test]
    fn a_board_without_a_second_slot_names_what_it_lacks() {
        let single = DTS.replace("slot1_partition", "unused_partition");
        let layout = FlashLayout::read(&devicetree::parse(&single));

        assert!(!layout.supports_ab());
        assert_eq!(layout.missing_for_ab(), vec!["slot1_partition"]);
    }

    #[test]
    fn partitions_outside_the_partitions_node_are_not_collected() {
        let layout = layout();

        // Only the four declared above, and nothing from the flash
        // controller or the SoC around them.
        assert_eq!(layout.partitions.len(), 4);
        assert!(layout.node("storage_partition").is_some());
    }

    #[test]
    fn an_unreadable_reg_is_skipped_rather_than_fatal() {
        let broken = DTS.replace("reg = < 0x0 0x10000 >", "reg = < >");
        let layout = FlashLayout::read(&devicetree::parse(&broken));

        assert!(layout.boot().is_none());
        assert!(layout.slot0().is_some(), "the rest still reads");
        assert_eq!(layout.missing_for_ab(), vec!["boot_partition"]);
    }
}
