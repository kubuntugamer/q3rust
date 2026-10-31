//! Cvar core, ported from `code/qcommon/cvar.c`.
//!
//! Names are case-insensitive, as in `Cvar_FindVar`. `get` registers a
//! variable and keeps the existing value if it is already set.

/// Flags, matching the bit values in `qcommon.h`.
pub const CVAR_ARCHIVE: u32 = 1;
pub const CVAR_USERINFO: u32 = 2;
pub const CVAR_SERVERINFO: u32 = 4;
pub const CVAR_SYSTEMINFO: u32 = 8;
pub const CVAR_INIT: u32 = 16;
pub const CVAR_LATCH: u32 = 32;
pub const CVAR_ROM: u32 = 64;
pub const CVAR_USER_CREATED: u32 = 128;
pub const CVAR_TEMP: u32 = 256;
pub const CVAR_CHEAT: u32 = 512;
pub const CVAR_NORESTART: u32 = 1024;

#[derive(Debug, Clone)]
pub struct Cvar {
    pub name: String,
    pub string: String,
    pub reset_string: String,
    pub latched_string: Option<String>,
    pub value: f32,
    pub integer: i32,
    pub flags: u32,
    pub modification_count: u32,
}

impl Cvar {
    fn new(name: &str, value: &str, flags: u32) -> Self {
        Cvar {
            name: name.to_string(),
            string: value.to_string(),
            reset_string: value.to_string(),
            latched_string: None,
            value: atof(value),
            integer: atoi(value),
            flags,
            modification_count: 1,
        }
    }
}

/// Parses a leading float the way C `atof` does, returning 0 on bad input.
fn atof(s: &str) -> f32 {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || *c == '.' || (*i == 0 && (*c == '-' || *c == '+')))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    t[..end].parse::<f32>().unwrap_or(0.0)
}

/// Parses a leading integer the way C `atoi` does, returning 0 on bad input.
fn atoi(s: &str) -> i32 {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    t[..end].parse::<i32>().unwrap_or(0)
}

#[derive(Debug, Default)]
pub struct Cvars {
    vars: Vec<Cvar>,
}

impl Cvars {
    pub fn new() -> Self {
        Self::default()
    }

    /// Finds a cvar by name, ignoring ASCII case.
    pub fn find(&self, name: &str) -> Option<&Cvar> {
        self.vars.iter().find(|v| v.name.eq_ignore_ascii_case(name))
    }

    fn find_index(&self, name: &str) -> Option<usize> {
        self.vars.iter().position(|v| v.name.eq_ignore_ascii_case(name))
    }

    /// Registers a cvar with a default value. If it already exists, the
    /// existing value is kept and the flags are added to it.
    pub fn get(&mut self, name: &str, value: &str, flags: u32) -> &Cvar {
        match self.find_index(name) {
            Some(i) => {
                let v = &mut self.vars[i];
                v.flags |= flags;
                if v.reset_string.is_empty() {
                    v.reset_string = value.to_string();
                }
                &self.vars[i]
            }
            None => {
                self.vars.push(Cvar::new(name, value, flags));
                self.vars.last().unwrap()
            }
        }
    }

    /// Sets a cvar from a string, following `Cvar_Set2`. A `force` set skips
    /// the ROM, INIT, LATCH and CHEAT checks. Returns `None` if the cvar
    /// does not exist. Refused sets return the unchanged cvar.
    pub fn set(&mut self, name: &str, value: &str, force: bool, cheats: bool) -> Option<&Cvar> {
        let i = self.find_index(name)?;
        let v = &mut self.vars[i];

        if !force {
            if v.flags & CVAR_ROM != 0 || v.flags & CVAR_INIT != 0 || (v.flags & CVAR_CHEAT != 0 && !cheats) {
                return Some(&self.vars[i]);
            }
            if v.flags & CVAR_LATCH != 0 {
                v.latched_string = Some(value.to_string());
                v.modification_count += 1;
                return Some(&self.vars[i]);
            }
        } else {
            v.latched_string = None;
        }

        if v.string == value {
            return Some(&self.vars[i]);
        }
        v.modification_count += 1;
        v.string = value.to_string();
        v.value = atof(value);
        v.integer = atoi(value);
        Some(&self.vars[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_ignores_case() {
        let mut c = Cvars::new();
        c.get("r_Gamma", "1.0", 0);
        assert!(c.find("R_GAMMA").is_some());
        assert!(c.find("r_gamma").is_some());
        assert!(c.find("missing").is_none());
    }

    #[test]
    fn get_creates_with_default_and_parsed_values() {
        let mut c = Cvars::new();
        let v = c.get("sv_maxclients", "8", CVAR_SERVERINFO);
        assert_eq!(v.string, "8");
        assert_eq!(v.integer, 8);
        assert_eq!(v.value, 8.0);
        assert_eq!(v.flags, CVAR_SERVERINFO);
    }

    #[test]
    fn get_keeps_existing_value_and_ors_flags() {
        let mut c = Cvars::new();
        c.get("cl_x", "5", 0);
        let v = c.get("cl_x", "9", CVAR_ARCHIVE);
        assert_eq!(v.string, "5");
        assert_eq!(v.flags, CVAR_ARCHIVE);
    }

    #[test]
    fn set_changes_value_and_bumps_modification_count() {
        let mut c = Cvars::new();
        c.get("cl_y", "1", 0);
        let v = c.set("cl_y", "3", false, false).unwrap();
        assert_eq!(v.string, "3");
        assert_eq!(v.integer, 3);
        assert_eq!(v.modification_count, 2);
    }

    #[test]
    fn set_respects_rom_and_init_unless_forced() {
        let mut c = Cvars::new();
        c.get("sv_rom", "1", CVAR_ROM);
        c.get("sv_init", "1", CVAR_INIT);
        assert_eq!(c.set("sv_rom", "2", false, false).unwrap().string, "1");
        assert_eq!(c.set("sv_init", "2", false, false).unwrap().string, "1");
        assert_eq!(c.set("sv_rom", "2", true, false).unwrap().string, "2");
    }

    #[test]
    fn set_cheat_cvar_needs_cheats_enabled() {
        let mut c = Cvars::new();
        c.get("cg_cheat", "0", CVAR_CHEAT);
        assert_eq!(c.set("cg_cheat", "1", false, false).unwrap().string, "0");
        assert_eq!(c.set("cg_cheat", "1", false, true).unwrap().string, "1");
    }

    #[test]
    fn set_latch_defers_value_until_restart() {
        let mut c = Cvars::new();
        c.get("g_latch", "a", CVAR_LATCH);
        let v = c.set("g_latch", "b", false, false).unwrap();
        assert_eq!(v.string, "a");
        assert_eq!(v.latched_string.as_deref(), Some("b"));
    }

    #[test]
    fn set_unknown_cvar_returns_none() {
        let mut c = Cvars::new();
        assert!(c.set("nope", "1", false, false).is_none());
    }

    #[test]
    fn atof_and_atoi_match_c_on_leading_text() {
        assert_eq!(atof("2.5abc"), 2.5);
        assert_eq!(atoi("42px"), 42);
        assert_eq!(atoi("-7"), -7);
        assert_eq!(atoi("nope"), 0);
    }
}
