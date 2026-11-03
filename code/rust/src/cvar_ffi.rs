//! The C ABI surface of the cvar system: `code/qcommon/cvar.c`'s public
//! functions, backed by the thread-local [`Cvars`] store. Linking
//! `libq3rust.a` into the engine in place of `cvar.o` makes every cvar
//! registration, lookup and console command run through the port.
//!
//! Two rules keep this safe to call from C — the same two the command
//! layer follows:
//!
//! * **No borrow of the cvar store is held across a call into C.** The
//!   console builtins (`cvarlist`, `set`, …) run as `extern "C"`
//!   functions precisely so they can call `Com_Filter`, `FS_Printf` and
//!   `Info_SetValueForKey` with no borrow held; the messages each call
//!   collected in a [`Msgs`] vector are flushed afterwards.
//! * **`Com_Error` can longjmp out of these frames** (`Cvar_Update` and
//!   `Info_SetValueForKey` both raise it), so nothing is left borrowed
//!   when a call returns.
//!
//! `cvar_modifiedFlags` stays in [`cvar`](crate::cvar) as a real exported
//! global; several C files read and clear it directly.
//!
//! Compiled only under the `engine-ffi` feature: the engine's
//! `Com_*`/`FS_*` symbols do not exist in a `cargo test` binary.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr, CString};

use crate::cvar::{self, Cvars, Msg, Msgs, VmCvar, CVAR_ARCHIVE, CVAR_SERVERINFO, CVAR_SYSTEMINFO, CVAR_USERINFO};
use crate::ffi::{c_argv, c_argc, cstring};

// Symbols provided by the engine's C code at link time.
extern "C" {
    fn Com_Printf(fmt: *const c_char, ...);
    /// Gated on `com_developer` exactly as the C's own gate does, so
    /// `Msg::Dev` lines keep the developer-only behaviour.
    fn Com_DPrintf(fmt: *const c_char, ...);
    fn Com_Error(code: c_int, fmt: *const c_char, ...);
    fn Com_Filter(filter: *mut c_char, name: *mut c_char, casesensitive: c_int) -> c_int;
    fn Info_SetValueForKey(s: *mut c_char, key: *const c_char, value: *const c_char);
    fn Info_SetValueForKey_Big(s: *mut c_char, key: *const c_char, value: *const c_char);
    fn FS_Printf(f: c_int, fmt: *const c_char, ...);
}

thread_local! {
    /// The engine's one cvar store, where the C kept `cvar_indexes`.
    static CVARS: RefCell<Cvars> = RefCell::new(Cvars::new());
    /// Rotating result buffers for `Cvar_InfoString` / `_Big`, matching the
    /// C's `static char info[...]` (one buffer each, overwritten per call).
    static INFO: RefCell<Option<CString>> = const { RefCell::new(None) };
    static INFO_BIG: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Borrow the cvar store for a *pure* stretch only — never across a call
/// into C (see the module docs).
pub(crate) fn with<T>(f: impl FnOnce(&mut Cvars) -> T) -> T {
    CVARS.with(|c| f(&mut c.borrow_mut()))
}

/// A C string as a Rust string; `NULL` reads as `""`.
fn to_str<'a>(p: *const c_char) -> Cow<'a, str> {
    if p.is_null() {
        return Cow::Borrowed("");
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy()
}

/// Print one already-formatted line through a single `%s`, keeping any
/// `%` in the data itself out of the varargs. The text must be
/// NUL-terminated for C: a Rust `String` is not.
fn print(message: &str) {
    let message = cstring(message);
    unsafe {
        Com_Printf(c"%s".as_ptr(), message.as_ptr())
    }
}

/// Print one already-formatted line through `Com_DPrintf` (developer-only).
fn dprint(message: &str) {
    let message = cstring(message);
    unsafe {
        Com_DPrintf(c"%s".as_ptr(), message.as_ptr())
    }
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

/// Flush the collected [`Msgs`] through the engine, with no borrow held.
/// `Msg::Dev` goes through `Com_DPrintf`, which applies the C's own
/// `com_developer` gate itself.
pub(crate) fn flush(msgs: &Msgs) {
    for msg in msgs {
        match msg {
            Msg::Out(text) => print(text),
            Msg::Dev(text) => dprint(text),
        }
    }
}

/// Run a closure against the store, then flush whatever it collected.
fn act<T>(f: impl FnOnce(&mut Cvars, &mut Msgs) -> T) -> T {
    let mut msgs = Msgs::new();
    let result = CVARS.with(|c| f(&mut c.borrow_mut(), &mut msgs));
    flush(&msgs);
    result
}

/// Turn a `CvarError` into the engine's `Com_Error` with its own code.
pub(crate) fn fail(error: cvar::CvarError) -> ! {
    let text = format!("{}\0", error.message());
    unsafe { Com_Error(error.code(), text.as_ptr() as *const c_char) };
    // `Com_Error` is declared to return, so the type system needs the
    // divergence spelled out; at runtime it longjmps away.
    unreachable!("Com_Error returned")
}

// ---------------------------------------------------------------------------
// Registration and assignment

/// `Cvar_Get`: register `name` with a default value and flags, returning
/// the live `cvar_t *` (or `NULL`, which only `Cvar_Update`'s error path
/// can produce).
#[no_mangle]
pub extern "C" fn Cvar_Get(
    var_name: *const c_char,
    var_value: *const c_char,
    flags: c_int,
) -> *mut cvar::Cvar {
    let name = to_str(var_name).into_owned();
    let value = to_str(var_value).into_owned();
    match act(|c, msgs| c.get(&name, &value, flags, msgs)) {
        Ok(i) => with(|c| c.ptr_at(i)),
        Err(e) => fail(e), // diverges; Com_Error never returns
    }
}

/// `Cvar_Set`: forced assignment, creating the variable if needed.
#[no_mangle]
pub extern "C" fn Cvar_Set(var_name: *const c_char, value: *const c_char) {
    let name = to_str(var_name).into_owned();
    let value = to_str(value).into_owned();
    act(|c, msgs| c.set(&name, &value, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetLatched`: non-forced assignment (a `CVAR_LATCH` variable
/// takes the value only on the next restart).
#[no_mangle]
pub extern "C" fn Cvar_SetLatched(var_name: *const c_char, value: *const c_char) {
    let name = to_str(var_name).into_owned();
    let value = to_str(value).into_owned();
    act(|c, msgs| c.set2(&name, Some(&value), false, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetValue`: assignment through the float expansion
/// (`%i` for whole values, `%f` otherwise).
#[no_mangle]
pub extern "C" fn Cvar_SetValue(var_name: *const c_char, value: f32) {
    let name = to_str(var_name).into_owned();
    act(|c, msgs| c.set_value(&name, value, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_Reset`: back to the reset value (non-forced).
#[no_mangle]
pub extern "C" fn Cvar_Reset(var_name: *const c_char) {
    let name = to_str(var_name).into_owned();
    act(|c, msgs| c.reset(&name, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetCheatState`: every `CVAR_CHEAT` variable loses its latch and
/// returns to the hard-coded value.
#[no_mangle]
pub extern "C" fn Cvar_SetCheatState() {
    act(|c, msgs| c.set_cheat_state(msgs)).unwrap_or_else(|e| fail(e));
}

// ---------------------------------------------------------------------------
// Reads

/// `Cvar_VariableValue`: the float value, or `0` when missing.
#[no_mangle]
pub extern "C" fn Cvar_VariableValue(var_name: *const c_char) -> f32 {
    let name = to_str(var_name);
    with(|c| c.variable_value(&name))
}

/// `Cvar_VariableIntegerValue`: the integer value, or `0` when missing.
#[no_mangle]
pub extern "C" fn Cvar_VariableIntegerValue(var_name: *const c_char) -> c_int {
    let name = to_str(var_name);
    with(|c| c.variable_integer(&name))
}

/// `Cvar_VariableString`: the live `string` pointer, or a permanent `""`.
/// (Rust-internal callers use [`crate::cvar::variable_string`] instead;
/// this export exists for C callers outside the engine core.)
#[no_mangle]
pub extern "C" fn Cvar_VariableString(var_name: *const c_char) -> *mut c_char {
    let name = to_str(var_name);
    with(|c| c.variable_string_ptr(&name))
}

/// `Cvar_VariableStringBuffer`: the value copied into the caller's buffer.
#[no_mangle]
pub extern "C" fn Cvar_VariableStringBuffer(
    var_name: *const c_char,
    buffer: *mut c_char,
    bufsize: c_int,
) {
    let name = to_str(var_name).into_owned();
    let value = with(|c| c.variable_string_owned(&name));
    strncpyz(buffer, bufsize, &value);
}

/// `Cvar_CommandCompletion`: hand every cvar name to `callback` in the
/// store's walk order (newest first, the C's list order). The names are
/// copied out first so the callback may reenter the cvar system freely.
#[no_mangle]
pub extern "C" fn Cvar_CommandCompletion(callback: Option<extern "C" fn(*const c_char)>) {
    let Some(callback) = callback else {
        return;
    };
    let names: Vec<CString> =
        with(|c| c.completion_names().into_iter().map(|n| cstring(&n)).collect());
    for name in names {
        callback(name.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// Info strings

/// Build one info string from the store's matching `(name, value)` pairs,
/// letting the C `Info_SetValueForKey[_Big]` do the escaping.
fn build_info(bit: c_int, big: bool) -> CString {
    let pairs = with(|c| c.info_pairs(bit));
    let mut buffer = vec![0u8; if big { cvar::BIG_INFO_STRING } else { cvar::MAX_INFO_STRING }];
    for (key, value) in pairs {
        let key = cstring(&key);
        let value = cstring(&value);
        unsafe {
            if big {
                Info_SetValueForKey_Big(buffer.as_mut_ptr().cast(), key.as_ptr(), value.as_ptr());
            } else {
                Info_SetValueForKey(buffer.as_mut_ptr().cast(), key.as_ptr(), value.as_ptr());
            }
        }
    }
    // The buffer is a NUL-terminated C string the C functions wrote into.
    let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    buffer.truncate(len);
    CString::new(buffer).unwrap_or_default()
}

/// `Cvar_InfoString`: the `MAX_INFO_STRING` info string for one flag bit.
#[no_mangle]
pub extern "C" fn Cvar_InfoString(bit: c_int) -> *mut c_char {
    let text = build_info(bit, false);
    INFO.with(|slot| *slot.borrow_mut() = Some(text));
    INFO.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(empty_ptr, |c| c.as_ptr() as *mut c_char)
    })
}

/// `Cvar_InfoString_Big`: the `BIG_INFO_STRING` info string (CS_SYSTEMINFO).
#[no_mangle]
pub extern "C" fn Cvar_InfoString_Big(bit: c_int) -> *mut c_char {
    let text = build_info(bit, true);
    INFO_BIG.with(|slot| *slot.borrow_mut() = Some(text));
    INFO_BIG.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(empty_ptr, |c| c.as_ptr() as *mut c_char)
    })
}

/// `Cvar_InfoStringBuffer`: the small info string copied into the caller's
/// buffer.
#[no_mangle]
pub extern "C" fn Cvar_InfoStringBuffer(bit: c_int, buffer: *mut c_char, buffsize: c_int) {
    let text = build_info(bit, false);
    strncpyz(buffer, buffsize, &text.to_string_lossy());
}

/// The permanent `""` the info-string getters return for an empty store.
fn empty_ptr() -> *mut c_char {
    c"".as_ptr() as *mut c_char
}

// ---------------------------------------------------------------------------
// VM mirror (vmCvar_t)

/// `Cvar_Register`: `Cvar_Get` plus handle assignment and a first update.
/// A NULL `vmCvar_t` still registers the variable (the C runs `Cvar_Get`
/// before its null check) and only skips the mirror.
///
/// # Safety
/// `vm_cvar` must be NULL or a valid `vmCvar_t` that outlives this call;
/// `var_name` must be a valid C string, and `default_value` likewise when
/// non-NULL — the caller's contract, exactly as in C.
#[no_mangle]
pub unsafe extern "C" fn Cvar_Register(
    vm_cvar: *mut VmCvar,
    var_name: *const c_char,
    default_value: *const c_char,
    flags: c_int,
) {
    let name = to_str(var_name).into_owned();
    let value = to_str(default_value).into_owned();
    if vm_cvar.is_null() {
        act(|c, msgs| c.get(&name, &value, flags, msgs)).unwrap_or_else(|e| fail(e));
        return;
    }
    // Validated non-null above: the raw C pointer becomes a reference
    // exactly here, at the boundary where it enters from C.
    let vm = unsafe { &mut *vm_cvar };
    act(|c, msgs| c.register(vm, &name, &value, flags, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_Update`: refresh a VM's mirror when the modification count moved.
/// A NULL mirror is the C's own programming error (`assert(vmCvar)` in
/// the original) and raises `ERR_FATAL`.
///
/// # Safety
/// `vm_cvar` must be a valid `vmCvar_t` (NULL is caught and reported,
/// but a non-NULL wild pointer would be UB to read) — the caller's
/// contract, exactly as in C.
#[no_mangle]
pub unsafe extern "C" fn Cvar_Update(vm_cvar: *mut VmCvar) {
    let mut msgs = Msgs::new();
    let result = CVARS.with(|c| match unsafe { vm_cvar.as_mut() } {
        None => Err(cvar::CvarError::Fatal("Cvar_Update: NULL vmCvar".to_string())),
        Some(vm) => c.borrow_mut().update(vm, &mut msgs),
    });
    flush(&msgs);
    result.unwrap_or_else(|e| fail(e));
}

// ---------------------------------------------------------------------------
// Console commands (the C's `Cvar_*_f`)

/// `Cvar_Command`: print the variable named `argv[0]`, or set it from
/// `argv[1]`; false when no such variable exists (the command layer then
/// falls through to the game commands).
#[no_mangle]
pub extern "C" fn Cvar_Command() -> c_int {
    let argv: Vec<String> = (0..c_argc()).map(c_argv).collect();
    match act(|c, msgs| c.command(&argv, msgs)) {
        Ok(found) => found as c_int,
        Err(e) => fail(e),
    }
}

/// `Cvar_Toggle_f`: flip a variable between whole-number states.
extern "C" fn cvar_toggle_f() {
    let argv = current_argv();
    act(|c, msgs| c.toggle_f(&argv, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_Set_f`: `set <name> <value...>`, joining the value arguments.
extern "C" fn cvar_set_f() {
    let argv = current_argv();
    act(|c, msgs| c.set_f(&argv, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetU_f`: `set`, then flag the variable `CVAR_USERINFO`.
extern "C" fn cvar_setu_f() {
    let argv = current_argv();
    act(|c, msgs| c.set_flag_f(&argv, CVAR_USERINFO, "setu <variable> <value>", msgs))
        .unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetS_f`: `set`, then flag the variable `CVAR_SERVERINFO`.
extern "C" fn cvar_sets_f() {
    let argv = current_argv();
    act(|c, msgs| c.set_flag_f(&argv, CVAR_SERVERINFO, "sets <variable> <value>", msgs))
        .unwrap_or_else(|e| fail(e));
}

/// `Cvar_SetA_f`: `set`, then flag the variable `CVAR_ARCHIVE`.
extern "C" fn cvar_seta_f() {
    let argv = current_argv();
    act(|c, msgs| c.set_flag_f(&argv, CVAR_ARCHIVE, "seta <variable> <value>", msgs))
        .unwrap_or_else(|e| fail(e));
}

/// `Cvar_Reset_f`: back to the reset value.
extern "C" fn cvar_reset_f() {
    let argv = current_argv();
    act(|c, msgs| c.reset_f(&argv, msgs)).unwrap_or_else(|e| fail(e));
}

/// `Cvar_List_f`: flag column + `name "value"` lines, filtered through the
/// C's `Com_Filter`.
extern "C" fn cvar_list_f() {
    let argc = c_argc();
    let filter = if argc > 1 { c_argv(1) } else { String::new() };
    let (rows, total) = with(|c| c.list_rows());
    let indexes = with(|c| c.num_indexes());
    let mut msgs = Msgs::new();
    Cvars::list_lines(&rows, total, indexes, |name| {
        if argc <= 1 {
            return true;
        }
        let filter = cstring(&filter);
        let name = cstring(name);
        unsafe {
            Com_Filter(
                filter.as_ptr() as *mut c_char,
                name.as_ptr() as *mut c_char,
                0,
            ) != 0
        }
    }, &mut msgs);
    flush(&msgs);
}

/// `Cvar_Restart_f`: drop user-created variables, restore the rest.
extern "C" fn cvar_restart_f() {
    act(|c, msgs| c.restart(msgs)).unwrap_or_else(|e| fail(e));
}

/// The current token list, copied out so the builtins run with no borrow
/// held (they call into C while processing).
fn current_argv() -> Vec<String> {
    let argc = c_argc();
    (0..argc).map(c_argv).collect()
}

/// `Cvar_Init`: the `sv_cheats` registration plus the eight console
/// commands, registered in the C's order so `cvarlist`'s output matches.
#[no_mangle]
pub extern "C" fn Cvar_Init() {
    act(|c, msgs| {
        c.get("sv_cheats", "1", (cvar::CVAR_ROM | CVAR_SYSTEMINFO) as c_int, msgs)
    })
    .unwrap_or_else(|e| fail(e));
    // The C order, so `cvarlist`'s output matches the original.
    crate::ffi::Cmd_AddCommand(c"toggle".as_ptr(), Some(cvar_toggle_f));
    crate::ffi::Cmd_AddCommand(c"set".as_ptr(), Some(cvar_set_f));
    crate::ffi::Cmd_AddCommand(c"sets".as_ptr(), Some(cvar_sets_f));
    crate::ffi::Cmd_AddCommand(c"setu".as_ptr(), Some(cvar_setu_f));
    crate::ffi::Cmd_AddCommand(c"seta".as_ptr(), Some(cvar_seta_f));
    crate::ffi::Cmd_AddCommand(c"reset".as_ptr(), Some(cvar_reset_f));
    crate::ffi::Cmd_AddCommand(c"cvarlist".as_ptr(), Some(cvar_list_f));
    crate::ffi::Cmd_AddCommand(c"cvar_restart".as_ptr(), Some(cvar_restart_f));
}

// ---------------------------------------------------------------------------
// `Cvar_WriteVariables` needs the C `FS_Printf` on a `fileHandle_t`; it
// lives here because that is a C call.

/// `Cvar_WriteVariables`: the `seta` lines for archived cvars, written to
/// the engine's file handle through `FS_Printf`.
#[no_mangle]
pub extern "C" fn Cvar_WriteVariables(f: c_int) {
    let lines = with(|c| c.write_variables());
    for line in lines {
        let line = cstring(&line);
        unsafe { FS_Printf(f, c"%s".as_ptr(), line.as_ptr()) };
    }
}
