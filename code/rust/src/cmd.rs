//! Command execution, ported from the `Cmd_*` half of
//! `code/qcommon/cmd.c`.
//!
//! [`crate::cbuf`] holds the command buffer; this module adds the tokenizer,
//! the argument accessors and the command table, and runs buffered text
//! through them.
//!
//! The console-facing script commands (`exec`, `vstr`, `echo`, `cmdlist`)
//! are left to the caller, because they need the filesystem or a print sink.
//! Only `wait` is registered here, since it needs nothing but the command
//! buffer.

use crate::cbuf::Cbuf;
use crate::cvar::atoi;

/// Maximum number of tokens parsed from one command line, matching
/// `MAX_STRING_TOKENS`.
pub const MAX_STRING_TOKENS: usize = 32;

/// Size of the saved original command string, matching `BIG_INFO_STRING`.
pub const BIG_INFO_STRING: usize = 8192;

/// A command body. The command system is passed in so commands can read
/// their arguments and touch the buffer, the way the C functions use
/// `Cmd_Argv` and `Cbuf_InsertText`.
pub type CommandFn = fn(&mut CmdSystem);

/// What happened to a command line handed to [`CmdSystem::execute_string`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecResult {
    /// A registered command ran.
    Command,
    /// The first token matched a completion-only command (registered with
    /// no function). The caller should keep looking, as `cmd.c` does by
    /// falling through to `Cvar_Command` and the game imports.
    CompletionOnly,
    /// Nothing matched. The caller can forward the line to the server.
    NotFound,
}

#[derive(Debug)]
struct Command {
    name: String,
    function: Option<CommandFn>,
}

/// Command table plus the state of the last tokenized line.
#[derive(Debug)]
pub struct CmdSystem {
    pub cbuf: Cbuf,
    commands: Vec<Command>,
    tokens: Vec<String>,
    original: String,
}

impl Default for CmdSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl CmdSystem {
    /// Creates an empty command system with the `wait` command registered.
    pub fn new() -> Self {
        let mut sys = CmdSystem {
            cbuf: Cbuf::new(),
            commands: Vec::new(),
            tokens: Vec::new(),
            original: String::new(),
        };
        sys.add_command("wait", Some(wait_f));
        sys
    }

    /// Number of tokens from the last [`CmdSystem::tokenize`].
    pub fn argc(&self) -> usize {
        self.tokens.len()
    }

    /// Token `arg`, or `""` when it was not supplied, as `Cmd_Argv` does.
    pub fn argv(&self, arg: usize) -> &str {
        self.tokens.get(arg).map(String::as_str).unwrap_or("")
    }

    /// Tokens 1 to the end joined with single spaces, as `Cmd_Args` builds.
    pub fn args(&self) -> String {
        self.args_from(1)
    }

    /// Tokens `arg` to the end joined with single spaces.
    pub fn args_from(&self, arg: usize) -> String {
        if arg >= self.tokens.len() {
            return String::new();
        }
        self.tokens[arg..].join(" ")
    }

    /// The unmodified text of the last tokenized line, as `Cmd_Cmd` keeps it.
    pub fn cmd(&self) -> &str {
        &self.original
    }

    /// Parses `text` into command arguments, replacing the previous ones.
    /// Quoted strings are single tokens, `//` ends the line and `/* */` is
    /// skipped, as in `Cmd_TokenizeString`.
    pub fn tokenize(&mut self, text: &str) {
        self.tokens.clear();
        self.original.clear();

        // `Q_strncpyz` truncates the saved line to the buffer size.
        let max = text.len().min(BIG_INFO_STRING - 1);
        let end = (0..=max)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        self.original.push_str(&text[..end]);

        let bytes = text.as_bytes();
        let mut i = 0usize;

        loop {
            if self.tokens.len() == MAX_STRING_TOKENS {
                return; // this is usually something malicious
            }

            loop {
                // skip whitespace
                while i < bytes.len() && bytes[i] <= b' ' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return; // all tokens parsed
                }

                // skip // comments
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    return; // all tokens parsed
                }

                // skip /* */ comments
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/'))
                    {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return; // all tokens parsed
                    }
                    i += 2;
                } else {
                    break; // ready to parse a token
                }
            }

            // handle quoted strings; like the C code, `\"` is not escaped
            if bytes[i] == b'"' {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                self.tokens
                    .push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
                if i >= bytes.len() {
                    return; // all tokens parsed
                }
                i += 1;
                continue;
            }

            // regular token, ending at whitespace, a quote or a comment
            let start = i;
            while i < bytes.len() && bytes[i] > b' ' {
                if bytes[i] == b'"' {
                    break;
                }
                if bytes[i] == b'/' && matches!(bytes.get(i + 1), Some(&b'/') | Some(&b'*')) {
                    break;
                }
                i += 1;
            }
            self.tokens
                .push(String::from_utf8_lossy(&bytes[start..i]).into_owned());

            if i >= bytes.len() {
                return; // all tokens parsed
            }
        }
    }

    /// Registers a command. Returns `false` when a command of that exact
    /// name already exists, which is the `Cmd_AddCommand` duplicate check
    /// (`strcmp`, so it is case-sensitive).
    pub fn add_command(&mut self, name: &str, function: Option<CommandFn>) -> bool {
        if self.commands.iter().any(|c| c.name == name) {
            return false;
        }
        self.commands.insert(
            0,
            Command {
                name: name.to_string(),
                function,
            },
        );
        true
    }

    /// Removes a command by exact name. Returns `false` if it was not there.
    pub fn remove_command(&mut self, name: &str) -> bool {
        match self.commands.iter().position(|c| c.name == name) {
            Some(i) => {
                self.commands.remove(i);
                true
            }
            None => false,
        }
    }

    /// Calls `callback` with every registered command name, in table order.
    pub fn command_completion<F: FnMut(&str)>(&self, mut callback: F) {
        for cmd in &self.commands {
            callback(&cmd.name);
        }
    }

    /// Parses and dispatches one complete command line. Names are matched
    /// ignoring ASCII case, and a used command moves to the front of the
    /// table so it is found sooner next time, as the C code relinks its
    /// command list.
    pub fn execute_string(&mut self, text: &str) -> ExecResult {
        self.tokenize(text);
        if self.tokens.is_empty() {
            return ExecResult::NotFound;
        }

        let index = match self
            .commands
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(self.tokens[0].as_str()))
        {
            Some(i) => i,
            None => return ExecResult::NotFound,
        };

        let function = self.commands[index].function;
        let cmd = self.commands.remove(index);
        self.commands.insert(0, cmd);

        match function {
            Some(function) => {
                function(self);
                ExecResult::Command
            }
            None => ExecResult::CompletionOnly,
        }
    }

    /// Runs text waiting in [`CmdSystem::cbuf`], returning how many lines
    /// were dispatched. A pending `wait` is counted down and leaves the rest
    /// of the buffer for the next call.
    pub fn execute_buffer(&mut self) -> usize {
        let mut dispatched = 0;
        while !self.cbuf.is_empty() {
            if self.cbuf.wait > 0 {
                self.cbuf.wait -= 1;
                break;
            }
            let line = match self.cbuf.next_line() {
                Some(line) => line,
                None => break,
            };
            self.execute_string(&line);
            dispatched += 1;
        }
        dispatched
    }
}

/// `Cmd_Wait_f`: `wait [count]` defers the rest of the buffer, defaulting to
/// one frame.
fn wait_f(sys: &mut CmdSystem) {
    sys.cbuf.wait = if sys.argc() == 2 {
        atoi(sys.argv(1))
    } else {
        1
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark_eleven(sys: &mut CmdSystem) {
        sys.cbuf.wait = 11;
    }

    fn mark_twenty_two(sys: &mut CmdSystem) {
        sys.cbuf.wait = 22;
    }

    fn names(sys: &CmdSystem) -> Vec<String> {
        let mut out = Vec::new();
        sys.command_completion(|n| out.push(n.to_string()));
        out
    }

    #[test]
    fn tokenize_splits_on_whitespace_and_keeps_original() {
        let mut sys = CmdSystem::new();
        sys.tokenize("  map\tq3dm1   +set sv_cheats 1 ");
        assert_eq!(sys.argc(), 5);
        assert_eq!(sys.argv(0), "map");
        assert_eq!(sys.argv(1), "q3dm1");
        assert_eq!(sys.argv(3), "sv_cheats");
        assert_eq!(sys.argv(4), "1");
        assert_eq!(sys.cmd(), "  map\tq3dm1   +set sv_cheats 1 ");
    }

    #[test]
    fn quoted_string_is_one_token() {
        let mut sys = CmdSystem::new();
        sys.tokenize("say \"hello world\" now");
        assert_eq!(sys.argc(), 3);
        assert_eq!(sys.argv(1), "hello world");
        assert_eq!(sys.argv(2), "now");
    }

    #[test]
    fn unterminated_quote_still_yields_the_token() {
        let mut sys = CmdSystem::new();
        sys.tokenize("say \"abc");
        assert_eq!(sys.argc(), 2);
        assert_eq!(sys.argv(1), "abc");
    }

    #[test]
    fn line_and_block_comments_are_skipped() {
        let mut sys = CmdSystem::new();
        sys.tokenize("map q3dm1 // trailing comment");
        assert_eq!(sys.argc(), 2);
        assert_eq!(sys.argv(1), "q3dm1");

        sys.tokenize("// nothing here");
        assert_eq!(sys.argc(), 0);

        sys.tokenize("/* skip this */ map q3dm1");
        assert_eq!(sys.argc(), 2);
        assert_eq!(sys.argv(0), "map");

        sys.tokenize("map /* unterminated");
        assert_eq!(sys.argc(), 1);
        assert_eq!(sys.argv(0), "map");
    }

    #[test]
    fn tokenizing_stops_at_max_tokens() {
        let mut sys = CmdSystem::new();
        let line = (0..40)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        sys.tokenize(&line);
        assert_eq!(sys.argc(), MAX_STRING_TOKENS);
        assert_eq!(sys.argv(MAX_STRING_TOKENS - 1), "t31");
    }

    #[test]
    fn args_joins_from_the_requested_token() {
        let mut sys = CmdSystem::new();
        sys.tokenize("set sv_cheats 1");
        assert_eq!(sys.args(), "sv_cheats 1");
        assert_eq!(sys.args_from(2), "1");
        assert_eq!(sys.args_from(7), "");
    }

    #[test]
    fn argv_out_of_range_is_empty() {
        let mut sys = CmdSystem::new();
        sys.tokenize("echo");
        assert_eq!(sys.argc(), 1);
        assert_eq!(sys.argv(1), "");
        assert_eq!(sys.argv(99), "");
    }

    #[test]
    fn wait_builtin_sets_the_buffer_wait() {
        let mut sys = CmdSystem::new();
        assert_eq!(sys.execute_string("wait 5"), ExecResult::Command);
        assert_eq!(sys.cbuf.wait, 5);
        assert_eq!(sys.execute_string("wait"), ExecResult::Command);
        assert_eq!(sys.cbuf.wait, 1);
        // like `atoi`, leading digits are used and junk is ignored
        sys.execute_string("wait 3abc");
        assert_eq!(sys.cbuf.wait, 3);
    }

    #[test]
    fn execute_matches_case_insensitively_and_moves_command_to_front() {
        let mut sys = CmdSystem::new();
        sys.add_command("alpha", Some(mark_eleven));
        sys.add_command("beta", Some(mark_twenty_two));
        assert_eq!(names(&sys), vec!["beta", "alpha", "wait"]);

        assert_eq!(sys.execute_string("ALPHA now"), ExecResult::Command);
        assert_eq!(sys.cbuf.wait, 11);
        assert_eq!(names(&sys), vec!["alpha", "beta", "wait"]);
        assert_eq!(sys.args(), "now");
    }

    #[test]
    fn add_command_rejects_the_same_name_only() {
        let mut sys = CmdSystem::new();
        assert!(sys.add_command("dup", Some(mark_eleven)));
        assert!(!sys.add_command("dup", Some(mark_twenty_two)));
        // `strcmp` lets a differently-cased name through as its own entry.
        assert!(sys.add_command("DUP", None));

        assert_eq!(sys.execute_string("Dup"), ExecResult::CompletionOnly);
        assert!(sys.remove_command("DUP"));
        assert_eq!(sys.execute_string("dup"), ExecResult::Command);
        assert_eq!(sys.cbuf.wait, 11);
        assert!(sys.remove_command("dup"));
        assert!(!sys.remove_command("dup"));
    }

    #[test]
    fn unknown_or_empty_lines_are_not_found() {
        let mut sys = CmdSystem::new();
        assert_eq!(sys.execute_string("no_such_command"), ExecResult::NotFound);
        assert_eq!(sys.execute_string(""), ExecResult::NotFound);
        assert_eq!(sys.execute_string("   // comment"), ExecResult::NotFound);
    }

    #[test]
    fn execute_buffer_dispatches_every_line() {
        let mut sys = CmdSystem::new();
        // Completion-only entries are enough to count dispatches.
        sys.add_command("alpha", None);
        sys.add_command("beta", None);
        sys.cbuf.add_text("alpha; beta\ngamma");

        assert_eq!(sys.execute_buffer(), 3);
        assert!(sys.cbuf.is_empty());
    }

    #[test]
    fn execute_buffer_leaves_text_after_wait() {
        let mut sys = CmdSystem::new();
        sys.add_command("alpha", None);
        sys.cbuf.add_text("alpha; wait 1; alpha\n");

        assert_eq!(sys.execute_buffer(), 2);
        // The frame that ran `wait 1` counts it down, as `Cbuf_Execute`
        // does, so the tail waits for the next call.
        assert_eq!(sys.cbuf.wait, 0);
        assert_eq!(sys.execute_buffer(), 1);
        assert!(sys.cbuf.is_empty());
    }
}
