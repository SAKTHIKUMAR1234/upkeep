use std::collections::VecDeque;

use super::steps;

/// Rows above the cursor that stay open while a program redraws by moving the cursor up, such as
/// a restic status block.
const OPEN_ROWS: usize = 16;
/// Output lines longer than this are cut into several lines.
const MAX_LINE: usize = 16 * 1024;
const DEFAULT_COUNT: usize = 1;

/// Where the parser is in an escape sequence.
#[derive(Debug, Default)]
enum Escape {
    #[default]
    None,
    /// After ESC.
    Start,
    /// A control sequence, `ESC [`, with its parameters so far.
    Control(String),
    /// An operating system command, `ESC ]`, which ends with BEL or `ESC \`.
    Command,
}

/// Turns the bytes of a remote terminal into plain lines. It drops escape sequences and keeps
/// only the final state of rows that the program redraws with `\r` or cursor moves. The cursor
/// row and the rows below it stay open. Rows above it close after each chunk, unless the cursor
/// moved up within the last `OPEN_ROWS` line feeds; then up to `OPEN_ROWS` of them stay open.
/// A finished `::step` line closes every row above the cursor.
#[derive(Debug)]
pub struct Terminal {
    rows: VecDeque<Vec<char>>,
    row: usize,
    column: usize,
    escape: Escape,
    /// The start of a UTF-8 character that the next chunk completes.
    incomplete: Vec<u8>,
    line_feeds_since_cursor_up: usize,
    closed: Vec<String>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            rows: VecDeque::from([Vec::new()]),
            row: 0,
            column: 0,
            escape: Escape::None,
            incomplete: Vec::new(),
            line_feeds_since_cursor_up: OPEN_ROWS,
            closed: Vec::new(),
        }
    }
}

impl Terminal {
    /// Takes a chunk of output and returns the lines that can no longer change.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut bytes = std::mem::take(&mut self.incomplete)
            .into_iter()
            .chain(bytes.iter().copied())
            .collect::<Vec<u8>>();
        loop {
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    self.write(text);
                    break;
                }
                Err(error) => {
                    let (valid, rest) = bytes.split_at(error.valid_up_to());
                    self.write(std::str::from_utf8(valid).expect("checked as UTF-8"));
                    let Some(invalid) = error.error_len() else {
                        self.incomplete = rest.to_vec();
                        break;
                    };
                    self.write_char(char::REPLACEMENT_CHARACTER);
                    bytes = rest[invalid..].to_vec();
                }
            }
        }
        if self.line_feeds_since_cursor_up >= OPEN_ROWS {
            self.close_above(0);
        }
        std::mem::take(&mut self.closed)
    }

    /// Rows that may still change, without empty rows at the end.
    pub fn open_lines(&self) -> Vec<String> {
        let used = self
            .rows
            .iter()
            .rposition(|row| !row.is_empty())
            .map_or(0, |last| last + 1);
        self.rows
            .iter()
            .take(used)
            .map(|row| row.iter().collect())
            .collect()
    }

    /// Ends the stream and returns every line that is left.
    pub fn finish(&mut self) -> Vec<String> {
        if !self.incomplete.is_empty() {
            self.write_char(char::REPLACEMENT_CHARACTER);
            self.incomplete.clear();
        }
        let mut lines = std::mem::take(&mut self.closed);
        lines.extend(self.open_lines());
        *self = Self::default();
        lines
    }

    fn write(&mut self, text: &str) {
        for character in text.chars() {
            self.write_char(character);
        }
    }

    fn write_char(&mut self, character: char) {
        match std::mem::take(&mut self.escape) {
            Escape::None => self.print(character),
            Escape::Start => {
                self.escape = match character {
                    '[' => Escape::Control(String::new()),
                    ']' => Escape::Command,
                    _ => Escape::None,
                };
            }
            Escape::Control(mut parameters) => {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    self.control(character, &parameters);
                } else {
                    parameters.push(character);
                    self.escape = Escape::Control(parameters);
                }
            }
            Escape::Command => {
                self.escape = match character {
                    '\u{7}' => Escape::None,
                    '\u{1b}' => Escape::Start,
                    _ => Escape::Command,
                };
            }
        }
    }

    fn print(&mut self, character: char) {
        match character {
            '\n' => self.line_feed(),
            '\r' => self.column = 0,
            '\u{8}' => self.column = self.column.saturating_sub(1),
            '\u{1b}' => self.escape = Escape::Start,
            '\t' => self.put(character),
            _ if character.is_control() => {}
            _ => self.put(character),
        }
    }

    fn put(&mut self, character: char) {
        if self.column >= MAX_LINE {
            self.line_feed();
        }
        let row = &mut self.rows[self.row];
        if self.column < row.len() {
            row[self.column] = character;
        } else {
            row.resize(self.column, ' ');
            row.push(character);
        }
        self.column += 1;
    }

    /// Moves to the start of the next row. A finished `::step` line closes every row above the
    /// cursor, so its step starts at once.
    fn line_feed(&mut self) {
        self.row += 1;
        self.column = 0;
        self.line_feeds_since_cursor_up = self.line_feeds_since_cursor_up.saturating_add(1);
        if self.row == self.rows.len() {
            self.rows.push_back(Vec::new());
        }
        let finished: String = self.rows[self.row - 1].iter().collect();
        if steps::step_title(&finished).is_some() {
            self.close_above(0);
        } else {
            self.close_above(OPEN_ROWS);
        }
    }

    /// Closes the rows above the cursor except the last `keep` of them.
    fn close_above(&mut self, keep: usize) {
        while self.row > keep {
            let row = self.rows.pop_front().expect("rows above the cursor");
            self.closed.push(row.into_iter().collect());
            self.row -= 1;
        }
    }

    /// Applies a control sequence. Cursor moves and erasing change the rows; colours and every
    /// other sequence are dropped.
    fn control(&mut self, command: char, parameters: &str) {
        let count = parameters
            .parse()
            .ok()
            .filter(|&count| count > 0)
            .unwrap_or(DEFAULT_COUNT);
        match command {
            'A' => {
                self.row = self.row.saturating_sub(count);
                self.line_feeds_since_cursor_up = 0;
            }
            'B' => {
                self.row += count;
                while self.rows.len() <= self.row {
                    self.rows.push_back(Vec::new());
                }
            }
            'C' => self.column += count,
            'D' => self.column = self.column.saturating_sub(count),
            'G' => self.column = count - 1,
            'K' => self.erase_in_row(parameters),
            _ => {}
        }
    }

    fn erase_in_row(&mut self, parameters: &str) {
        let column = self.column;
        let row = &mut self.rows[self.row];
        match parameters {
            "" | "0" => row.truncate(column),
            "1" => row.iter_mut().take(column + 1).for_each(|cell| *cell = ' '),
            "2" => row.clear(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds every chunk and returns the closed lines, then the open ones.
    fn run(chunks: &[&[u8]]) -> (Vec<String>, Vec<String>) {
        let mut terminal = Terminal::default();
        let mut closed = Vec::new();
        for chunk in chunks {
            closed.extend(terminal.push(chunk));
        }
        (closed, terminal.open_lines())
    }

    #[test]
    fn plain_lines_close_at_once() {
        let (closed, open) = run(&[b"one\r\ntwo\r\nthr", b"ee\r\nfour"]);
        assert_eq!(closed, ["one", "two", "three"]);
        assert_eq!(open, ["four"]);

        let mut terminal = Terminal::default();
        assert_eq!(terminal.push(b"one\n\ntwo\nthree"), ["one", "", "two"]);
        assert_eq!(terminal.finish(), ["three"]);
        assert!(terminal.finish().is_empty());
    }

    #[test]
    fn carriage_return_rewrites_keep_the_final_progress() {
        let chunks: Vec<Vec<u8>> = (0..=100)
            .step_by(5)
            .map(|percent| format!("\r(Reading database ... {percent}%").into_bytes())
            .collect();
        let mut terminal = Terminal::default();
        for chunk in &chunks {
            assert!(terminal.push(chunk).is_empty());
            assert_eq!(terminal.open_lines().len(), 1);
        }
        assert_eq!(
            terminal.push(b"\r(Reading database ... 1784 files.)\r\nnext\n"),
            ["(Reading database ... 1784 files.)", "next"]
        );
        assert!(terminal.finish().is_empty());
    }

    #[test]
    fn restic_redraws_stay_one_status_block() {
        let mut terminal = Terminal::default();
        let opened = terminal.push(b"\r\x1b[2Krepository 65384d12 opened\n");
        assert_eq!(opened, ["repository 65384d12 opened"]);
        let mut closed = Vec::new();
        for second in 0..200 {
            let frame = format!(
                "\r\x1b[2K[0:{second:02}] {second} files, 0 errors\n\r\x1b[2K/boot/file-{second}\n\r\x1b[2K/etc/file-{second}\r\x1b[1A\x1b[1A"
            );
            // Later frames arrive split after their first row, as SSH reads may cut them.
            let (first, rest) = frame.split_at(if second == 0 {
                frame.len()
            } else {
                frame.find('\n').unwrap() + 1
            });
            closed.extend(terminal.push(first.as_bytes()));
            closed.extend(terminal.push(rest.as_bytes()));
        }
        assert!(closed.is_empty(), "{closed:?}");
        assert_eq!(
            terminal.open_lines(),
            [
                "[0:199] 199 files, 0 errors",
                "/boot/file-199",
                "/etc/file-199"
            ]
        );

        terminal.push(b"\r\x1b[2K\n\r\x1b[2K\n\r\x1b[2K\r\x1b[1A\x1b[1Asnapshot 1a2b saved\n");
        assert_eq!(terminal.open_lines(), ["snapshot 1a2b saved"]);
        let after: String = (1..=OPEN_ROWS)
            .map(|number| format!("line {number}\n"))
            .collect();
        let closed = terminal.push(after.as_bytes());
        assert_eq!(
            closed.len(),
            OPEN_ROWS + 1,
            "rows close again once redraws stop"
        );
        assert_eq!(closed[0], "snapshot 1a2b saved");
        assert!(terminal.open_lines().is_empty());
    }

    #[test]
    fn step_lines_close_every_row_above_at_once() {
        let (closed, open) = run(&[
            b"\x1b[32mReading\x1b[0m\n::step Take snapshot\n",
            b"progress 10%\r",
        ]);
        assert_eq!(closed, ["Reading", "::step Take snapshot"]);
        assert_eq!(open, ["progress 10%"]);
        assert_eq!(steps::step_title(&closed[1]), Some("Take snapshot"));
    }

    #[test]
    fn escapes_and_characters_split_across_chunks_are_joined() {
        let (_, open) = run(&[
            b"abc\x1b",
            b"[2",
            b"K\rdone \xe2\x80",
            b"\xa6 \x1b]0;title\x07ok",
        ]);
        assert_eq!(open, ["done … ok"]);
        let (_, open) = run(&[b"bad \xff byte"]);
        assert_eq!(open, ["bad \u{fffd} byte"]);
    }

    #[test]
    fn long_lines_are_cut() {
        let mut terminal = Terminal::default();
        let closed = terminal.push(&vec![b'x'; MAX_LINE + 10]);
        assert_eq!(
            closed.iter().map(String::len).collect::<Vec<_>>(),
            [MAX_LINE]
        );
        assert_eq!(terminal.finish(), ["x".repeat(10)]);
    }
}
