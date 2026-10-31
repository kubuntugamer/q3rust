//! Rust port of the Quake III Arena engine core.
//!
//! Phase 1 scaffold: this crate builds beside the C code and does not
//! replace any of it yet. Modules are added one at a time in later phases.

pub mod cbuf;
pub mod cmd;
pub mod cvar;
pub mod pak;
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
