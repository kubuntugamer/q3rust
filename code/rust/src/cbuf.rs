//! Command buffer, ported from `Cbuf_*` in `code/qcommon/cmd.c`.
//!
//! Behaviour matches the C code: text is appended or inserted, then split
//! into command lines on `;`, `\n` or `\r` (a `;` inside quotes does not
//! split), and each line is handed to the executor. Lines longer than
//! `MAX_CMD_LINE - 1` bytes are clipped, and the rest of the text stays in
//! the buffer for the next pass, as in the C code.

/// Size of the command buffer in bytes, matching `MAX_CMD_BUFFER`.
pub const MAX_CMD_BUFFER: usize = 16384;
/// Longest command line handed to the executor, matching `MAX_CMD_LINE`.
pub const MAX_CMD_LINE: usize = 1024;

/// Command buffer state. `wait` is the number of frames to defer execution,
/// as set by the `wait` command.
#[derive(Debug, Default)]
pub struct Cbuf {
    text: Vec<u8>,
    pub wait: i32,
}

impl Cbuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of buffered bytes.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// True when no text is buffered.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Buffered text as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.text
    }

    /// Appends text to the end of the buffer. No newline is added.
    /// Returns `false` on overflow, in which case nothing is added.
    pub fn add_text(&mut self, text: &str) -> bool {
        // Same strict check as the C code, which always keeps one byte free.
        if self.text.len() + text.len() >= MAX_CMD_BUFFER {
            return false;
        }
        self.text.extend_from_slice(text.as_bytes());
        true
    }

    /// Inserts text at the front of the buffer, followed by a newline.
    /// Returns `false` on overflow, in which case nothing is added.
    pub fn insert_text(&mut self, text: &str) -> bool {
        let len = text.len() + 1;
        if len + self.text.len() > MAX_CMD_BUFFER {
            return false;
        }
        let mut new = Vec::with_capacity(len + self.text.len());
        new.extend_from_slice(text.as_bytes());
        new.push(b'\n');
        new.extend_from_slice(&self.text);
        self.text = new;
        true
    }

    /// Removes and returns the next command line, or `None` when the buffer
    /// is empty. A `;` inside quotes does not end the line, and a line longer
    /// than `MAX_CMD_LINE - 1` bytes is clipped, leaving the rest buffered.
    pub fn next_line(&mut self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }

        // Find the end of the line: `;` outside quotes, or a newline.
        let mut quotes = 0usize;
        let mut i = 0usize;
        while i < self.text.len() {
            let c = self.text[i];
            if c == b'"' {
                quotes += 1;
            }
            if quotes & 1 == 0 && c == b';' {
                break;
            }
            if c == b'\n' || c == b'\r' {
                break;
            }
            i += 1;
        }

        if i >= MAX_CMD_LINE - 1 {
            i = MAX_CMD_LINE - 1;
        }

        let line = String::from_utf8_lossy(&self.text[..i]).into_owned();

        // Remove the consumed text, including the separator. If the line
        // ran to the end of the buffer, clear it.
        if i == self.text.len() {
            self.text.clear();
        } else {
            self.text.drain(..i + 1);
        }

        Some(line)
    }

    /// Runs buffered commands, calling `exec` with each command line.
    /// Stops early when `wait` is set, leaving the rest for the next call.
    pub fn execute<F: FnMut(&str)>(&mut self, mut exec: F) {
        while !self.text.is_empty() {
            if self.wait > 0 {
                self.wait -= 1;
                break;
            }
            match self.next_line() {
                Some(line) => exec(&line),
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cbuf: &mut Cbuf) -> Vec<String> {
        let mut out = Vec::new();
        cbuf.execute(|l| out.push(l.to_string()));
        out
    }

    #[test]
    fn add_text_appends_in_order() {
        let mut c = Cbuf::new();
        assert!(c.add_text("map q3dm1"));
        assert!(c.add_text("\n"));
        assert_eq!(run(&mut c), vec!["map q3dm1"]);
    }

    #[test]
    fn add_text_rejects_overflow() {
        let mut c = Cbuf::new();
        let big = "x".repeat(MAX_CMD_BUFFER);
        assert!(!c.add_text(&big));
        assert!(c.is_empty());
    }

    #[test]
    fn insert_text_runs_before_existing_text() {
        let mut c = Cbuf::new();
        c.add_text("second");
        assert!(c.insert_text("first"));
        assert_eq!(run(&mut c), vec!["first", "second"]);
    }

    #[test]
    fn insert_text_rejects_overflow() {
        let mut c = Cbuf::new();
        c.add_text("abc");
        let big = "x".repeat(MAX_CMD_BUFFER);
        assert!(!c.insert_text(&big));
        assert_eq!(c.as_bytes(), b"abc");
    }

    #[test]
    fn semicolon_and_newline_split_commands() {
        let mut c = Cbuf::new();
        c.add_text("a 1; b 2\nc 3\r\nd 4");
        // `\r\n` is one line break for `c 3`; the `\n` left over is an empty line.
        assert_eq!(run(&mut c), vec!["a 1", " b 2", "c 3", "", "d 4"]);
    }

    #[test]
    fn semicolon_inside_quotes_does_not_split() {
        let mut c = Cbuf::new();
        c.add_text("say \"a;b\"; next\n");
        assert_eq!(run(&mut c), vec!["say \"a;b\"", " next"]);
    }

    #[test]
    fn wait_defers_remaining_text_to_next_call() {
        let mut c = Cbuf::new();
        c.add_text("one\ntwo\n");
        c.wait = 1;
        assert!(run(&mut c).is_empty());
        assert_eq!(c.wait, 0);
        assert_eq!(run(&mut c), vec!["one", "two"]);
    }

    #[test]
    fn long_line_is_clipped_and_rest_kept() {
        let mut c = Cbuf::new();
        let long = "y".repeat(MAX_CMD_LINE + 10);
        c.add_text(&long);
        // The first 1023 bytes run as one line. The 10-byte tail runs on the
        // next pass, as in the C code's `while (cmd_text.cursize)` loop.
        let lines = run(&mut c);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_CMD_LINE - 1);
        assert_eq!(lines[1].len(), 10);
        assert!(c.is_empty());
    }
}
