//! The console: the text buffer, the input line and its history.
//!
//! This is `Con_Printf`, the input line from `cl_keys.c` and the scrollback
//! from `cl_console.c`, with the drawing left to the client. The console is
//! engine-global in Quake (`Con_Printf` is called from everywhere), so it lives
//! behind a lock here too, and commands may print without being handed a
//! reference to it.
//!
//! Nothing here draws, and nothing here runs a command: the client pulls the
//! pending line out with [`Console::take_line`] and hands it to the command
//! system *outside* the lock, so a command that prints cannot deadlock.

use std::sync::{Mutex, OnceLock};

/// Lines of scrollback kept before the oldest are dropped.
pub const MAX_LINES: usize = 1024;
/// Lines of input history kept.
pub const MAX_HISTORY: usize = 64;

/// Character columns the console wraps to, before the client tells it the
/// real window size.
pub const DEFAULT_COLUMNS: usize = 78;

#[derive(Debug)]
pub struct Console {
    lines: Vec<String>,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// How far back in the history the input line is, if at all.
    history_position: Option<usize>,
    /// What was typed before the history was walked, restored on the way back.
    draft: Vec<char>,
    /// Lines scrolled up from the bottom.
    scroll: usize,
    open: bool,
    columns: usize,
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Console {
    pub fn new() -> Self {
        Console {
            lines: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_position: None,
            draft: Vec::new(),
            scroll: 0,
            open: true,
            columns: DEFAULT_COLUMNS,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Columns the console wraps to. The client sets this from the window
    /// width and the font scale.
    pub fn set_columns(&mut self, columns: usize) {
        self.columns = columns.max(8);
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn input(&self) -> String {
        self.input.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Lines scrolled up from the bottom of the scrollback.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Prints text, splitting on newlines and wrapping at the console width.
    /// Colour escapes (`^1` … `^7`) are dropped: this console has one colour.
    pub fn print(&mut self, text: &str) {
        for line in text.split('\n') {
            let line = strip_colours(line);
            for wrapped in wrap(&line, self.columns) {
                self.lines.push(wrapped);
            }
        }
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
        // New output pulls the view back to the bottom, as the C console does.
        self.scroll = 0;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll = 0;
    }

    /// Scrolls the view up by `lines`, hiding the newest ones. At least one
    /// line stays on screen, so the top of the scrollback is reachable.
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = (self.scroll + lines).min(self.lines.len().saturating_sub(1));
    }

    /// Scrolls the view down, stopping at the newest line.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// The lines to draw above the input line, newest last, given how many
    /// rows fit on screen.
    pub fn visible(&self, rows: usize) -> &[String] {
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(rows);
        &self.lines[start..end]
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        self.input.insert(self.cursor, ch);
        self.cursor += 1;
        self.leave_history();
    }

    pub fn pop_char(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
        self.leave_history();
    }

    pub fn delete_char(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
        self.leave_history();
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.len());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Replaces the input line, as tab completion does. The cursor goes to the
    /// end of the completed text.
    pub fn set_input(&mut self, text: &str) {
        self.input = text.chars().collect();
        self.cursor = self.input.len();
        self.history_position = None;
    }

    /// Walks back through the history, remembering what was being typed.
    /// Editing what history suggested turns it into a fresh line, so walking
    /// forward again will not overwrite the edit.
    fn leave_history(&mut self) {
        self.history_position = None;
        self.draft.clear();
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let position = match self.history_position {
            Some(position) => position.saturating_sub(1),
            None => {
                self.draft = self.input.clone();
                self.history.len() - 1
            }
        };
        self.history_position = Some(position);
        self.input = self.history[position].chars().collect();
        self.cursor = self.input.len();
    }

    /// Walks forward through the history, back to the remembered draft.
    pub fn history_next(&mut self) {
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 < self.history.len() {
            self.history_position = Some(position + 1);
            self.input = self.history[position + 1].chars().collect();
        } else {
            self.history_position = None;
            self.input = std::mem::take(&mut self.draft);
        }
        self.cursor = self.input.len();
    }

    /// Takes the current input line, records it in the history and leaves the
    /// input empty. Returns `None` for a blank line.
    pub fn take_line(&mut self) -> Option<String> {
        let line: String = self.input.drain(..).collect();
        self.cursor = 0;
        self.history_position = None;
        self.draft.clear();

        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Consecutive repeats are kept: the C console does not dedupe either.
        self.history.push(trimmed.to_string());
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        Some(line)
    }
}

/// Drops `^N` colour escapes, keeping the text either side.
pub fn strip_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '^' {
            if matches!(chars.peek(), Some(next) if next.is_ascii_digit()) {
                chars.next();
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Splits a line to fit `columns`, preferring to break at a space.
pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(8);
    let mut out = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();

    while rest.len() > columns {
        let window = &rest[..columns];
        let split = window
            .iter()
            .rposition(|ch| *ch == ' ')
            .filter(|index| *index > 0)
            .unwrap_or(columns);
        out.push(rest[..split].iter().collect());
        rest.drain(..split);
        while rest.first() == Some(&' ') {
            rest.remove(0);
        }
    }
    out.push(rest.into_iter().collect());
    out
}

/// The process-wide console.
fn shared() -> &'static Mutex<Console> {
    static CONSOLE: OnceLock<Mutex<Console>> = OnceLock::new();
    CONSOLE.get_or_init(|| Mutex::new(Console::new()))
}

/// Locks the console for the client's own reads and edits.
pub fn lock() -> std::sync::MutexGuard<'static, Console> {
    // A panic while holding the console would otherwise take the client with
    // it; the buffer is plain data, so recovering is safe.
    shared().lock().unwrap_or_else(|e| e.into_inner())
}

/// `Con_Printf` for the rest of the engine.
pub fn print(text: &str) {
    lock().print(text);
}

/// `Con_ClearNotify`-style wipe of the scrollback.
pub fn clear() {
    lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_console_is_open_and_empty() {
        let console = Console::new();
        assert!(console.is_open());
        assert_eq!(console.line_count(), 0);
        assert_eq!(console.input(), "");
    }

    #[test]
    fn print_splits_and_wraps() {
        let mut console = Console::new();
        console.set_columns(10);
        console.print("one\ntwo\n");
        assert_eq!(console.lines(), ["one", "two", ""]);

        console.clear();
        console.print("the quick brown fox jumps");
        // Broken at spaces where one fits in the ten-column window.
        assert_eq!(console.lines(), ["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn words_longer_than_the_console_are_cut() {
        let mut console = Console::new();
        console.set_columns(8);
        console.print("abcdefghijkl");
        assert_eq!(console.lines(), ["abcdefgh", "ijkl"]);
    }

    #[test]
    fn colour_escapes_are_dropped() {
        assert_eq!(strip_colours("^1red^7 white"), "red white");
        assert_eq!(strip_colours("a^"), "a^");
        assert_eq!(strip_colours("100%^"), "100%^");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut console = Console::new();
        for i in 0..(MAX_LINES + 10) {
            console.print(&i.to_string());
        }
        assert_eq!(console.line_count(), MAX_LINES);
        assert_eq!(console.lines()[MAX_LINES - 1], (MAX_LINES + 9).to_string());
    }

    #[test]
    fn printing_returns_the_view_to_the_bottom() {
        let mut console = Console::new();
        for i in 0..50 {
            console.print(&i.to_string());
        }
        console.scroll_up(10);
        assert_eq!(console.scroll(), 10);
        assert_eq!(console.visible(5).last(), Some(&"39".to_string()));

        console.print("new");
        assert_eq!(console.scroll(), 0);
        assert_eq!(console.visible(2), ["49", "new"]);
    }

    #[test]
    fn scroll_stops_at_both_ends() {
        let mut console = Console::new();
        console.print("a\nb");
        console.scroll_up(100);
        assert_eq!(console.scroll(), 1, "the newest line stays hidden, not all");
        assert_eq!(console.visible(10), ["a"]);
        console.scroll_down(1);
        assert_eq!(console.visible(10), ["a", "b"]);
    }

    #[test]
    fn editing_the_input_line() {
        let mut console = Console::new();
        for ch in "helo".chars() {
            console.insert_char(ch);
        }
        assert_eq!(console.cursor(), 4);
        console.move_left();
        console.insert_char('l');
        assert_eq!(console.input(), "hello");
        console.move_home();
        console.delete_char();
        assert_eq!(console.input(), "ello");
        console.pop_char();
        assert_eq!(console.input(), "ello");
        console.move_end();
        console.pop_char();
        assert_eq!(console.input(), "ell");
    }

    #[test]
    fn take_line_records_history_and_clears_input() {
        let mut console = Console::new();
        for ch in "echo hi".chars() {
            console.insert_char(ch);
        }
        assert_eq!(console.take_line().as_deref(), Some("echo hi"));
        assert_eq!(console.input(), "");
        assert_eq!(console.cursor(), 0);

        assert_eq!(console.take_line(), None, "an empty line is not a command");
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut console = Console::new();
        for line in ["one", "two"] {
            console.set_input(line);
            console.take_line();
        }

        console.set_input("dra");
        console.history_previous();
        assert_eq!(console.input(), "two");
        console.history_previous();
        assert_eq!(console.input(), "one");
        console.history_previous();
        assert_eq!(console.input(), "one", "the oldest line is the end");

        console.history_next();
        assert_eq!(console.input(), "two");
        console.history_next();
        assert_eq!(console.input(), "dra");
    }

    #[test]
    fn history_is_bounded() {
        let mut console = Console::new();
        for i in 0..(MAX_HISTORY + 5) {
            console.set_input(&i.to_string());
            console.take_line();
        }
        console.history_previous();
        assert_eq!(console.input(), (MAX_HISTORY + 4).to_string());
        for _ in 0..MAX_HISTORY {
            console.history_previous();
        }
        assert_eq!(console.input(), "5");
    }

}
