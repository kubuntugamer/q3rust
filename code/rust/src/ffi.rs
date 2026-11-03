//! The C ABI surface of the port: `code/qcommon/cmd.c`'s `Cbuf_*` and
//! `Cmd_*` functions, backed by [`crate::cmd::CmdSystem`]. Linking
//! `libq3rust.a` into the engine in place of `cmd.o` makes every console
//! command and every buffered line run through Rust.
//!
//! Two rules keep this safe to call from C:
//!
//! * **No borrow of the command system is held across a call into C.**
//!   The C code reenters through `Cmd_Argc` & co. mid-dispatch
//!   (`Cvar_Command` does exactly that), and `Com_Error` can longjmp out
//!   of our frames — a held `RefCell` borrow would poison every later
//!   call.
//! * **Rust-registered commands must not call out to C while running**,
//!   because they do run with the borrow held. Only `wait` is one and it
//!   is pure; the script builtins (`exec`, `vstr`, `echo`, `cmdlist`) are
//!   registered as `extern "C"` functions instead, so they run with no
//!   borrow held and borrow only for their own short reads.
//!
//! Compiled only under the `engine-ffi` feature: the engine's
//! `Com_*`/`Cvar_*`/`FS_*` symbols do not exist in a `cargo test`
//! binary.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::thread::LocalKey;

use crate::cmd::{self, CmdSystem, Dispatch};
use crate::ref_types::MAX_QPATH;
use crate::shader::default_extension;

/// `ERR_FATAL` from `errorParm_t` in `q_shared.h` (first enumerator).
const ERR_FATAL: c_int = 0;
/// `EXEC_NOW` from `cbufExec_t`.
const EXEC_NOW: c_int = 0;
/// `EXEC_INSERT` from `cbufExec_t`.
const EXEC_INSERT: c_int = 1;
/// `EXEC_APPEND` from `cbufExec_t`.
const EXEC_APPEND: c_int = 2;

// Symbols provided by the engine's C code at link time.
extern "C" {
    fn Com_Printf(fmt: *const c_char, ...);
    fn Com_Error(code: c_int, fmt: *const c_char, ...);
    fn Com_Filter(filter: *mut c_char, name: *mut c_char, casesensitive: c_int) -> c_int;
    fn FS_ReadFile(qpath: *const c_char, buffer: *mut *mut c_void) -> c_int;
    fn FS_FreeFile(buffer: *mut c_void);
    fn CL_GameCommand() -> c_int;
    fn SV_GameCommand() -> c_int;
    fn UI_GameCommand() -> c_int;
    fn CL_ForwardCommandToServer(string: *const c_char);
}

thread_local! {
    /// The engine's one command system, where the C kept statics.
    static SYS: RefCell<CmdSystem> = RefCell::new(CmdSystem::empty());
    /// NUL-terminated mirrors of the current tokens: `Cmd_Argv` hands
    /// their pointers to C, valid until the next tokenize — exactly the
    /// lifetime of the C's `cmd_tokenized` buffer.
    static ARGV: RefCell<Vec<CString>> = const { RefCell::new(Vec::new()) };
    /// Result buffers for `Cmd_Args`, `Cmd_ArgsFrom` and `Cmd_Cmd`,
    /// rebuilt per call like the C's static buffers.
    static ARGS: RefCell<Option<CString>> = const { RefCell::new(None) };
    static ARGS_FROM: RefCell<Option<CString>> = const { RefCell::new(None) };
    static CMD: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Borrow the command system for a *pure* stretch only — never across a
/// call into C (see the module docs).
fn with<T>(f: impl FnOnce(&mut CmdSystem) -> T) -> T {
    SYS.with(|sys| f(&mut sys.borrow_mut()))
}

/// A C string as a Rust string; `NULL` reads as `""`.
fn to_str<'a>(p: *const c_char) -> Cow<'a, str> {
    if p.is_null() {
        return Cow::Borrowed("");
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy()
}

/// Print a fully-formatted line through one `%s`, so engine-side
/// formatting (and any `%` in the data itself) stays out of the varargs.
/// The text must be NUL-terminated for C: a Rust `String` is not.
fn print(message: &str) {
    let message = cstring(message);
    unsafe {
        Com_Printf(c"%s".as_ptr(), message.as_ptr())
    }
}

/// A `CString` that never panics: interior NULs are truncated away.
pub(crate) fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|e| {
        CString::new(&text.as_bytes()[..e.nul_position()]).unwrap_or_default()
    })
}

/// The pointer `Cmd_Argv` uses for a missing token: a permanent `""`,
/// like the C's literal.
fn empty_arg() -> *mut c_char {
    c"".as_ptr() as *mut c_char
}

/// `Q_strncpyz`: copy at most `destsize - 1` bytes and terminate.
fn strncpyz(dest: *mut c_char, destsize: c_int, src: &str) {
    if dest.is_null() || destsize <= 0 {
        return;
    }
    let n = (destsize as usize - 1).min(src.len());
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dest.cast(), n);
        *dest.add(n) = 0;
    }
}

/// Rebuild the `Cmd_Argv`/`Cmd_Cmd` mirrors after a tokenize, so the
/// pointers C holds die at the same moment the C's would.
fn refresh_arg_slots() {
    with(|sys| {
        ARGV.with(|slots| {
            let mut slots = slots.borrow_mut();
            slots.clear();
            for i in 0..sys.argc() {
                slots.push(cstring(sys.argv(i)));
            }
        });
        CMD.with(|slot| *slot.borrow_mut() = Some(cstring(sys.cmd())));
    });
}

/// Store a result string in one of the rotating output buffers and hand
/// back its pointer, as the C returns its `static char[]`s.
fn store(slot: &'static LocalKey<RefCell<Option<CString>>>, text: &str) -> *mut c_char {
    let text = cstring(text);
    slot.with(|s| *s.borrow_mut() = Some(text));
    slot.with(|s| {
        s.borrow()
            .as_ref()
            .map_or_else(empty_arg, |c| c.as_ptr() as *mut c_char)
    })
}

/// `Cbuf_AddText`'s body over a borrowed string.
fn add_text_str(text: &str) {
    if !with(|sys| sys.cbuf.add_text(text)) {
        print("Cbuf_AddText: overflow\n");
    }
}

/// `Cbuf_InsertText`'s body over a borrowed string.
fn insert_text_str(text: &str) {
    if !with(|sys| sys.cbuf.insert_text(text)) {
        print("Cbuf_InsertText overflowed\n");
    }
}

/// Execute one command line: tokenize and look up with the borrow
/// held, refresh the argument mirrors, then run the found function —
/// or the C fallback chain — with no borrow held.
fn run_line(text: &str) {
    let dispatch = with(|sys| sys.dispatch(text));
    refresh_arg_slots();
    match dispatch {
        Dispatch::NoTokens => {}
        Dispatch::FallThrough { .. } => fallback_chain(text),
        Dispatch::Rust(function) => with(function),
        Dispatch::C(function) => function(),
    }
}

/// `Cmd_ExecuteString`'s tail: cvar, then the game commands, then
/// forwarding to the server — gated on the running-subsystem cvars the
/// same way the C gates on `com_cl_running->integer` /
/// `com_sv_running->integer` (reading those cvars by name gives the
/// identical value, including before they are registered).
fn fallback_chain(text: &str) {
    // The cvar store is Rust now: read it through its own accessors with
    // no borrow held across the C calls below, exactly as the C read the
    // `com_cl_running` / `com_sv_running` globals (a cvar that does not
    // exist yet reads as `0`, same as the C's unregistered global).
    let cl_running = crate::cvar::variable_integer("cl_running") != 0;
    let sv_running = crate::cvar::variable_integer("sv_running") != 0;
    let text = cstring(text);
    unsafe {
        if crate::cvar::command() {
            return;
        }
        if cl_running && CL_GameCommand() != 0 {
            return;
        }
        if sv_running && SV_GameCommand() != 0 {
            return;
        }
        if cl_running && UI_GameCommand() != 0 {
            return;
        }
        CL_ForwardCommandToServer(text.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// Command buffer (Cbuf_*)

/// `Cbuf_Init`.
#[no_mangle]
pub extern "C" fn Cbuf_Init() {
    with(|sys| sys.cbuf = crate::cbuf::Cbuf::new());
}

/// `Cbuf_AddText`: append without a trailing newline.
#[no_mangle]
pub extern "C" fn Cbuf_AddText(text: *const c_char) {
    add_text_str(&to_str(text));
}

/// `Cbuf_InsertText`: insert at the front, followed by a newline.
#[no_mangle]
pub extern "C" fn Cbuf_InsertText(text: *const c_char) {
    insert_text_str(&to_str(text));
}

/// `Cbuf_Execute`: run buffered lines until the buffer empties or a
/// `wait` defers the rest to the next frame.
#[no_mangle]
pub extern "C" fn Cbuf_Execute() {
    enum Step {
        Done,
        Wait,
        Line(String),
    }
    loop {
        let step = with(|sys| {
            if sys.cbuf.is_empty() {
                return Step::Done;
            }
            if sys.cbuf.wait > 0 {
                sys.cbuf.wait -= 1;
                return Step::Wait;
            }
            match sys.cbuf.next_line() {
                Some(line) => Step::Line(line),
                None => Step::Done,
            }
        });
        match step {
            Step::Done | Step::Wait => break,
            Step::Line(line) => run_line(&line),
        }
    }
}

/// `Cbuf_ExecuteText`: run, insert or append according to `exec_when`.
#[no_mangle]
pub extern "C" fn Cbuf_ExecuteText(exec_when: c_int, text: *const c_char) {
    let text = to_str(text);
    match exec_when {
        EXEC_NOW => {
            if text.is_empty() {
                Cbuf_Execute();
            } else {
                run_line(&text);
            }
        }
        EXEC_INSERT => insert_text_str(&text),
        EXEC_APPEND => add_text_str(&text),
        _ => unsafe {
            Com_Error(
                ERR_FATAL,
                c"Cbuf_ExecuteText: bad exec_when".as_ptr(),
            )
        },
    }
}

// ---------------------------------------------------------------------------
// Command execution (Cmd_*)

/// `Cmd_Init`: the five script commands, registered in the C's order so
/// the table (and `cmdlist`'s output) matches.
#[no_mangle]
pub extern "C" fn Cmd_Init() {
    with(|sys| {
        sys.add_c_command("cmdlist", Some(cmdlist_f));
        sys.add_c_command("exec", Some(exec_f));
        sys.add_c_command("vstr", Some(vstr_f));
        sys.add_c_command("echo", Some(echo_f));
        sys.add_command("wait", Some(cmd::wait_f));
    });
}

/// `Cmd_Argc`.
#[no_mangle]
pub extern "C" fn Cmd_Argc() -> c_int {
    with(|sys| sys.argc() as c_int)
}

/// `Cmd_Argc` as a `usize`, for the command layer's own callers.
pub(crate) fn c_argc() -> usize {
    Cmd_Argc().max(0) as usize
}

/// `Cmd_Argv` as an owned string (the cvar layer reads tokens this way).
pub(crate) fn c_argv(i: usize) -> String {
    to_str(Cmd_Argv(i as c_int)).into_owned()
}

/// `Cmd_Argv`: the token's stable address, or `""` when out of range
/// (negative arguments count as out of range, as C's unsigned check
/// makes them).
#[no_mangle]
pub extern "C" fn Cmd_Argv(arg: c_int) -> *mut c_char {
    if arg < 0 {
        return empty_arg();
    }
    ARGV.with(|slots| {
        slots
            .borrow()
            .get(arg as usize)
            .map_or_else(empty_arg, |s| s.as_ptr() as *mut c_char)
    })
}

/// `Cmd_ArgvBuffer`: the token copied into the caller's buffer.
#[no_mangle]
pub extern "C" fn Cmd_ArgvBuffer(arg: c_int, buffer: *mut c_char, buffer_length: c_int) {
    let token = unsafe { CStr::from_ptr(Cmd_Argv(arg)) }.to_string_lossy();
    strncpyz(buffer, buffer_length, &token);
}

/// `Cmd_Args`: tokens 1..argc joined with single spaces.
#[no_mangle]
pub extern "C" fn Cmd_Args() -> *mut c_char {
    let args = with(|sys| sys.args());
    store(&ARGS, &args)
}

/// `Cmd_ArgsFrom`: tokens `arg`..argc joined with single spaces (a
/// negative `arg` clamps to 0, as in the C).
#[no_mangle]
pub extern "C" fn Cmd_ArgsFrom(arg: c_int) -> *mut c_char {
    let args = with(|sys| sys.args_from(arg.max(0) as usize));
    store(&ARGS_FROM, &args)
}

/// `Cmd_ArgsBuffer`: the arguments copied into the caller's buffer.
#[no_mangle]
pub extern "C" fn Cmd_ArgsBuffer(buffer: *mut c_char, buffer_length: c_int) {
    let args = unsafe { CStr::from_ptr(Cmd_Args()) }
        .to_string_lossy()
        .into_owned();
    strncpyz(buffer, buffer_length, &args);
}

/// `Cmd_Cmd`: the unmodified text of the last tokenized line.
#[no_mangle]
pub extern "C" fn Cmd_Cmd() -> *mut c_char {
    CMD.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(empty_arg, |c| c.as_ptr() as *mut c_char)
    })
}

/// `Cmd_TokenizeString`: parse `text_in` into the argument list.
/// (`NULL` is treated as `""`, which clears the arguments; no engine
/// caller passes `NULL`.)
#[no_mangle]
pub extern "C" fn Cmd_TokenizeString(text_in: *const c_char) {
    let text = to_str(text_in);
    with(|sys| sys.tokenize(&text));
    refresh_arg_slots();
}

/// `Cmd_AddCommand`: register a C command handler; duplicates print the
/// C's warning, silent when the existing entry is completion-only.
#[no_mangle]
pub extern "C" fn Cmd_AddCommand(cmd_name: *const c_char, function: Option<extern "C" fn()>) {
    let name = to_str(cmd_name);
    let added = with(|sys| sys.add_c_command(&name, function));
    if !added && function.is_some() {
        print(&format!("Cmd_AddCommand: {name} already defined\n"));
    }
}

/// `Cmd_RemoveCommand`: unregister by exact name, silent if absent.
#[no_mangle]
pub extern "C" fn Cmd_RemoveCommand(cmd_name: *const c_char) {
    let name = to_str(cmd_name);
    with(|sys| sys.remove_command(&name));
}

/// `Cmd_CommandCompletion`: hand every command name to `callback` in
/// table order. The names are copied out first, so the callback may
/// reenter the command system freely.
#[no_mangle]
pub extern "C" fn Cmd_CommandCompletion(callback: Option<extern "C" fn(*const c_char)>) {
    let Some(callback) = callback else {
        return;
    };
    let names: Vec<CString> = with(|sys| {
        let mut names = Vec::new();
        sys.command_completion(|name| names.push(cstring(name)));
        names
    });
    for name in names {
        callback(name.as_ptr());
    }
}

/// `Cmd_ExecuteString`: tokenize, run the command, or walk the fallback
/// chain — the C's whole function, reborrowed safely around each C call.
#[no_mangle]
pub extern "C" fn Cmd_ExecuteString(text: *const c_char) {
    let text = to_str(text);
    run_line(&text);
}

// ---------------------------------------------------------------------------
// Script commands registered by Cmd_Init

/// `Cmd_List_f`: list command names, filtered through `Com_Filter`.
extern "C" fn cmdlist_f() {
    let (argc, filter, names) = with(|sys| {
        let argc = sys.argc();
        let filter = sys.argv(1).to_string();
        let mut names = Vec::new();
        sys.command_completion(|name| names.push(name.to_string()));
        (argc, filter, names)
    });
    let mut count = 0;
    for name in &names {
        // Com_Filter is C: call it with no borrow held, as everywhere.
        if argc > 1 {
            let filter = cstring(&filter);
            let name_ptr = cstring(name);
            if unsafe {
                Com_Filter(
                    filter.as_ptr() as *mut c_char,
                    name_ptr.as_ptr() as *mut c_char,
                    0,
                )
            } == 0
            {
                continue;
            }
        }
        print(&format!("{name}\n"));
        count += 1;
    }
    print(&format!("{count} commands\n"));
}

/// `Cmd_Exec_f`: load a script file and insert it into the buffer.
extern "C" fn exec_f() {
    let (argc, arg1) = with(|sys| (sys.argc(), sys.argv(1).to_string()));
    if argc != 2 {
        print("exec <filename> : execute a script file\n");
        return;
    }
    // Q_strncpyz into a MAX_QPATH buffer, then COM_DefaultExtension(".cfg").
    let mut filename = arg1.clone().into_bytes();
    filename.truncate(MAX_QPATH - 1);
    let filename = default_extension(&String::from_utf8_lossy(&filename), ".cfg");

    let path = cstring(&filename);
    let mut buffer: *mut c_void = std::ptr::null_mut();
    unsafe { FS_ReadFile(path.as_ptr(), &mut buffer) };
    if buffer.is_null() {
        print(&format!("couldn't exec {arg1}\n"));
        return;
    }
    print(&format!("execing {arg1}\n"));

    // FS_ReadFile NUL-terminates what it hands back.
    let text = unsafe { CStr::from_ptr(buffer.cast()) }.to_string_lossy();
    insert_text_str(&text);
    unsafe { FS_FreeFile(buffer) };
}

/// `Cmd_Vstr_f`: insert the cvar's current value as command text.
extern "C" fn vstr_f() {
    let (argc, arg1) = with(|sys| (sys.argc(), sys.argv(1).to_string()));
    if argc != 2 {
        print("vstr <variablename> : execute a variable command\n");
        return;
    }
    let value = crate::cvar::variable_string(&arg1);
    // The C passes `va("%s\n", v)` to a Cbuf_InsertText that appends a
    // second newline; reproduce both.
    insert_text_str(&format!("{value}\n"));
}

/// `Cmd_Echo_f`: print the rest of the line — every argument followed
/// by a space, then a newline, byte-for-byte as the C's loop.
extern "C" fn echo_f() {
    let line = with(|sys| {
        (1..sys.argc())
            .map(|i| format!("{} ", sys.argv(i)))
            .collect::<String>()
    });
    print(&format!("{line}\n"));
}
