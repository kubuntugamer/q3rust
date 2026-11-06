//! Rust port of the Quake III Arena engine.
//!
//! The client is Rust end to end: SDL for the window, input and audio device,
//! Vulkan for drawing, and this crate for the engine itself. Modules ported
//! from the C tree keep the C names (`Con_Printf`, `Com_Parse` and so on) and
//! say which file they came from.

pub mod assets;
pub mod audio;
pub mod bg_misc;
pub mod bg_pmove;
pub mod bsp;
pub mod cbuf;
pub mod client;
pub mod cmd;
pub mod collision;
pub mod console;
pub mod cvar;
pub mod entities;
pub mod font;
pub mod fs;
pub mod game;
pub mod game_imports;
pub mod hunk;
pub mod items;
pub mod mmap;
pub mod model;
pub mod pak;
pub mod pickup;
pub mod pmove;
pub mod prediction;
pub mod ref_types;
pub mod refexport;
pub mod refimport;
pub mod shader;
pub mod shared_entity;
pub mod texture;
pub mod vk;
pub mod vm;
pub mod wav;
pub mod world;
pub mod zone;

/// Maximum length of a path, matching `MAX_OSPATH` in the C code.
pub const MAX_OSPATH: usize = 256;

/// Returns the crate version, so the C and Rust builds can be told apart.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn max_ospath_matches_c_engine() {
        assert_eq!(MAX_OSPATH, 256);
    }
}
