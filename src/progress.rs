//! Parses progress out of build/flash tool output, so the state line can
//! show something better than a stopwatch (item 05 of the 2026-08-20 UX
//! audit --- `SPEC.md` §4.6's "fast feedback" principle). Both shapes here
//! are already emitted by the exact tools ChipTUI drives, unprompted: ninja
//! streams a `[123/456]` step counter ahead of most build lines, and
//! esptool a `Writing at 0x... (NN %)` progress line while it flashes.
//! Nothing here spawns or reads a process; it only classifies a line
//! [`crate::build::BuildPanel`]/[`crate::flash::FlashPanel`] already have.

/// One line's worth of progress, in whichever shape its tool reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Ninja's step counter (`[123/456]`).
    Steps { done: u32, total: u32 },
    /// esptool's write percentage (`(37 %)`), 0..=100.
    Percent(u8),
}

impl Progress {
    /// `"123/456"` or `"37%"`, for the state line.
    pub fn render(self) -> String {
        match self {
            Self::Steps { done, total } => format!("{done}/{total}"),
            Self::Percent(percent) => format!("{percent}%"),
        }
    }
}

/// Tries every known shape against one line of tool output. `None` is the
/// common case --- most lines (a compiler invocation, a cmake status
/// message, esptool's chip banner) carry no progress at all, and that is
/// not an error.
pub fn detect(line: &str) -> Option<Progress> {
    ninja_step(line).or_else(|| esptool_percent(line))
}

/// Ninja's own step counter, at the start of most of its lines (`[123/456]
/// Building C object ...`) --- `west build` streams ninja's stdout straight
/// through, so this is exactly what a Zephyr build's Monitor output carries.
fn ninja_step(line: &str) -> Option<Progress> {
    let rest = line.strip_prefix('[')?;
    let (counts, _) = rest.split_once(']')?;
    let (done, total) = counts.split_once('/')?;
    let done: u32 = done.trim().parse().ok()?;
    let total: u32 = total.trim().parse().ok()?;
    (total > 0 && done <= total).then_some(Progress::Steps { done, total })
}

/// esptool's write-flash progress (`Writing at 0x00001000... (37 %)`).
/// esptool draws these with a bare `\r` between updates rather than a
/// trailing `\n`, so each one arrives as its own [`crate::process::ProcessEvent::Line`]
/// (see `tests/fixtures/bin/progress`, which exists to prove exactly that
/// streaming) --- this only has to read one already-split line.
fn esptool_percent(line: &str) -> Option<Progress> {
    let (_, tail) = line.rsplit_once('(')?;
    let digits = tail.strip_suffix("%)")?.trim();
    let percent: u8 = digits.parse().ok()?;
    (percent <= 100).then_some(Progress::Percent(percent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ninja_lines_report_the_step_counter() {
        assert_eq!(
            detect("[45/321] Building C object CMakeFiles/app.dir/main.c.obj"),
            Some(Progress::Steps {
                done: 45,
                total: 321
            })
        );
        assert_eq!(
            detect("[1/1] Linking C executable zephyr.elf"),
            Some(Progress::Steps { done: 1, total: 1 })
        );
    }

    #[test]
    fn esptool_lines_report_the_percentage() {
        assert_eq!(
            detect("Writing at 0x00001000... (10 %)"),
            Some(Progress::Percent(10))
        );
        assert_eq!(
            detect("Writing at 0x00050000... (100 %)"),
            Some(Progress::Percent(100))
        );
        assert_eq!(
            detect("Verifying 0x1000 (100 %)"),
            Some(Progress::Percent(100))
        );
    }

    #[test]
    fn unrelated_lines_report_nothing() {
        for line in [
            "-- Configuring done",
            "esptool v5.3.1",
            "Chip is ESP32-D0WD (revision 3)",
            "ninja: build stopped: subcommand failed.",
            "",
        ] {
            assert_eq!(detect(line), None, "{line:?} should not parse");
        }
    }

    #[test]
    fn a_malformed_bracket_does_not_parse_as_steps() {
        for line in [
            "[ 33%] Building C object foo.c.o",
            "[abc/def] nonsense",
            "[5/0] impossible",
        ] {
            assert_eq!(detect(line), None, "{line:?} should not parse");
        }
    }

    #[test]
    fn render_matches_the_shape() {
        assert_eq!(
            Progress::Steps {
                done: 12,
                total: 34
            }
            .render(),
            "12/34"
        );
        assert_eq!(Progress::Percent(7).render(), "7%");
    }
}
