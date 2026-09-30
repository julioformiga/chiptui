//! Parses progress out of build/flash tool output, so the state line can
//! show something better than a stopwatch (item 05 of the 2026-08-20 UX
//! audit --- `SPEC.md` §4.6's "fast feedback" principle). Both shapes here
//! are already emitted by the exact tools ChipTUI drives, unprompted: ninja
//! streams a `[123/456]` step counter ahead of most build lines, and
//! esptool a `Writing at 0x... (NN %)` progress line while it flashes ---
//! and, from v5 on (via esp-pylib), a `Writing at 0x... ━ 94.5% 384kB/406kB
//! [5s]` bar. On a terminal both redraw in place with a bare `\r`; piped,
//! which is how ChipTUI runs every tool, v5 prints one full
//! newline-terminated line per update instead, so recognizing the bar is
//! also what lets the panels redraw it in place (see [`is_bar_line`]).
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
    ninja_step(line)
        .or_else(|| esptool_bar_percent(line))
        .or_else(|| esptool_percent(line))
}

/// Whether *line* is one update of esptool v5's piped progress bar
/// ([`esptool_bar_percent`]). Those updates arrive newline-terminated ---
/// unlike the `\r` redraws the panels already collapse --- yet they all
/// draw the same bar, so a panel uses this to replace its previous bar row
/// in place instead of stacking a new line per update.
pub fn is_bar_line(line: &str) -> bool {
    esptool_bar_percent(line).is_some()
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

/// esptool v5's bar, as esp-pylib prints it when stdout is *not* a terminal
/// --- exactly what ChipTUI's panels see, because `west flash` and `esptool`
/// run piped:
///
/// ```text
/// Writing at 0x00060000 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━    94.5% 384.00kB/406.39kB [5s]
/// ```
///
/// On an interactive terminal esp-pylib redraws this bar in place with a
/// bare `\r` (the pump splits those, and the old [`esptool_percent`] shape
/// still covers an ASCII `%`-in-parens fallback), but piped it prints one
/// full newline-terminated line per update --- so the percentage has to be
/// read from a plain line, and [`is_bar_line`] tells the panels to redraw
/// it in place.
///
/// The elapsed tail (`[5s]`, `[2:07]`) is what marks a line as a live bar
/// rather than ordinary output that happens to carry a percentage; the
/// bar's M/N suffix (`384.00kB/406.39kB`) sits between the percentage and
/// that tail, and its `/` is part of the same guard.
fn esptool_bar_percent(line: &str) -> Option<Progress> {
    let rest = line.trim_end().strip_suffix(']')?;
    let (counts, elapsed) = rest.rsplit_once('[')?;
    if !is_elapsed(elapsed.trim()) {
        return None;
    }
    let (percent, counts) = counts.trim_end().rsplit_once('%')?;
    if !counts.trim().contains('/') {
        return None;
    }
    let percent = percent.trim_end();
    let start = percent
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_ascii_digit() || c == '.')
        .map(|(i, _)| i)
        .last()?;
    let value: f32 = percent[start..].parse().ok()?;
    (value <= 100.0).then(|| Progress::Percent(value.round() as u8))
}

/// esp-pylib's elapsed tail: `5s`, `45s`, `2:07`.
fn is_elapsed(s: &str) -> bool {
    if let Some(secs) = s.strip_suffix('s') {
        return !secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit());
    }
    match s.split_once(':') {
        Some((mins, secs)) => {
            !mins.is_empty()
                && mins.bytes().all(|b| b.is_ascii_digit())
                && secs.len() == 2
                && secs.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
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
    fn esptool_v5_piped_bar_lines_report_the_percentage() {
        // The exact line the user's terminal showed, copied verbatim.
        assert_eq!(
            detect(
                "Writing at 0x00060000 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━    94.5% 384.00kB/406.39kB [5s]"
            ),
            Some(Progress::Percent(95))
        );
        // The ASCII fallback esp-pylib renders when the console cannot draw
        // `━` (esp-pylib's `_render_plain_bar`).
        assert_eq!(
            detect("Writing at 0x00001000 ==================  50.0% 8.00kB/16.00kB [1s]"),
            Some(Progress::Percent(50))
        );
        // Verify runs the same bar under a different prefix, and minutes
        // render as `M:SS`.
        assert_eq!(
            detect("Verifying 0x00001000 ━━━━━━━━━━━━━━━ 100.0% 406.39kB/406.39kB [2:07]"),
            Some(Progress::Percent(100))
        );
        // esp-pylib ends the suffix with a space (`{percent}%{counts} `).
        assert_eq!(
            detect("Writing at 0x00001000 ━━━ 7.5% 1.00kB/13.00kB [0s] "),
            Some(Progress::Percent(8))
        );
        assert!(is_bar_line(
            "Writing at 0x00060000 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━    94.5% 384.00kB/406.39kB [5s]"
        ));
    }

    #[test]
    fn ordinary_lines_do_not_parse_as_the_v5_bar() {
        for line in [
            // A percentage and a bracket are not enough --- the M/N suffix
            // between them is part of the shape.
            "Done 50% [5s]",
            "Copied 3 of 5 files [12s]",
            "Hash of data verified.",
            "Wrote 406.39kB (compressed 289.11kB) at 0x00060000 in 5.4 seconds",
            "",
        ] {
            assert_eq!(detect(line), None, "{line:?} should not parse");
        }
        for line in [
            // The old v4 shape: no elapsed tail, no M/N suffix. It still
            // parses (as the parenthesised percentage), but it is not a
            // newline-terminated bar the panels would redraw in place.
            "Writing at 0x00001000... (10 %)",
            // cmake's percentage is bracketed, and the line does not end in
            // an elapsed tail.
            "[ 33%] Building C object foo.c.o",
        ] {
            assert!(
                !is_bar_line(line),
                "{line:?} must not count as a bar update"
            );
        }
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
