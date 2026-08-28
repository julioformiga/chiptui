//! `<build>/zephyr/zephyr.stat` --- the ELF's own headers, as text.
//!
//! Every Zephyr build runs `readelf -e` over the kernel ELF and saves the
//! output here (`KERNEL_STAT_NAME`, `cmake/modules/kernel.cmake`; the
//! invocation is in the top-level `CMakeLists.txt`'s post-build commands).
//! The file is the ELF Stats tab verbatim, and it is also where the Build
//! Summary's memory figures come from.
//!
//! **This is what spares the crate an ELF parser.** `dashboard.py` opens the
//! binary with `pyelftools` and sums section sizes by `sh_type` and
//! `sh_flags` (`elf_memory_summary`). The section header table in this file
//! carries exactly those two columns, so [`summary`] reproduces the same
//! five buckets from text --- no `object`/`goblin` dependency, and no second
//! reader for a file the build already wrote.
//!
//! The table is parsed structurally rather than by column offsets, because
//! `readelf -e` has two layouts and Zephyr passes no `-W`: a 32-bit target
//! prints one line per section, a 64-bit one splits each section across two.
//! Both are read here, since a Zephyr workspace builds for both (an ESP32-C3
//! is 32-bit; an ARM64 board or `native_sim` is not).

/// One row of the `Section Headers:` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub index: usize,
    /// The section's name. Empty for the mandatory null section, and
    /// possibly elided by readelf itself (`.note.gnu.bu[...]`), which
    /// truncates long names when not asked for wide output.
    pub name: String,
    /// The type column, e.g. `PROGBITS`, `NOBITS`, `SYMTAB`. readelf clips
    /// this to 15 columns too (`RISCV_ATTRIBUTES` prints as
    /// `RISCV_ATTRIBUTE`), so it is compared by prefix where it matters.
    pub kind: String,
    /// The flag letters, e.g. `AX`, `WA`, or empty. `W` is
    /// `SHF_WRITE`, `A` is `SHF_ALLOC`, `X` is `SHF_EXECINSTR`.
    pub flags: String,
    pub addr: u64,
    pub size: u64,
}

impl Section {
    pub fn writable(&self) -> bool {
        self.flags.contains('W')
    }

    pub fn allocated(&self) -> bool {
        self.flags.contains('A')
    }

    pub fn executable(&self) -> bool {
        self.flags.contains('X')
    }
}

/// The five buckets the Build Summary shows, in bytes.
///
/// The names and the rules are `dashboard.py`'s, kept identical so the two
/// dashboards never disagree about the same build: `NOBITS` is bss whatever
/// its flags; a `PROGBITS` section is text if executable, else read-write
/// data if writable, else read-only data if allocated, else other; and every
/// other section type is other.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemorySummary {
    pub text: u64,
    pub rodata: u64,
    pub rwdata: u64,
    pub bss: u64,
    pub other: u64,
}

impl MemorySummary {
    /// Every bucket summed --- the denominator the Summary's bars use.
    pub fn total(&self) -> u64 {
        self.text + self.rodata + self.rwdata + self.bss + self.other
    }

    /// The buckets in the order the Summary lists them, each with its label.
    pub fn rows(&self) -> [(&'static str, u64); 5] {
        [
            ("text", self.text),
            ("rodata", self.rodata),
            ("rwdata", self.rwdata),
            ("bss", self.bss),
            ("other", self.other),
        ]
    }
}

/// Sums the section table into the Summary's buckets.
pub fn summary(sections: &[Section]) -> MemorySummary {
    let mut summary = MemorySummary::default();
    for section in sections {
        let bucket = if section.kind == "NOBITS" {
            &mut summary.bss
        } else if section.kind == "PROGBITS" {
            if section.executable() {
                &mut summary.text
            } else if section.writable() {
                &mut summary.rwdata
            } else if section.allocated() {
                &mut summary.rodata
            } else {
                &mut summary.other
            }
        } else {
            &mut summary.other
        };
        *bucket += section.size;
    }
    summary
}

/// A labelled line from the `ELF Header:` block, e.g. `Machine` or
/// `Entry point address`. The block is `  Label:` followed by the value,
/// padded; the label match is exact so `Version` does not answer for
/// `ABI Version`.
pub fn header_field(text: &str, label: &str) -> Option<String> {
    let needle = format!("{label}:");
    let mut lines = text
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("ELF Header:"))
        .skip(1);
    lines.find_map(|line| {
        let trimmed = line.trim_start();
        // The block ends at the blank line before `Section Headers:`;
        // stopping there is what keeps a same-named field further down the
        // file (the program headers repeat `Type` and `Flg`) from answering.
        if trimmed.is_empty() {
            return Some(None);
        }
        let rest = trimmed.strip_prefix(needle.as_str())?;
        let value = rest.trim();
        Some((!value.is_empty()).then(|| value.to_string()))
    })?
}

/// Reads the `Section Headers:` table.
///
/// Returns an empty vector for a file that has no such table --- a missing,
/// truncated or foreign `zephyr.stat` --- which the Summary shows as zeroed
/// figures rather than an error, the same way a missing `build_info.yml`
/// shows as empty rows.
pub fn parse(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut lines = text
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("Section Headers:"))
        .skip(1);
    // `next()` is called a second time inside the body for the 64-bit
    // layout's continuation line, so this cannot be a `for`.
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        // The table ends at `Key to Flags:` / a blank line before the
        // program headers. Anything that is not a `[nn]` row after the table
        // started is past its end.
        if trimmed.starts_with("Key to Flags:") {
            break;
        }
        let Some(row) = split_row(trimmed) else {
            continue;
        };
        let (index, tail) = row;
        // The name column may be empty (the null section). Whatever is not
        // the type is the name, and the type is the first token that starts
        // with an ASCII capital --- section names start with `.`, `_` or a
        // lowercase letter, type names never do.
        let (name, kind, fields) = match tail.split_first() {
            Some((first, rest)) if looks_like_type(first) => (String::new(), *first, rest),
            Some((first, rest)) => match rest.split_first() {
                Some((second, rest)) => ((*first).to_string(), *second, rest),
                None => continue,
            },
            None => continue,
        };

        // One layout or the other. 32-bit prints
        // `Addr Off Size ES [Flg] Lk Inf Al` on the same line; 64-bit prints
        // `Address Offset` here and `Size EntSize [Flags] Link Info Align`
        // on the next.
        let (addr, size, flags) = if fields.len() >= 7 {
            let flags = if fields.len() >= 8 { fields[4] } else { "" };
            (hex(fields[0]), hex(fields[2]), flags)
        } else if fields.len() == 2 {
            let Some(continuation) = lines.next() else {
                break;
            };
            let second: Vec<&str> = continuation.split_whitespace().collect();
            if second.len() < 5 {
                continue;
            }
            let flags = if second.len() >= 6 { second[2] } else { "" };
            (hex(fields[0]), hex(second[0]), flags)
        } else {
            continue;
        };

        sections.push(Section {
            index,
            name,
            kind: kind.to_string(),
            flags: flags.to_string(),
            addr,
            size,
        });
    }
    sections
}

/// Splits `[ 7] .iram0.text PROGBITS …` into its index and the rest as
/// whitespace tokens. `None` for any line that is not such a row.
fn split_row(trimmed: &str) -> Option<(usize, Vec<&str>)> {
    let rest = trimmed.strip_prefix('[')?;
    let (number, rest) = rest.split_once(']')?;
    let index = number.trim().parse::<usize>().ok()?;
    Some((index, rest.split_whitespace().collect()))
}

/// Whether a token is the type column rather than a section name.
fn looks_like_type(token: &str) -> bool {
    token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// The table's numbers are bare hex with no `0x`. An unreadable field is 0
/// rather than a failure: one odd row must not cost the whole summary.
fn hex(token: &str) -> u64 {
    u64::from_str_radix(token.trim_start_matches("0x"), 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut from a real `zephyr.stat` (RISC-V, ESP32-C3 --- the one-line
    /// 32-bit layout). It keeps every shape the reader must survive: the
    /// null section with no name, a `PROGBITS` that is writable but not
    /// allocated, executable and read-only sections, a `NOBITS`, a section
    /// whose flags column is empty while later columns are not
    /// (`.symtab`, whose Link is 64), and the `Key to Flags:` block that
    /// ends the table. Three rows are copied verbatim because the real build
    /// proved each one load-bearing: `.text` is `WAX`, not `AX`, so the flag
    /// tests must try execute before write; `.flash.rodat[...]` is how
    /// readelf elides a name past 17 columns without `-W`; and
    /// `.debug_info` is a `PROGBITS` with no flags at all.
    const RISCV32: &str = "\
ELF Header:
  Magic:   7f 45 4c 46 01 01 01 00 00 00 00 00 00 00 00 00
  Class:                             ELF32
  Data:                              2's complement, little endian
  Version:                           1 (current)
  OS/ABI:                            UNIX - System V
  ABI Version:                       0
  Type:                              EXEC (Executable file)
  Machine:                           RISC-V
  Entry point address:               0x40380260
  Number of section headers:         66

Section Headers:
  [Nr] Name              Type            Addr     Off    Size   ES Flg Lk Inf Al
  [ 0]                   NULL            00000000 000000 000000 00      0   0  0
  [ 1] .rtc.text         PROGBITS        50000000 134d94 000000 00   W  0   0  1
  [ 5] .rtc.force_slow   PROGBITS        50000000 0001f4 000024 00  WA  0   0  4
  [ 7] .iram0.text       PROGBITS        40380000 000300 00fd58 00  AX  0   0 256
  [12] .dram0.dummy      NOBITS          3fc80000 134d94 010640 00  WA  0   0  1
  [14] .loader.data      PROGBITS        3fc93be0 013ed0 000320 00   A  0   0  4
  [31] .text             PROGBITS        42000000 020000 0d1800 00 WAX  0   0 65536
  [33] .flash.rodat[...] NOBITS          3c000000 134d94 0e0000 00  WA  0   0  1
  [54] .debug_info       PROGBITS        00000000 142000 3e9fce 00      0   0  1
  [62] .riscv.attributes RISCV_ATTRIBUTE 00000000 a6497d 000048 00      0   0  1
  [63] .symtab           SYMTAB          00000000 a649c8 02ebf0 10     64 5622  4
Key to Flags:
  W (write), A (alloc), X (execute), M (merge), S (strings), I (info),

Program Headers:
  Type           Offset   VirtAddr   PhysAddr   FileSiz MemSiz  Flg Align
  LOAD           0x000300 0x40380000 0x00000024 0x10630 0x10630 R E 0x100
";

    /// The same table in `readelf -e`'s 64-bit layout, which splits every
    /// section across two lines --- what an ARM64 board or `native_sim`
    /// build writes. Zephyr passes no `-W`, so this form is not optional,
    /// and readelf elides long names (`.note.gnu.bu[...]`) to keep the
    /// columns.
    const ELF64: &str = "\
ELF Header:
  Class:                             ELF64
  Machine:                           AArch64
  Entry point address:               0x40000000

Section Headers:
  [Nr] Name              Type             Address           Offset
       Size              EntSize          Flags  Link  Info  Align
  [ 0]                   NULL             0000000000000000  00000000
       0000000000000000  0000000000000000           0     0     0
  [ 1] .note.gnu.bu[...] NOTE             0000000000000388  00000388
       0000000000000024  0000000000000000   A       0     0     4
  [ 2] .text             PROGBITS         0000000040000000  00010000
       0000000000001200  0000000000000000  AX       0     0    64
  [ 3] .bss              NOBITS           0000000041000000  00020000
       0000000000000800  0000000000000000  WA       0     0     8
Key to Flags:
  W (write), A (alloc), X (execute)
";

    #[test]
    fn the_thirty_two_bit_table_is_read() {
        let sections = parse(RISCV32);
        assert_eq!(sections.len(), 11, "one row per `[nn]` line, table only");
        assert_eq!(sections[0].index, 0);
        assert_eq!(sections[0].name, "");
        assert_eq!(sections[0].kind, "NULL");
        assert_eq!(sections[0].flags, "");

        let text = &sections[3];
        assert_eq!(text.name, ".iram0.text");
        assert!(
            sections
                .iter()
                .any(|section| section.name == ".flash.rodat[...]"),
            "an elided name is kept verbatim"
        );
        assert_eq!(text.kind, "PROGBITS");
        assert_eq!(text.flags, "AX");
        assert_eq!(text.addr, 0x4038_0000);
        assert_eq!(text.size, 0xfd58);
        assert!(text.executable() && text.allocated() && !text.writable());
    }

    /// `.symtab`'s flags column is empty while its Link column holds 64:
    /// a reader that took "the token after EntSize" as the flags would read
    /// `64` as flag letters and lose the size of everything after it.
    #[test]
    fn an_empty_flags_column_is_not_confused_with_the_next_one() {
        let sections = parse(RISCV32);
        let symtab = sections
            .iter()
            .find(|section| section.name == ".symtab")
            .expect("symtab row");
        assert_eq!(symtab.flags, "");
        assert_eq!(symtab.size, 0x2_ebf0);
    }

    #[test]
    fn the_sixty_four_bit_two_line_layout_is_read() {
        let sections = parse(ELF64);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].kind, "NULL");
        assert_eq!(sections[0].size, 0);
        assert_eq!(sections[1].name, ".note.gnu.bu[...]");
        assert_eq!(sections[1].flags, "A");
        assert_eq!(sections[1].size, 0x24);
        assert_eq!(sections[2].name, ".text");
        assert_eq!(sections[2].flags, "AX");
        assert_eq!(sections[2].size, 0x1200);
        assert_eq!(sections[2].addr, 0x4000_0000);
        assert_eq!(sections[3].kind, "NOBITS");
        assert_eq!(sections[3].size, 0x800);
    }

    /// The five buckets are `dashboard.py::elf_memory_summary`'s, rule for
    /// rule --- the two dashboards must never disagree about one build.
    #[test]
    fn the_summary_reproduces_the_python_buckets() {
        let summary = summary(&parse(RISCV32));
        // .iram0.text (AX) + .text (WAX): executable, whatever else it is.
        assert_eq!(summary.text, 0xfd58 + 0xd_1800);
        // .loader.data, allocated and neither writable nor executable.
        assert_eq!(summary.rodata, 0x320);
        // .rtc.text (0) + .rtc.force_slow (0x24), writable PROGBITS.
        assert_eq!(summary.rwdata, 0x24);
        // .dram0.dummy + .flash.rodat[...] --- NOBITS counts as bss whatever
        // its flags say, including the `WA` both of these carry.
        assert_eq!(summary.bss, 0x1_0640 + 0xe_0000);
        // NULL + a flagless .debug_info + RISCV_ATTRIBUTE + SYMTAB.
        assert_eq!(summary.other, 0x3e_9fce + 0x48 + 0x2_ebf0);
        assert_eq!(
            summary.total(),
            0xfd58 + 0xd_1800 + 0x320 + 0x24 + 0x1_0640 + 0xe_0000 + 0x3e_9fce + 0x48 + 0x2_ebf0
        );
    }

    /// The real `.text` of an ESP32-C3 build is `WAX`, not `AX`: testing
    /// writability before executability files the whole text segment as
    /// rwdata, and the numbers still look plausible. `dashboard.py` tests
    /// `SHF_EXECINSTR` first, and so does this.
    #[test]
    fn a_writable_executable_section_is_text_not_rwdata() {
        let sections = parse(RISCV32);
        let text = sections
            .iter()
            .find(|section| section.name == ".text")
            .expect("the .text row");
        assert_eq!(text.flags, "WAX");
        assert!(text.writable() && text.executable());
        assert_eq!(summary(std::slice::from_ref(text)).text, 0xd_1800);
        assert_eq!(summary(std::slice::from_ref(text)).rwdata, 0);
    }

    /// A writable `PROGBITS` that is *not* allocated is still rwdata, and an
    /// unallocated, unwritable, unexecutable one falls to `other` --- the
    /// branch order matters and is easy to get backwards.
    #[test]
    fn progbits_are_bucketed_in_the_python_branch_order() {
        let sections = vec![
            Section {
                index: 1,
                name: ".debug_info".into(),
                kind: "PROGBITS".into(),
                flags: String::new(),
                addr: 0,
                size: 100,
            },
            Section {
                index: 2,
                name: ".both".into(),
                kind: "PROGBITS".into(),
                // Executable wins over writable, as in the Python.
                flags: "WAX".into(),
                addr: 0,
                size: 8,
            },
        ];
        let summary = summary(&sections);
        assert_eq!(summary.other, 100);
        assert_eq!(summary.text, 8);
        assert_eq!(summary.rwdata, 0);
    }

    #[test]
    fn the_elf_header_block_answers_its_labels() {
        assert_eq!(header_field(RISCV32, "Machine").as_deref(), Some("RISC-V"));
        assert_eq!(header_field(RISCV32, "Class").as_deref(), Some("ELF32"));
        assert_eq!(
            header_field(RISCV32, "Entry point address").as_deref(),
            Some("0x40380260")
        );
        assert_eq!(header_field(ELF64, "Machine").as_deref(), Some("AArch64"));
    }

    /// `ABI Version` must not answer for `Version`, and the program
    /// headers' own `Type` column must not answer for the header's.
    #[test]
    fn header_labels_match_exactly_and_stop_at_the_block() {
        assert_eq!(
            header_field(RISCV32, "Version").as_deref(),
            Some("1 (current)")
        );
        assert_eq!(
            header_field(RISCV32, "Type").as_deref(),
            Some("EXEC (Executable file)")
        );
        assert_eq!(header_field(RISCV32, "Align"), None);
    }

    #[test]
    fn a_missing_or_truncated_file_yields_no_sections() {
        for text in ["", "ELF Header:\n  Class: ELF32\n", "garbage\n"] {
            assert!(parse(text).is_empty(), "expected nothing for {text:?}");
            assert_eq!(summary(&parse(text)), MemorySummary::default());
        }
    }

    /// A build killed while readelf was writing leaves the table cut in the
    /// middle --- including, for a 64-bit target, between a section's two
    /// lines. What did land must still count.
    #[test]
    fn a_table_cut_mid_section_keeps_the_rows_that_completed() {
        let cut = ELF64
            .split_once("  [ 3] .bss")
            .expect("has the row")
            .0
            .to_string()
            + "  [ 3] .bss             NOBITS           0000000041000000  00020000\n";
        let sections = parse(&cut);
        assert_eq!(sections.len(), 3, "the half-written row is dropped");
        assert_eq!(sections[2].name, ".text");
    }
}
