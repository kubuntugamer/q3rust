//! Renderer imports, ported from `refimport_t` in
//! `code/renderer/tr_public.h`.
//!
//! `refimport_t` is the other half of the refresh interface: everything the
//! renderer pulls back out of the engine — console printing, memory, cvars,
//! commands, the filesystem, clipmodel debug drawing and cinematics.
//! `GetRefAPI` is handed this table, and a Rust renderer built on the
//! [`RefExport`](crate::refexport::RefExport) trait would be handed an
//! implementation of this one.
//!
//! Where the C API hands out memory the caller must release, the Rust
//! signatures return owned values instead, so `FS_FreeFile`,
//! `FS_FreeFileList` and `Free` have no counterpart here. Formatting is done
//! at the call site, because a trait method cannot be variadic.

use crate::cvar::Cvar;
use crate::hunk::{HunkPtr, Pref};

/// `cbufExec_t` in `q_shared.h`, as used by `Cmd_ExecuteText`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecWhen {
    /// Don't return until completed; a VM should never use this, because
    /// some commands might unload it.
    Now = 0,
    /// Insert at the current position, but don't run yet.
    Insert = 1,
    /// Add to the end of the command buffer (the normal case).
    Append = 2,
}

/// `e_status` in `q_shared.h`, the state a cinematic is in after
/// `CIN_RunCinematic`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FmvStatus {
    #[default]
    Idle = 0,
    Play = 1,
    /// All other conditions, i.e. stop / EOF / abort.
    Eof = 2,
    IdBlt = 3,
    IdIdle = 4,
    Looped = 5,
    IdWait = 6,
}

/// The engine services a renderer imports (`refimport_t`).
pub trait RefImport {
    /// Prints a message on the local console. The C entry point is
    /// variadic, so the message is formatted by the caller here.
    fn printf(&mut self, print_level: i32, message: &str);

    /// Aborts the game at the given error level (`ERR_FATAL`, `ERR_DROP`).
    fn error(&mut self, error_level: i32, message: &str);

    /// Milliseconds should only be used for profiling, never for anything
    /// game related — the renderer gets time from the refdef.
    fn milliseconds(&mut self) -> i32;

    /// Stack based memory for per-level things that will not be freed,
    /// zero-filled as `Hunk_Alloc` does.
    fn hunk_alloc(&mut self, size: usize, pref: Pref) -> HunkPtr;

    /// Temporary memory for file loading, freed with
    /// [`RefImport::hunk_free_temp_memory`].
    fn hunk_allocate_temp_memory(&mut self, size: usize) -> HunkPtr;
    fn hunk_free_temp_memory(&mut self, ptr: HunkPtr);

    /// The dynamic allocator for things that need to be freed; the returned
    /// buffer is zero-filled, as `Z_Malloc` does, and dropped by the caller
    /// instead of through a separate `Free`.
    fn malloc(&mut self, bytes: usize) -> Vec<u8>;

    /// Registers a cvar with a default value, keeping any existing value.
    fn cvar_get(&mut self, name: &str, value: &str, flags: u32) -> &Cvar;
    fn cvar_set(&mut self, name: &str, value: &str);

    fn cmd_add_command(&mut self, name: &str, command: fn());
    fn cmd_remove_command(&mut self, name: &str);

    fn cmd_argc(&self) -> usize;
    fn cmd_argv(&self, arg: usize) -> &str;
    fn cmd_execute_text(&mut self, exec_when: ExecWhen, text: &str);

    /// Visualization for debugging collision detection: `draw_poly` is
    /// called once per polygon with a color and the point list.
    fn cm_draw_debug_surface(&mut self, draw_poly: &mut dyn FnMut(i32, &[f32]));

    /// The checksum of the pak the file lives in, or `None` when it is not
    /// in any pak. The C version returns -1/1 and writes the checksum to an
    /// out-parameter.
    fn fs_file_is_in_pak(&mut self, name: &str) -> Option<i32>;

    /// Reads a whole file. A `None` buffer only asks for the length, as
    /// passing NULL does in C; with a buffer the contents are appended to
    /// it. Returns -1 when the file does not exist.
    fn fs_read_file(&mut self, name: &str, buffer: Option<&mut Vec<u8>>) -> i32;

    /// Lists files in a directory with the given extension, without the
    /// dot. The list is returned by value, so `FS_FreeFileList` has no
    /// counterpart here.
    fn fs_list_files(&mut self, name: &str, extension: &str) -> Vec<String>;

    fn fs_write_file(&mut self, qpath: &str, buffer: &[u8]);
    fn fs_file_exists(&mut self, file: &str) -> bool;

    fn cin_upload_cinematic(&mut self, handle: i32);
    fn cin_play_cinematic(
        &mut self,
        arg0: &str,
        xpos: i32,
        ypos: i32,
        width: i32,
        height: i32,
        bits: i32,
    ) -> i32;
    fn cin_run_cinematic(&mut self, handle: i32) -> FmvStatus;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::CmdSystem;
    use crate::cvar::{Cvars, CVAR_ARCHIVE};
    use crate::hunk::Hunk;
    use std::collections::HashMap;

    /// A minimal engine behind the import table, wired to the modules that
    /// have already been ported.
    #[derive(Default)]
    struct StubEngine {
        messages: Vec<String>,
        errors: Vec<(i32, String)>,
        clock: i32,
        hunk: Option<Hunk>,
        cvars: Cvars,
        cmds: CmdSystem,
        files: HashMap<String, Vec<u8>>,
        cinematic: i32,
        uploaded: Vec<i32>,
    }

    impl StubEngine {
        fn new() -> Self {
            StubEngine {
                hunk: Some(Hunk::new(4096)),
                ..Default::default()
            }
        }
    }

    impl RefImport for StubEngine {
        fn printf(&mut self, print_level: i32, message: &str) {
            self.messages.push(format!("[{print_level}] {message}"));
        }

        fn error(&mut self, error_level: i32, message: &str) {
            self.errors.push((error_level, message.to_string()));
        }

        fn milliseconds(&mut self) -> i32 {
            self.clock += 16;
            self.clock
        }

        fn hunk_alloc(&mut self, size: usize, pref: Pref) -> HunkPtr {
            self.hunk.as_mut().unwrap().alloc(size, pref).unwrap()
        }

        fn hunk_allocate_temp_memory(&mut self, size: usize) -> HunkPtr {
            self.hunk.as_mut().unwrap().allocate_temp(size).unwrap()
        }

        fn hunk_free_temp_memory(&mut self, ptr: HunkPtr) {
            let _ = self.hunk.as_mut().unwrap().free_temp(ptr);
        }

        fn malloc(&mut self, bytes: usize) -> Vec<u8> {
            vec![0; bytes]
        }

        fn cvar_get(&mut self, name: &str, value: &str, flags: u32) -> &Cvar {
            self.cvars.get(name, value, flags)
        }

        fn cvar_set(&mut self, name: &str, value: &str) {
            let _ = self.cvars.set(name, value, true, true);
        }

        fn cmd_add_command(&mut self, name: &str, command: fn()) {
            // The C signature carries a no-argument callback; a Rust engine
            // would keep its own thunk table, so this stub only records the
            // name to prove the call arrives.
            let _ = (name, command);
        }

        fn cmd_remove_command(&mut self, name: &str) {
            let _ = name;
        }

        fn cmd_argc(&self) -> usize {
            self.cmds.argc()
        }

        fn cmd_argv(&self, arg: usize) -> &str {
            self.cmds.argv(arg)
        }

        fn cmd_execute_text(&mut self, exec_when: ExecWhen, text: &str) {
            match exec_when {
                ExecWhen::Now => {
                    self.cmds.execute_string(text);
                }
                ExecWhen::Insert => {
                    self.cmds.cbuf.insert_text(text);
                }
                ExecWhen::Append => {
                    self.cmds.cbuf.add_text(text);
                }
            }
        }

        fn cm_draw_debug_surface(&mut self, draw_poly: &mut dyn FnMut(i32, &[f32])) {
            draw_poly(3, &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        }

        fn fs_file_is_in_pak(&mut self, name: &str) -> Option<i32> {
            self.files.contains_key(name).then_some(0x1234)
        }

        fn fs_read_file(&mut self, name: &str, buffer: Option<&mut Vec<u8>>) -> i32 {
            match self.files.get(name) {
                None => -1,
                Some(bytes) => {
                    if let Some(buffer) = buffer {
                        buffer.extend_from_slice(bytes);
                    }
                    bytes.len() as i32
                }
            }
        }

        fn fs_list_files(&mut self, name: &str, extension: &str) -> Vec<String> {
            let suffix = format!(".{extension}");
            let mut files: Vec<String> = self
                .files
                .keys()
                .filter(|key| key.starts_with(name) && key.ends_with(&suffix))
                .cloned()
                .collect();
            files.sort();
            files
        }

        fn fs_write_file(&mut self, qpath: &str, buffer: &[u8]) {
            self.files.insert(qpath.to_string(), buffer.to_vec());
        }

        fn fs_file_exists(&mut self, file: &str) -> bool {
            self.files.contains_key(file)
        }

        fn cin_upload_cinematic(&mut self, handle: i32) {
            self.uploaded.push(handle);
        }

        fn cin_play_cinematic(
            &mut self,
            _arg0: &str,
            _xpos: i32,
            _ypos: i32,
            _width: i32,
            _height: i32,
            _bits: i32,
        ) -> i32 {
            self.cinematic = 1;
            self.cinematic
        }

        fn cin_run_cinematic(&mut self, handle: i32) -> FmvStatus {
            if handle == self.cinematic {
                FmvStatus::Play
            } else {
                FmvStatus::Eof
            }
        }
    }

    #[test]
    fn exec_when_keeps_the_c_values() {
        assert_eq!(ExecWhen::Now as i32, 0);
        assert_eq!(ExecWhen::Insert as i32, 1);
        assert_eq!(ExecWhen::Append as i32, 2);
    }

    #[test]
    fn fmv_status_keeps_the_c_values() {
        assert_eq!(FmvStatus::Idle as i32, 0);
        assert_eq!(FmvStatus::Play as i32, 1);
        assert_eq!(FmvStatus::Eof as i32, 2);
        assert_eq!(FmvStatus::IdBlt as i32, 3);
        assert_eq!(FmvStatus::IdIdle as i32, 4);
        assert_eq!(FmvStatus::Looped as i32, 5);
        assert_eq!(FmvStatus::IdWait as i32, 6);
    }

    #[test]
    fn console_and_clock_reach_the_engine() {
        let mut engine = StubEngine::new();
        engine.printf(1, "loading world");
        engine.error(1, "bad shader");
        assert_eq!(engine.messages, vec!["[1] loading world"]);
        assert_eq!(engine.errors, vec![(1, "bad shader".to_string())]);
        assert_eq!(engine.milliseconds(), 16);
        assert_eq!(engine.milliseconds(), 32);
    }

    #[test]
    fn memory_imports_use_the_ported_allocators() {
        let mut engine = StubEngine::new();

        let ptr = engine.hunk_alloc(64, Pref::Low);
        assert_eq!(
            engine.hunk.as_ref().unwrap().get(ptr, 64),
            Some(&[0u8; 64][..])
        );

        let temp = engine.hunk_allocate_temp_memory(32);
        engine.hunk_free_temp_memory(temp);

        assert_eq!(engine.malloc(4), vec![0u8; 4]);
    }

    #[test]
    fn cvar_imports_use_the_ported_cvar_store() {
        let mut engine = StubEngine::new();
        let cv = engine.cvar_get("r_gamma", "1.0", CVAR_ARCHIVE);
        assert_eq!(cv.string, "1.0");
        assert_eq!(cv.value, 1.0);
        assert_eq!(cv.flags, CVAR_ARCHIVE);

        engine.cvar_set("r_gamma", "2.5");
        assert_eq!(engine.cvar_get("r_gamma", "1.0", 0).value, 2.5);
    }

    #[test]
    fn command_imports_use_the_ported_command_system() {
        let mut engine = StubEngine::new();

        engine.cmd_execute_text(ExecWhen::Append, "setwait 3");
        assert!(!engine.cmds.cbuf.is_empty());

        // Register a real command through the ported table and run it.
        engine.cmds.add_command(
            "setwait",
            Some(|sys: &mut CmdSystem| {
                sys.cbuf.wait = if sys.argc() == 2 { 3 } else { 1 };
            }),
        );
        engine.cmd_execute_text(ExecWhen::Now, "setwait 3");
        assert_eq!(engine.cmd_argc(), 2);
        assert_eq!(engine.cmd_argv(0), "setwait");
        assert_eq!(engine.cmd_argv(1), "3");
        assert_eq!(engine.cmds.cbuf.wait, 3);
    }

    #[test]
    fn filesystem_imports_round_trip_files() {
        let mut engine = StubEngine::new();
        assert!(!engine.fs_file_exists("scripts/foo.arena"));
        assert_eq!(engine.fs_read_file("scripts/foo.arena", None), -1);

        engine.fs_write_file("scripts/foo.arena", b"arenaData");
        assert!(engine.fs_file_exists("scripts/foo.arena"));
        assert_eq!(engine.fs_file_is_in_pak("scripts/foo.arena"), Some(0x1234));
        assert_eq!(engine.fs_file_is_in_pak("nope"), None);

        // Length only, as passing NULL does in C.
        assert_eq!(engine.fs_read_file("scripts/foo.arena", None), 9);

        let mut buffer = Vec::new();
        assert_eq!(
            engine.fs_read_file("scripts/foo.arena", Some(&mut buffer)),
            9
        );
        assert_eq!(buffer, b"arenaData");

        engine.fs_write_file("scripts/bar.arena", b"other");
        assert_eq!(
            engine.fs_list_files("scripts/", "arena"),
            vec!["scripts/bar.arena", "scripts/foo.arena"]
        );
        assert_eq!(
            engine.fs_list_files("scripts/", "map"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn collision_debug_drawing_reaches_the_callback() {
        let mut engine = StubEngine::new();
        let mut seen: Vec<(i32, Vec<f32>)> = Vec::new();
        engine.cm_draw_debug_surface(&mut |color, points| {
            seen.push((color, points.to_vec()));
        });
        assert_eq!(seen, vec![(3, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0])]);
    }

    #[test]
    fn cinematic_imports_report_status() {
        let mut engine = StubEngine::new();
        assert_eq!(engine.cin_run_cinematic(7), FmvStatus::Eof);

        let handle = engine.cin_play_cinematic("video.roq", 0, 0, 640, 480, 16);
        assert_eq!(handle, 1);
        assert_eq!(engine.cin_run_cinematic(handle), FmvStatus::Play);

        engine.cin_upload_cinematic(handle);
        assert_eq!(engine.uploaded, vec![handle]);
    }
}
