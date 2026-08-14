//! VT-aware line console for streamed device output.
//!
//! A device REPL is not line-oriented: it echoes control sequences to redraw
//! the line being edited. MicroPython's readline answers a backspace with
//! `\b\x1b[K` (erase to end of line) and moves the cursor with `\x1b[nD`
//! after bigger jumps, so a renderer that appends every printable char shows
//! the escape bodies as literal `[K` garbage (`AGENTS.md` §6: interactive
//! sessions are not ordinary line-oriented output).
//!
//! [`LineConsole`] interprets just enough of VT100 for the display to match
//! what a terminal would show: cursor movement within the current line,
//! carriage return, backspace and erase-in-line. Everything else (SGR colors,
//! OSC titles, mode switches) is consumed silently rather than rendered,
//! because device output here is display-only.
//!
//! The escape parser keeps state across chunks: a sequence split over two PTY
//! reads is still recognized.

/// A line type [`LineConsole`] can edit: plain text for the monitor,
/// timestamped text for run output.
pub trait ConsoleLine: Sized {
    /// A new empty line. Called when output starts a line, so timestamped
    /// lines can stamp "now".
    fn blank() -> Self;
    fn text(&self) -> &str;
    fn text_mut(&mut self) -> &mut String;
}

impl ConsoleLine for String {
    fn blank() -> Self {
        String::new()
    }

    fn text(&self) -> &str {
        self
    }

    fn text_mut(&mut self) -> &mut String {
        self
    }
}

/// Escape-sequence parser state. Kept between chunks so a sequence split
/// across two PTY reads is still consumed as one unit.
#[derive(Debug, Default)]
enum Parse {
    #[default]
    Ground,
    /// Saw ESC; the next char decides the sequence type.
    Esc,
    /// CSI (`ESC [`): collecting parameter/intermediate chars until the
    /// final byte (0x40..=0x7e).
    Csi(String),
    /// OSC (`ESC ]`): consuming until BEL or ST.
    Osc,
    /// ESC inside an OSC: the sequence ends only if the next char is `\`.
    OscEsc,
    /// `ESC O`/`(`/`)`/`#`/`%`: exactly one more char follows.
    EscFinal,
}

/// Turns raw terminal output into an editable list of lines.
///
/// The console owns the *cursor* (a position inside the current, i.e. last,
/// line) and the parser state; the lines themselves stay in the caller's
/// `Vec` so rendering code keeps working with plain `Vec<String>` /
/// `Vec<RunLine>`.
#[derive(Debug, Default)]
pub struct LineConsole {
    parse: Parse,
    /// Cursor position in the current line, as a byte offset kept on a char
    /// boundary by construction.
    col: usize,
}

impl LineConsole {
    pub fn new() -> Self {
        Self::default()
    }

    /// Byte offset of the cursor within the current (last) line, always on a
    /// char boundary. Renderers use it to draw a text cursor; `0` after a
    /// newline or carriage return.
    pub fn cursor(&self) -> usize {
        self.col
    }

    /// Forgets any half-parsed sequence and cursor position. Call when the
    /// backing lines are cleared, so a stale escape cannot bleed into the
    /// next session's first line.
    pub fn reset(&mut self) {
        self.parse = Parse::Ground;
        self.col = 0;
    }

    /// Interprets one chunk of raw output into `lines`.
    pub fn feed<L: ConsoleLine>(&mut self, lines: &mut Vec<L>, chunk: &str) {
        if !chunk.is_empty() && lines.is_empty() {
            lines.push(L::blank());
        }
        for c in chunk.chars() {
            self.step(lines, c);
        }
    }

    /// Appends `text` as its own line (a banner such as `[monitor ok]`),
    /// starting fresh only when the current line has content, so a banner
    /// after a newline does not leave a blank line above it.
    pub fn push_line<L: ConsoleLine>(&mut self, lines: &mut Vec<L>, text: String) {
        if lines.is_empty() || lines.last().is_some_and(|line| !line.text().is_empty()) {
            lines.push(L::blank());
        }
        if let Some(last) = lines.last_mut() {
            *last.text_mut() = text;
        }
        self.col = lines.last().map_or(0, |line| line.text().len());
        self.parse = Parse::Ground;
    }

    fn step<L: ConsoleLine>(&mut self, lines: &mut Vec<L>, c: char) {
        self.parse = match std::mem::take(&mut self.parse) {
            Parse::Ground => match c {
                '\x1b' => Parse::Esc,
                '\n' => {
                    lines.push(L::blank());
                    self.col = 0;
                    Parse::Ground
                }
                '\r' => {
                    self.col = 0;
                    Parse::Ground
                }
                '\x08' => {
                    self.back(1, lines);
                    Parse::Ground
                }
                // DEL only exists on the input side of a terminal.
                '\x7f' => Parse::Ground,
                c if u32::from(c) < 0x20 => Parse::Ground,
                c => {
                    self.putc(lines, c);
                    Parse::Ground
                }
            },
            Parse::Esc => match c {
                '[' => Parse::Csi(String::new()),
                ']' => Parse::Osc,
                'O' | '(' | ')' | '#' | '%' => Parse::EscFinal,
                _ => Parse::Ground,
            },
            Parse::Csi(mut params) => {
                if ('\x20'..='\x3f').contains(&c) {
                    params.push(c);
                    Parse::Csi(params)
                } else if ('\x40'..='\x7e').contains(&c) {
                    self.csi(lines, &params, c);
                    Parse::Ground
                } else {
                    // Malformed: drop what was collected so far.
                    Parse::Ground
                }
            }
            Parse::Osc => match c {
                '\x07' => Parse::Ground,
                '\x1b' => Parse::OscEsc,
                _ => Parse::Osc,
            },
            Parse::OscEsc => {
                if c == '\\' {
                    Parse::Ground
                } else {
                    Parse::Osc
                }
            }
            Parse::EscFinal => Parse::Ground,
        };
    }

    /// Dispatches one complete CSI sequence. Only the subset a REPL echo
    /// needs is implemented; the rest is swallowed.
    fn csi<L: ConsoleLine>(&mut self, lines: &mut [L], params: &str, action: char) {
        match action {
            // Erase in line: 0 (default) cursor→end, 1 start→cursor, 2 all.
            'K' => self.erase_in_line(lines, param(params, 0)),
            // Cursor back / forward, default and minimum 1.
            'D' => self.back(param(params, 1), lines),
            'C' => self.forward(param(params, 1), lines),
            _ => {}
        }
    }

    fn putc<L: ConsoleLine>(&mut self, lines: &mut [L], c: char) {
        let Some(line) = lines.last_mut() else {
            return;
        };
        let text = line.text_mut();
        if self.col >= text.len() {
            text.push(c);
            self.col = text.len();
        } else {
            // Printing at the cursor overwrites, exactly like a terminal.
            let old_len = text[self.col..].chars().next().map_or(0, |c| c.len_utf8());
            let mut encoded = [0; 4];
            let new = c.encode_utf8(&mut encoded);
            text.replace_range(self.col..self.col + old_len, new);
            self.col += new.len();
        }
    }

    fn back<L: ConsoleLine>(&mut self, steps: usize, lines: &[L]) {
        for _ in 0..steps {
            let Some(line) = lines.last() else { break };
            if self.col == 0 {
                break;
            }
            let text = line.text();
            let prev = text[..self.col]
                .chars()
                .next_back()
                .map_or(0, |c| c.len_utf8());
            self.col -= prev;
        }
    }

    fn forward<L: ConsoleLine>(&mut self, steps: usize, lines: &[L]) {
        let Some(line) = lines.last() else {
            return;
        };
        let text = line.text();
        let mut col = self.col;
        for c in text[col..].chars().take(steps) {
            col += c.len_utf8();
        }
        self.col = col;
    }

    fn erase_in_line<L: ConsoleLine>(&mut self, lines: &mut [L], mode: usize) {
        let Some(line) = lines.last_mut() else {
            return;
        };
        let text = line.text_mut();
        match mode {
            0 => text.truncate(self.col),
            1 => {
                text.replace_range(..self.col, "");
                self.col = 0;
            }
            _ => {
                text.clear();
                self.col = 0;
            }
        }
    }
}

/// First numeric parameter of a CSI sequence, falling back to `default`
/// when absent or zero (VT100 treats a missing count as 1, and `EL` as 0).
fn param(params: &str, default: usize) -> usize {
    params
        .split(';')
        .next()
        .and_then(|p| p.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console_of(chunk: &str) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        LineConsole::new().feed(&mut lines, chunk);
        lines
    }

    #[test]
    fn backspace_echo_erases_instead_of_printing_the_sequence() {
        // What MicroPython's readline sends back for one backspace at the
        // end of the line: cursor left, then erase to end of line.
        let lines = console_of(">>> abc\x08\x1b[K");
        assert_eq!(lines, vec![">>> ab".to_string()]);
    }

    #[test]
    fn sequences_split_across_chunks_are_consumed() {
        let mut lines: Vec<String> = Vec::new();
        let mut console = LineConsole::new();
        console.feed(&mut lines, "abc\x08\x1b");
        console.feed(&mut lines, "[K");
        assert_eq!(lines, vec!["ab".to_string()]);
    }

    #[test]
    fn mid_line_delete_redraw_replays_the_tail() {
        // "abcd", Left (cursor before 'd'), then Backspace deletes 'c': the
        // readline answers with back, erase-to-end, reprint "d", park cursor.
        let lines = console_of("abcd\x08\x08\x1b[Kd\x08");
        assert_eq!(lines, vec!["abd".to_string()]);
    }

    #[test]
    fn long_cursor_jumps_arrive_as_csi_d() {
        let lines = console_of("0123456789\x1b[4D\x1b[K");
        assert_eq!(lines, vec!["012345".to_string()]);
    }

    #[test]
    fn carriage_return_overwrites_from_the_start() {
        let lines = console_of("hello\rHE");
        assert_eq!(lines, vec!["HEllo".to_string()]);
    }

    #[test]
    fn unknown_sequences_are_swallowed_whole() {
        let lines = console_of("a\x1b[?25lx\x1b[1;31mb\x1b]0;title\x07c");
        assert_eq!(lines, vec!["axbc".to_string()]);
    }

    #[test]
    fn push_line_reuses_an_empty_current_line() {
        let mut lines: Vec<String> = Vec::new();
        let mut console = LineConsole::new();
        // The trailing newline leaves an empty current line behind; the
        // banner must reuse it instead of adding a blank line above itself.
        console.feed(&mut lines, "boot\n");
        console.push_line(&mut lines, "[monitor ok]".to_string());
        assert_eq!(lines, vec!["boot".to_string(), "[monitor ok]".to_string()]);

        // Output after the banner continues right after it, as on a terminal.
        console.feed(&mut lines, "tail");
        console.push_line(&mut lines, "[monitor ok]".to_string());
        assert_eq!(
            lines,
            vec![
                "boot".to_string(),
                "[monitor ok]tail".to_string(),
                "[monitor ok]".to_string(),
            ]
        );
    }

    #[test]
    fn reset_drops_a_half_parsed_sequence() {
        let mut lines: Vec<String> = Vec::new();
        let mut console = LineConsole::new();
        console.feed(&mut lines, "ab\x1b");
        lines.clear();
        console.reset();
        // Without the reset, "[K" would complete the pending CSI sequence
        // and be swallowed instead of printed.
        console.feed(&mut lines, "[Kcd");
        assert_eq!(lines, vec!["[Kcd".to_string()]);
    }

    #[test]
    fn multibyte_chars_survive_overwrites() {
        let lines = console_of("aé\x08x");
        assert_eq!(lines, vec!["ax".to_string()]);
    }
}
