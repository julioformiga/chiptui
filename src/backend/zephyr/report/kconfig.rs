//! `<build>/zephyr/.config-trace.json` --- every Kconfig symbol with the
//! reason it holds the value it holds, and `.config` as the fallback.
//!
//! `scripts/kconfig/kconfig.py` writes this file on every build, beside the
//! pickle `dashboard.py` reads (`collect_trace_data`, dumped to both
//! `<out>-trace.pickle` and `<out>-trace.json` unconditionally). It is a JSON
//! array of 6-tuples, and the tuple's layout is the script's own docstring:
//!
//! ```text
//! (name, visibility, type, value, kind, location)
//! ```
//!
//! Three of those need care:
//!
//! * **`visibility` is a tristate string** (`n`/`m`/`y`), not a boolean ---
//!   `TRI_TO_STR[item.visibility]`. `dashboard.py` reduces it to
//!   `visible == 'y'`, which [`KconfigSymbol::visible`] reproduces, but the
//!   raw answer is kept so a `m` is not silently reported as invisible.
//! * **`value` is `null` for an unset symbol** (`value = None if kind ==
//!   "unset" else item.str_value`), which is why it is an `Option` rather
//!   than an empty string --- "no value" and "the empty string" are
//!   different answers for a `string` symbol.
//! * **`location` has four shapes**, decided by `kind`: `[file, line]` for
//!   `assign` and `default`, `null` for `unset` and for an implicit default,
//!   and a list of *Kconfig expression strings* --- not a location at all ---
//!   for `select` and `imply`. [`Source`] is that polymorphism made
//!   explicit, so the details pane never has to guess which one it holds.
//!
//! On the real project this file is 415 KB and 2190 entries, every one of
//! them a 6-tuple, with no duplicate names.
//!
//! The fallback is `.config` itself, which every build writes and which
//! older ones are all that have. It answers the names and values and nothing
//! else --- [`Source::NotRecorded`] says so rather than inventing an origin.

use super::json::Json;

/// A Kconfig file and the line inside it that decided a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: u64,
}

/// Where a symbol's value came from.
///
/// `assign` and `default` are the two kinds that carry a location --- and
/// both carry it *optionally*. That is not symmetry for its own sake: on the
/// real project 64 of the 1438 `assign` entries have no location at all
/// (`CONFIG_LV_COLOR_DEPTH_16`, for one --- a value settled by a choice
/// rather than by a line in a file), while every one of the 494 `default`
/// entries has one. An earlier shape here assumed the opposite, and quietly
/// relabelled those 64 assignments as defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Set by a `.config`/defconfig/fragment line. `None` when the trace
    /// records no line for it.
    Assign(Option<Location>),
    /// Took a `default`. `None` means no Kconfig line states it --- the
    /// type's own zero, which `dashboard.py` calls `(implicit)`.
    Default(Option<Location>),
    /// Forced on by other symbols; each string is a Kconfig expression, and
    /// several of them mean "any of these".
    Select(Vec<String>),
    /// Suggested by other symbols, same shape as [`Self::Select`].
    Imply(Vec<String>),
    /// Has no value in this build.
    Unset,
    /// A `kind` this reader does not know. Kept whole rather than dropped:
    /// a future Kconfig gaining an origin must show up as itself, not as a
    /// missing row.
    Other(String),
    /// Read from `.config`, which records no origin at all.
    NotRecorded,
}

impl Source {
    /// The one-word label, matching `dashboard.py`'s own badges
    /// (`KconfigSymbol.src_html`) so the two dashboards read the same.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Assign(_) => "assigned",
            Self::Default(_) => "default",
            Self::Select(_) => "selected",
            Self::Imply(_) => "implied",
            Self::Unset => "unset",
            Self::Other(_) => "other",
            Self::NotRecorded => "not recorded",
        }
    }

    /// The file and line, for the two kinds that have one.
    pub fn location(&self) -> Option<&Location> {
        match self {
            Self::Assign(location) | Self::Default(location) => location.as_ref(),
            _ => None,
        }
    }

    /// The Kconfig expressions, for the two kinds that carry them.
    pub fn expressions(&self) -> &[String] {
        match self {
            Self::Select(items) | Self::Imply(items) => items.as_slice(),
            _ => &[],
        }
    }
}

/// One symbol as the build evaluated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KconfigSymbol {
    /// The full name, `CONFIG_` prefix included --- `kconfig.py` writes
    /// `kconf.config_prefix + item.name`, so the rows read the way the
    /// symbol appears in `prj.conf`.
    pub name: String,
    /// The tristate visibility as written: `n`, `m` or `y`.
    pub visibility: String,
    /// The type as written: `bool`, `tristate`, `int`, `hex`, `string`, or
    /// `unknown`.
    pub kind: String,
    pub value: Option<String>,
    pub source: Source,
}

impl KconfigSymbol {
    /// Whether the symbol has a prompt the user could answer in
    /// `menuconfig` --- `dashboard.py`'s `bool(visible == 'y')`.
    pub fn visible(&self) -> bool {
        self.visibility == "y"
    }

    /// The value as the pane shows it: a `string` symbol's value is quoted,
    /// which is how it must be written in a fragment, and an unset symbol
    /// shows nothing at all.
    pub fn display_value(&self) -> String {
        match &self.value {
            None => String::new(),
            Some(value) if self.kind == "string" => format!("\"{value}\""),
            Some(value) => value.clone(),
        }
    }
}

/// Reads `.config-trace.json`. `None` when the text is not the expected
/// array of tuples --- the caller then falls back to [`parse_config`], which
/// is what an older build directory needs anyway.
pub fn parse_trace(text: &str) -> Option<Vec<KconfigSymbol>> {
    let rows = super::json::parse(text)?;
    let rows = rows.as_array()?;
    let mut symbols: Vec<KconfigSymbol> = rows.iter().filter_map(read_row).collect();
    // `kconfig.py` emits definition order; the pane lists alphabetically,
    // the order `dashboard.py` sorts into as well.
    symbols.sort_by(|a, b| a.name.cmp(&b.name));
    Some(symbols)
}

/// One 6-tuple. A row that is not one is skipped rather than fatal: the
/// tolerance every parser in this crate keeps.
fn read_row(row: &Json) -> Option<KconfigSymbol> {
    let fields = row.as_array()?;
    if fields.len() < 6 {
        return None;
    }
    let name = fields[0].as_str()?.to_string();
    let kind = fields[2].as_str().unwrap_or("unknown").to_string();
    Some(KconfigSymbol {
        name,
        visibility: fields[1].as_str().unwrap_or("n").to_string(),
        kind,
        value: fields[3].as_str().map(str::to_string),
        source: read_source(fields[4].as_str().unwrap_or_default(), &fields[5]),
    })
}

fn read_source(kind: &str, loc: &Json) -> Source {
    match kind {
        "unset" => Source::Unset,
        "assign" => Source::Assign(location(loc)),
        "default" => Source::Default(location(loc)),
        "select" => Source::Select(expressions(loc)),
        "imply" => Source::Imply(expressions(loc)),
        other => Source::Other(other.to_string()),
    }
}

/// The `[file, line]` shape, `None` for `null` or the expression-list shape.
fn location(loc: &Json) -> Option<Location> {
    let parts = loc.as_array()?;
    let file = parts.first()?.as_str()?.to_string();
    let line = parts.get(1)?.as_u64()?;
    Some(Location { file, line })
}

/// The expression-list shape: every element that is a string.
fn expressions(loc: &Json) -> Vec<String> {
    loc.as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads a plain `.config`, the fallback for a build with no trace file.
///
/// Two line shapes carry a value: `CONFIG_X=value` and the commented-out
/// `# CONFIG_X is not set`, which is how Kconfig writes `n` (1012 of them in
/// the real project's file --- dropping them would hide most of what is
/// *off*). Everything else is a comment or blank.
///
/// The type is inferred from the value's own shape, because `.config`
/// records none. That is a guess and the pane says so, via
/// [`Source::NotRecorded`] on every row.
pub fn parse_config(text: &str) -> Vec<KconfigSymbol> {
    let mut symbols: Vec<KconfigSymbol> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("# ")
                && let Some(name) = rest.strip_suffix(" is not set")
            {
                return Some(symbol_from_config(name, "n"));
            }
            if line.starts_with('#') || line.is_empty() {
                return None;
            }
            let (name, value) = line.split_once('=')?;
            Some(symbol_from_config(name, value))
        })
        .collect();
    symbols.sort_by(|a, b| a.name.cmp(&b.name));
    symbols
}

fn symbol_from_config(name: &str, value: &str) -> KconfigSymbol {
    let quoted = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
    let kind = if quoted.is_some() {
        "string"
    } else if value.starts_with("0x") || value.starts_with("0X") {
        "hex"
    } else if matches!(value, "y" | "n" | "m") {
        "bool"
    } else if value.chars().all(|c| c.is_ascii_digit() || c == '-') {
        "int"
    } else {
        "unknown"
    };
    KconfigSymbol {
        name: name.trim().to_string(),
        // `.config` records no visibility either; `y` would claim the symbol
        // has a prompt, which is exactly what is not known here.
        visibility: "n".to_string(),
        kind: kind.to_string(),
        value: Some(quoted.unwrap_or(value).to_string()),
        source: Source::NotRecorded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows copied from the real `.config-trace.json` --- one per `kind`
    /// the build actually produced (`assign` 1438, `default` 494, `unset`
    /// 147, `select` 110, `imply` 1), which between them cover all four
    /// shapes the location field takes.
    const TRACE: &str = r#"[
  ["CONFIG_NET_IF_MAX_IPV4_COUNT","y","int","1","assign",
   ["/home/j/round-display/build/zephyr/.config",41]],
  ["CONFIG_DT_HAS_CHIPSEMI_CHSC6X_ENABLED","n","bool","y","default",
   ["/home/j/round-display/build/Kconfig/Kconfig.dts",2713]],
  ["CONFIG_LV_COLOR_DEPTH_32","y","bool",null,"unset",null],
  ["CONFIG_NET_L2_ETHERNET","y","bool","y","select",
   ["WIFI_ESP32 && DT_HAS_ESPRESSIF_ESP32_WIFI_ENABLED && !SMP && WIFI"]],
  ["CONFIG_COMMON_LIBC_TIME","n","bool","y","imply",
   ["PICOLIBC && PICOLIBC_SUPPORTED && <choice LIBC_IMPLEMENTATION>"]],
  ["CONFIG_BOARD","n","string","xiao_esp32c3","default",null],
  ["CONFIG_A_MANY_SELECTOR","y","bool","y","select",["FOO && BAR","BAZ"]]
]"#;

    fn find<'a>(symbols: &'a [KconfigSymbol], name: &str) -> &'a KconfigSymbol {
        symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    }

    #[test]
    fn every_kind_maps_to_its_own_source() {
        let symbols = parse_trace(TRACE).expect("parses");
        assert_eq!(symbols.len(), 7);
        assert_eq!(
            find(&symbols, "CONFIG_NET_IF_MAX_IPV4_COUNT").source,
            Source::Assign(Some(Location {
                file: "/home/j/round-display/build/zephyr/.config".into(),
                line: 41,
            }))
        );
        assert_eq!(
            find(&symbols, "CONFIG_DT_HAS_CHIPSEMI_CHSC6X_ENABLED").source,
            Source::Default(Some(Location {
                file: "/home/j/round-display/build/Kconfig/Kconfig.dts".into(),
                line: 2713,
            }))
        );
        assert_eq!(
            find(&symbols, "CONFIG_LV_COLOR_DEPTH_32").source,
            Source::Unset
        );
        assert_eq!(
            find(&symbols, "CONFIG_BOARD").source,
            Source::Default(None),
            "a default with no location is the implicit one"
        );
    }

    /// 64 of the real project's `assign` entries carry no location ---
    /// `CONFIG_LV_COLOR_DEPTH_16` among them. They are assignments with an
    /// unrecorded line, and calling them defaults would misreport where 64
    /// values came from.
    #[test]
    fn an_assignment_without_a_location_stays_an_assignment() {
        let symbols = parse_trace(r#"[["CONFIG_LV_COLOR_DEPTH_16","y","bool","y","assign",null]]"#)
            .expect("parses");
        assert_eq!(symbols[0].source, Source::Assign(None));
        assert_eq!(symbols[0].source.label(), "assigned");
        assert_eq!(symbols[0].source.location(), None);
    }

    /// `select`/`imply` put Kconfig *expressions* where the other kinds put
    /// a file and a line. Reading them as a location would show a symbol
    /// forced on by `WIFI_ESP32 && …` as living in a file of that name.
    #[test]
    fn select_and_imply_carry_expressions_not_locations() {
        let symbols = parse_trace(TRACE).expect("parses");
        assert_eq!(
            find(&symbols, "CONFIG_NET_L2_ETHERNET").source,
            Source::Select(vec![
                "WIFI_ESP32 && DT_HAS_ESPRESSIF_ESP32_WIFI_ENABLED && !SMP && WIFI".into()
            ])
        );
        assert_eq!(
            find(&symbols, "CONFIG_COMMON_LIBC_TIME").source,
            Source::Imply(vec![
                "PICOLIBC && PICOLIBC_SUPPORTED && <choice LIBC_IMPLEMENTATION>".into()
            ])
        );
        assert_eq!(
            find(&symbols, "CONFIG_A_MANY_SELECTOR").source,
            Source::Select(vec!["FOO && BAR".into(), "BAZ".into()]),
            "several expressions mean `any of these`, and all of them are kept"
        );
    }

    /// `value` is `null` only for `unset`, and that is a different answer
    /// from the empty string --- which a `string` symbol can legitimately
    /// hold.
    #[test]
    fn an_unset_symbol_has_no_value_at_all() {
        let symbols = parse_trace(TRACE).expect("parses");
        let unset = find(&symbols, "CONFIG_LV_COLOR_DEPTH_32");
        assert_eq!(unset.value, None);
        assert_eq!(unset.display_value(), "");
    }

    /// Visibility is a tristate, not a flag: `m` must not read as invisible
    /// and must survive to the details pane as itself.
    #[test]
    fn visibility_is_kept_as_the_tristate_it_is() {
        let symbols = parse_trace(TRACE).expect("parses");
        assert_eq!(find(&symbols, "CONFIG_NET_L2_ETHERNET").visibility, "y");
        assert!(find(&symbols, "CONFIG_NET_L2_ETHERNET").visible());
        assert_eq!(find(&symbols, "CONFIG_COMMON_LIBC_TIME").visibility, "n");
        assert!(!find(&symbols, "CONFIG_COMMON_LIBC_TIME").visible());

        let tristate =
            parse_trace(r#"[["CONFIG_M","m","tristate","m","assign",null]]"#).expect("parses");
        assert_eq!(tristate[0].visibility, "m");
        assert!(
            !tristate[0].visible(),
            "only `y` is a prompt, as in dashboard.py"
        );
    }

    /// A `string` value is shown quoted, the way it must be written in a
    /// fragment; every other type is shown bare.
    #[test]
    fn string_values_are_quoted_for_display_only() {
        let symbols = parse_trace(TRACE).expect("parses");
        let board = find(&symbols, "CONFIG_BOARD");
        assert_eq!(board.value.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(board.display_value(), "\"xiao_esp32c3\"");
        assert_eq!(
            find(&symbols, "CONFIG_NET_IF_MAX_IPV4_COUNT").display_value(),
            "1"
        );
    }

    #[test]
    fn rows_are_sorted_by_name() {
        let symbols = parse_trace(TRACE).expect("parses");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn a_malformed_row_is_skipped_not_fatal() {
        let symbols = parse_trace(
            r#"[["CONFIG_OK","y","bool","y","assign",null],
                ["CONFIG_SHORT","y"],
                [],
                ["CONFIG_ALSO_OK","y","bool","n","unset",null]]"#,
        )
        .expect("parses");
        assert_eq!(symbols.len(), 2);
    }

    /// The two accessors answer only for the kinds that have the thing.
    #[test]
    fn location_and_expressions_answer_for_their_own_kinds_only() {
        let symbols = parse_trace(TRACE).expect("parses");
        let assigned = find(&symbols, "CONFIG_NET_IF_MAX_IPV4_COUNT");
        assert_eq!(assigned.source.location().map(|l| l.line), Some(41));
        assert!(assigned.source.expressions().is_empty());

        let selected = find(&symbols, "CONFIG_A_MANY_SELECTOR");
        assert_eq!(selected.source.expressions().len(), 2);
        assert!(selected.source.location().is_none());

        assert!(Source::Unset.location().is_none());
        assert!(Source::Unset.expressions().is_empty());
    }

    /// An origin this reader does not know yet must surface as itself.
    #[test]
    fn an_unknown_kind_is_carried_through() {
        let symbols =
            parse_trace(r#"[["CONFIG_X","y","bool","y","conjured",null]]"#).expect("parses");
        assert_eq!(symbols[0].source, Source::Other("conjured".into()));
        assert_eq!(symbols[0].source.label(), "other");
    }

    #[test]
    fn a_file_that_is_not_the_trace_answers_none() {
        assert!(parse_trace("").is_none());
        assert!(parse_trace("{\"packages\": []}").is_none());
        assert!(parse_trace("[[").is_none());
    }

    // The real file opens with a blank line, then a comment banner.
    const CONFIG: &str = "
#
# Devicetree Info
#
CONFIG_DT_HAS_ESPRESSIF_ESP32_ADC_ENABLED=y
# CONFIG_LV_Z_FLUSH_THREAD is not set
CONFIG_KERNEL_ENTRY=\"__start\"
CONFIG_FLASH_BASE_ADDRESS=0x0
CONFIG_RISCV_MCAUSE_EXCEPTION_MASK=0x7FFFFFFF
CONFIG_NET_IF_MAX_IPV4_COUNT=1
CONFIG_SOMETHING_NEGATIVE=-1
";

    /// `# CONFIG_X is not set` is how Kconfig writes `n` --- 1012 of the
    /// real file's lines. Reading only `X=y` lines would hide most of what
    /// is switched off.
    #[test]
    fn the_config_fallback_reads_both_line_shapes() {
        let symbols = parse_config(CONFIG);
        assert_eq!(symbols.len(), 7);
        let off = find(&symbols, "CONFIG_LV_Z_FLUSH_THREAD");
        assert_eq!(off.value.as_deref(), Some("n"));
        assert_eq!(off.kind, "bool");
        assert_eq!(
            find(&symbols, "CONFIG_DT_HAS_ESPRESSIF_ESP32_ADC_ENABLED")
                .value
                .as_deref(),
            Some("y")
        );
    }

    #[test]
    fn the_config_fallback_infers_the_type_from_the_value() {
        let symbols = parse_config(CONFIG);
        assert_eq!(find(&symbols, "CONFIG_KERNEL_ENTRY").kind, "string");
        assert_eq!(
            find(&symbols, "CONFIG_KERNEL_ENTRY").value.as_deref(),
            Some("__start"),
            "the quotes are the file's, not the value's"
        );
        assert_eq!(
            find(&symbols, "CONFIG_KERNEL_ENTRY").display_value(),
            "\"__start\""
        );
        assert_eq!(find(&symbols, "CONFIG_FLASH_BASE_ADDRESS").kind, "hex");
        assert_eq!(
            find(&symbols, "CONFIG_RISCV_MCAUSE_EXCEPTION_MASK").kind,
            "hex"
        );
        assert_eq!(find(&symbols, "CONFIG_NET_IF_MAX_IPV4_COUNT").kind, "int");
        assert_eq!(find(&symbols, "CONFIG_SOMETHING_NEGATIVE").kind, "int");
    }

    /// The fallback knows names and values and nothing else; claiming an
    /// origin it cannot see would be worse than admitting it has none.
    #[test]
    fn the_config_fallback_never_invents_an_origin() {
        for symbol in parse_config(CONFIG) {
            assert_eq!(symbol.source, Source::NotRecorded);
            assert_eq!(symbol.source.label(), "not recorded");
            assert!(!symbol.visible());
        }
    }

    #[test]
    fn comments_and_blank_lines_are_not_symbols() {
        assert!(parse_config("#\n# Devicetree Info\n#\n\n").is_empty());
        assert!(parse_config("").is_empty());
    }
}
