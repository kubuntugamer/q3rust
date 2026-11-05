//! The imports the game module calls, from `gameImport_t` in
//! `code/game/g_public.h` and the `trap_*` wrappers in `g_syscalls.c`.
//!
//! A `qagame.qvm` reaches the engine only through these numbered calls, so
//! this module is the whole engine interface that module sees. The numbering
//! is the C enums', not an invention: import 0 is `G_PRINT`, 10 is
//! `G_FS_FOPEN_FILE`, 37 is `G_GET_ENTITY_TOKEN`, the C library sits at 100,
//! and the botlib block starts at 200.
//!
//! That 100 block is not optional. `g_syscalls.asm` defines `memset` as `-101`,
//! so the module's `memset( &level, 0, sizeof( level ) )` arrives here as
//! import 100, from `sharedTraps_t` in `qcommon.h`. Without it the game never
//! clears its level, entity or client arrays and runs on whatever the data
//! segment was initialised with.
//!
//! What is here answers from the parts of the engine that exist: the cvar
//! system, the filesystem and the paks, the console, the map's submodels, and
//! the map's collision model, so a brush entity gets the bounds of its inline
//! model, a real absolute box, and a trace that hits the world he was thrown
//! at. What belongs to subsystems that are not ported yet — the entity grid,
//! the server's client slots, botlib — answers with the value that means
//! "nothing happened" and counts the call in [`GameImports::unimplemented`], so
//! a run that leans on them says so instead of pretending.
//!
//! `vmCvar_t` is a mirror a module keeps of one engine variable:
//! `{ cvarHandle_t handle; int modificationCount; float value; int integer;
//! char string[MAX_CVAR_VALUE_STRING]; }` — handle, count, value and integer at
//! offsets 0, 4, 8 and 12, the string as a 256-byte buffer at 16. The engine
//! writes the mirror; the module only reads it, and asks for a refresh through
//! `G_CVAR_UPDATE` when it wants the count checked.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::assets::Assets;
use crate::bsp::BspModel;
use crate::collision::{Collision, Trace};
use crate::cvar::Cvars;
use crate::shared_entity;

/// The game imports, in `gameImport_t` order.
pub mod g {
    pub const PRINT: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const MILLISECONDS: i32 = 2;
    pub const CVAR_REGISTER: i32 = 3;
    pub const CVAR_UPDATE: i32 = 4;
    pub const CVAR_SET: i32 = 5;
    pub const CVAR_VARIABLE_INTEGER_VALUE: i32 = 6;
    pub const CVAR_VARIABLE_STRING_BUFFER: i32 = 7;
    pub const ARGC: i32 = 8;
    pub const ARGV: i32 = 9;
    pub const FS_FOPEN_FILE: i32 = 10;
    pub const FS_READ: i32 = 11;
    pub const FS_WRITE: i32 = 12;
    pub const FS_FCLOSE_FILE: i32 = 13;
    pub const SEND_CONSOLE_COMMAND: i32 = 14;
    pub const LOCATE_GAME_DATA: i32 = 15;
    pub const DROP_CLIENT: i32 = 16;
    pub const SEND_SERVER_COMMAND: i32 = 17;
    pub const SET_CONFIGSTRING: i32 = 18;
    pub const GET_CONFIGSTRING: i32 = 19;
    pub const GET_USERINFO: i32 = 20;
    pub const SET_USERINFO: i32 = 21;
    pub const GET_SERVERINFO: i32 = 22;
    pub const SET_BRUSH_MODEL: i32 = 23;
    pub const TRACE: i32 = 24;
    pub const POINT_CONTENTS: i32 = 25;
    pub const IN_PVS: i32 = 26;
    pub const IN_PVS_IGNORE_PORTALS: i32 = 27;
    pub const ADJUST_AREA_PORTAL_STATE: i32 = 28;
    pub const AREAS_CONNECTED: i32 = 29;
    pub const LINKENTITY: i32 = 30;
    pub const UNLINKENTITY: i32 = 31;
    pub const ENTITIES_IN_BOX: i32 = 32;
    pub const ENTITY_CONTACT: i32 = 33;
    pub const BOT_ALLOCATE_CLIENT: i32 = 34;
    pub const BOT_FREE_CLIENT: i32 = 35;
    pub const GET_USERCMD: i32 = 36;
    pub const GET_ENTITY_TOKEN: i32 = 37;
    pub const FS_GETFILELIST: i32 = 38;
    pub const DEBUG_POLYGON_CREATE: i32 = 39;
    pub const DEBUG_POLYGON_DELETE: i32 = 40;
    pub const REAL_TIME: i32 = 41;
    pub const SNAPVECTOR: i32 = 42;
    pub const TRACECAPSULE: i32 = 43;
    pub const ENTITY_CONTACTCAPSULE: i32 = 44;
    pub const FS_SEEK: i32 = 45;

    /// First botlib import; everything at or above it is botlib's block.
    pub const BOTLIB_SETUP: i32 = 200;
}

/// The shared C library traps, `sharedTraps_t` in `qcommon.h`. The VM build
/// of `bg_lib.c` reaches these through `g_syscalls.asm`, which spells them
/// `memset -101`, `memcpy -102` and so on.
pub mod trap {
    pub const MEMSET: i32 = 100;
    pub const MEMCPY: i32 = 101;
    pub const STRNCPY: i32 = 102;
    pub const SIN: i32 = 103;
    pub const COS: i32 = 104;
    pub const ATAN2: i32 = 105;
    pub const SQRT: i32 = 106;
    pub const MATRIXMULTIPLY: i32 = 107;
    pub const ANGLEVECTORS: i32 = 108;
    pub const PERPENDICULARVECTOR: i32 = 109;
    pub const FLOOR: i32 = 110;
    pub const CEIL: i32 = 111;
}

/// `ENTITYNUM_NONE` from `q_shared.h`, the entity a cleared trace reports.
const ENTITYNUM_NONE: i32 = 1023;
/// `ENTITYNUM_WORLD`: what `SV_Trace` names as the blocker when the world
/// stopped the move.
const ENTITYNUM_WORLD: i32 = 1022;
/// `MAX_SUBMODELS` from `cm_local.h`: `CM_InlineModel` refuses a higher
/// number, and so does the map loader.
const MAX_SUBMODELS: usize = 256;
/// `MAX_CVAR_VALUE_STRING` from `q_shared.h`: the mirror's string buffer.
const MAX_CVAR_VALUE_STRING: i32 = 256;
/// Cap on one string copy into the module, so a corrupt buffer size cannot
/// spin here. `MAX_TOKEN_CHARS` and `MAX_INFO_STRING` are both 1024, so this
/// leaves room for every string the game asks for.
const MAX_WRITE: usize = 4096;

/// One open file handle, as `FS_FOpenFileRead` hands out.
struct OpenFile {
    bytes: Vec<u8>,
    position: usize,
}

/// What `G_LOCATE_GAME_DATA` reported: the module's entity array with its
/// stride and length, and its client array, which is the game's `gclient_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameData {
    pub entities: i32,
    pub entity_count: i32,
    pub entity_size: i32,
    pub clients: i32,
    pub client_size: i32,
}

/// What a game module can reach. One of these lives as long as the module.
pub struct GameImports {
    cvars: Cvars,
    /// Handle -> cvar name, in registration order, standing in for
    /// `cvar_indexes`.
    registered: Vec<String>,
    config_strings: HashMap<i32, String>,
    /// Userinfo strings the server holds per client, which the module reads
    /// and rewrites through the userinfo imports.
    userinfo: HashMap<i32, String>,
    /// The map's submodels, indexed by the `*N` an entity's model key names.
    models: Vec<BspModel>,
    /// The map's collision model, which answers `G_TRACE` and
    /// `G_POINT_CONTENTS`.
    collision: Option<Collision>,
    /// The map's entity lump, and how far the module has parsed it.
    entity_string: Vec<u8>,
    entity_position: usize,
    files: Vec<Option<OpenFile>>,
    assets: Option<Assets>,
    started: Instant,
    /// Console lines the module asked the engine to execute.
    pub console: Vec<(i32, String)>,
    /// `trap_Printf` output, in order.
    pub printed: Vec<String>,
    /// `trap_Error` messages. A module that errors is not stopped here; the
    /// caller decides whether to abandon the run.
    pub errors: Vec<String>,
    /// Server commands and client drops, kept for a server that can act on
    /// them later.
    pub server_commands: Vec<(i32, String)>,
    /// What `G_LOCATE_GAME_DATA` reported, if the module is far enough to have
    /// called it.
    pub game_data: Option<GameData>,
    /// Imports that were answered by a stand-in, and how often.
    pub unimplemented: BTreeMap<i32, u32>,
}

impl GameImports {
    pub fn new() -> Self {
        GameImports {
            cvars: Cvars::new(),
            registered: Vec::new(),
            config_strings: HashMap::new(),
            userinfo: HashMap::new(),
            models: Vec::new(),
            collision: None,
            entity_string: Vec::new(),
            entity_position: 0,
            files: Vec::new(),
            assets: None,
            started: Instant::now(),
            console: Vec::new(),
            printed: Vec::new(),
            errors: Vec::new(),
            server_commands: Vec::new(),
            game_data: None,
            unimplemented: BTreeMap::new(),
        }
    }

    /// Gives the imports the mounted paks, so `G_FS_FOPEN_FILE` can serve a
    /// module its own files.
    pub fn with_assets(mut self, assets: Assets) -> Self {
        self.assets = Some(assets);
        self
    }

    /// The paks behind the module, for the files it reads.
    pub fn assets(&self) -> Option<&Assets> {
        self.assets.as_ref()
    }

    /// Gives the module the map's entity lump, as `CM_EntityString` does before
    /// `GAME_INIT`, and rewinds the parse point to its start.
    pub fn set_entity_string(&mut self, text: &[u8]) {
        self.entity_string = text.to_vec();
        self.entity_position = 0;
    }

    /// The next token `G_GET_ENTITY_TOKEN` hands out, or `None` once the lump
    /// has been parsed to its end.
    pub fn next_entity_token(&mut self) -> Option<String> {
        crate::bsp::next_token(&self.entity_string, &mut self.entity_position)
    }

    /// The userinfo the server holds for a client. The server sets this from
    /// the connect packet before `GAME_CLIENT_CONNECT`, and the module rewrites
    /// it through `G_SET_USERINFO`.
    pub fn set_userinfo(&mut self, client: i32, info: &str) {
        self.userinfo.insert(client, info.to_string());
    }

    pub fn userinfo(&self, client: i32) -> Option<&str> {
        self.userinfo.get(&client).map(String::as_str)
    }

    /// Gives the imports the map's submodels, as `CM_LoadMap` has them before
    /// the game asks for one.
    pub fn set_models(&mut self, models: Vec<BspModel>) {
        self.models = models;
    }

    pub fn models(&self) -> &[BspModel] {
        &self.models
    }

    /// Gives the imports the map's collision model, as `CM_LoadMap` builds it
    /// before the game is allowed to trace.
    pub fn set_collision(&mut self, collision: Collision) {
        self.collision = Some(collision);
    }

    pub fn collision(&self) -> Option<&Collision> {
        self.collision.as_ref()
    }

    pub fn cvars(&self) -> &Cvars {
        &self.cvars
    }

    pub fn cvars_mut(&mut self) -> &mut Cvars {
        &mut self.cvars
    }

    /// Seeds a cvar the way the server would before `GAME_INIT`, so a module
    /// asking for `mapname` or `sv_maxclients` gets an answer.
    pub fn seed(&mut self, name: &str, value: &str) {
        self.cvars.get(name, value, 0);
    }

    /// Runs one import. `data` is the module's data segment: every pointer in
    /// `parms` is an offset into it, which is what `VM_ArgPtr` resolves.
    pub fn call(&mut self, data: &mut [u8], number: i32, parms: &[i32; 10]) -> i32 {
        match number {
            g::PRINT => {
                self.printed.push(string(data, parms[1]));
                0
            }
            g::ERROR => {
                self.errors.push(string(data, parms[1]));
                0
            }
            g::MILLISECONDS => self.started.elapsed().as_millis() as i32,

            g::CVAR_REGISTER => {
                let (target, name, default, flags) =
                    (parms[1], parms[2], parms[3], parms[4] as u32);
                let name = string(data, name);
                let default = string(data, default);
                self.cvars.get(&name, &default, flags);
                let handle = match self
                    .registered
                    .iter()
                    .position(|registered| registered.eq_ignore_ascii_case(&name))
                {
                    Some(handle) => handle,
                    None => {
                        self.registered.push(name);
                        self.registered.len() - 1
                    }
                };
                if target != 0 {
                    store(data, target, handle as i32);
                    // `Cvar_Register` writes -1 and then calls `Cvar_Update`,
                    // so the first registration always copies the value.
                    store(data, target + 4, -1);
                    self.update_mirror(data, target);
                }
                0
            }
            g::CVAR_UPDATE => {
                if parms[1] != 0 {
                    self.update_mirror(data, parms[1]);
                }
                0
            }
            g::CVAR_SET => {
                // `trap_Cvar_Set` goes through `Cvar_Set`, which creates the
                // variable when it is missing and skips the latch.
                let name = string(data, parms[1]);
                let value = string(data, parms[2]);
                self.cvars.get(&name, "", 0);
                self.cvars.set(&name, &value, true, true);
                0
            }
            g::CVAR_VARIABLE_INTEGER_VALUE => {
                let name = string(data, parms[1]);
                self.cvars.find(&name).map(|cvar| cvar.integer).unwrap_or(0)
            }
            g::CVAR_VARIABLE_STRING_BUFFER => {
                let name = string(data, parms[1]);
                let (buffer, size) = (parms[2], parms[3]);
                let value = self
                    .cvars
                    .find(&name)
                    .map(|cvar| cvar.string.clone())
                    .unwrap_or_default();
                write_string(data, buffer, size, &value);
                0
            }

            // The server's command line: `trap_Argc`/`trap_Argv` read it.
            g::ARGC => 0,
            g::ARGV => {
                write_string(data, parms[2], parms[3], "");
                0
            }

            g::FS_FOPEN_FILE => {
                let qpath = string(data, parms[1]);
                let out = parms[2];
                let mode = parms[3];
                let opened = if mode == 0 {
                    self.assets.as_ref().and_then(|assets| {
                        assets.read(&qpath).map(|bytes| bytes.into_owned())
                    })
                } else {
                    None
                };
                match opened {
                    Some(bytes) => {
                        let length = bytes.len() as i32;
                        self.files.push(Some(OpenFile {
                            bytes,
                            position: 0,
                        }));
                        if out != 0 {
                            store(data, out, self.files.len() as i32);
                        }
                        length
                    }
                    None => {
                        if out != 0 {
                            store(data, out, 0);
                        }
                        -1
                    }
                }
            }
            g::FS_READ => {
                let (buffer, length, handle) = (parms[1], parms[2], parms[3]);
                let Some(Some(file)) = self.files.get_mut(handle.max(0) as usize) else {
                    return 0;
                };
                let remaining = file.bytes.len().saturating_sub(file.position);
                let count = (length.max(0) as usize).min(remaining);
                let start = buffer.max(0) as usize;
                let mask = data.len() - 1;
                for i in 0..count {
                    let at = (start + i) & mask;
                    data[at] = file.bytes[file.position + i];
                }
                file.position += count;
                count as i32
            }
            g::FS_WRITE => parms[2],
            g::FS_FCLOSE_FILE => {
                if let Some(slot) = self.files.get_mut(parms[1].max(0) as usize) {
                    *slot = None;
                }
                0
            }
            g::FS_SEEK => {
                let (handle, offset, origin) = (parms[1], parms[2], parms[3]);
                let Some(Some(file)) = self.files.get_mut(handle.max(0) as usize) else {
                    return 0;
                };
                let base = match origin {
                    1 => file.position as i64,
                    2 => file.bytes.len() as i64,
                    _ => 0,
                };
                file.position = (base + offset as i64).clamp(0, file.bytes.len() as i64) as usize;
                file.position as i32
            }
            g::FS_GETFILELIST => {
                // Directory listings come from the pak index, which the assets
                // layer does not expose yet.
                self.stub(number);
                0
            }

            g::SEND_CONSOLE_COMMAND => {
                self.console.push((parms[1], string(data, parms[2])));
                0
            }
            g::SEND_SERVER_COMMAND => {
                self.server_commands
                    .push((parms[1], string(data, parms[2])));
                0
            }
            g::DROP_CLIENT => {
                self.server_commands.push((parms[1], string(data, parms[2])));
                0
            }
            g::LOCATE_GAME_DATA => {
                self.game_data = Some(GameData {
                    entities: parms[1],
                    entity_count: parms[2],
                    entity_size: parms[3],
                    clients: parms[4],
                    client_size: parms[5],
                });
                0
            }

            g::SET_CONFIGSTRING => {
                self.config_strings
                    .insert(parms[1], string(data, parms[2]));
                0
            }
            g::GET_CONFIGSTRING => {
                let value = self
                    .config_strings
                    .get(&parms[1])
                    .cloned()
                    .unwrap_or_default();
                write_string(data, parms[2], parms[3], &value);
                0
            }
            g::GET_USERINFO => {
                // `SV_GetUserinfo`: the string the server holds for that
                // client, empty for a client number it does not know.
                let info = self.userinfo.get(&parms[1]).cloned().unwrap_or_default();
                write_string(data, parms[2], parms[3], &info);
                0
            }
            g::SET_USERINFO => {
                self.userinfo.insert(parms[1], string(data, parms[2]));
                0
            }
            g::GET_SERVERINFO => {
                write_string(data, parms[1], parms[2], "");
                0
            }

            // `SV_Trace` against the world's brushes. The entity half of it
            // needs the grid, which is not here; neither is patch collision.
            g::TRACE | g::TRACECAPSULE => {
                self.trace(data, parms);
                0
            }
            g::POINT_CONTENTS => self.point_contents(data, parms),
            g::IN_PVS | g::IN_PVS_IGNORE_PORTALS => 1,
            g::AREAS_CONNECTED => 1,
            // The bounds half of collision is real; the world is not.
            g::SET_BRUSH_MODEL => {
                self.set_brush_model(data, parms[1], parms[2]);
                0
            }
            g::LINKENTITY => {
                self.link_entity(data, parms[1]);
                0
            }
            g::UNLINKENTITY => {
                self.unlink_entity(data, parms[1]);
                0
            }
            g::ADJUST_AREA_PORTAL_STATE => 0,
            g::ENTITIES_IN_BOX => 0,
            g::ENTITY_CONTACT | g::ENTITY_CONTACTCAPSULE => 0,
            g::BOT_ALLOCATE_CLIENT => -1,
            g::BOT_FREE_CLIENT => 0,
            g::GET_USERCMD => 0,

            // `SV_GetEntityToken`: one token per call, qfalse once the parse
            // point runs out, which is how the module ends its spawn parse.
            g::GET_ENTITY_TOKEN => match self.next_entity_token() {
                Some(token) => {
                    write_string(data, parms[1], parms[2], &token);
                    1
                }
                None => {
                    write_string(data, parms[1], parms[2], "");
                    0
                }
            },

            g::DEBUG_POLYGON_CREATE | g::DEBUG_POLYGON_DELETE => 0,
            g::REAL_TIME => 0,
            g::SNAPVECTOR => {
                for axis in 0..3 {
                    let cell = parms[1] + axis * 4;
                    let value = load_f32(data, cell);
                    store_f32(data, cell, (value + 0.5).floor());
                }
                0
            }

            // The shared C library, called by the module's `bg_lib.c`.
            trap::MEMSET => {
                fill(data, parms[1], parms[2] as u8, parms[3].max(0) as usize);
                0
            }
            trap::MEMCPY => {
                copy(data, parms[1], parms[2], parms[3].max(0) as usize);
                0
            }
            trap::STRNCPY => {
                copy_strncpy(data, parms[1], parms[2], parms[3].max(0) as usize);
                parms[1]
            }
            // Floats cross the import table as their bit patterns, which is
            // what `PASSFLOAT` and `FloatAsInt` do on the C side.
            trap::SIN => ((load_f32(data, parms[1]) as f64).sin() as f32).to_bits() as i32,
            trap::COS => ((load_f32(data, parms[1]) as f64).cos() as f32).to_bits() as i32,
            trap::ATAN2 => ((load_f32(data, parms[1]) as f64)
                .atan2(load_f32(data, parms[2]) as f64) as f32)
                .to_bits() as i32,
            trap::SQRT => load_f32(data, parms[1]).sqrt().to_bits() as i32,
            trap::FLOOR => load_f32(data, parms[1]).floor().to_bits() as i32,
            trap::CEIL => load_f32(data, parms[1]).ceil().to_bits() as i32,
            trap::MATRIXMULTIPLY => {
                matrix_multiply(data, parms[1], parms[2], parms[3]);
                0
            }
            trap::ANGLEVECTORS => {
                angle_vectors(data, parms[1], parms[2], parms[3], parms[4]);
                0
            }
            trap::PERPENDICULARVECTOR => {
                perpendicular_vector(data, parms[1], parms[2]);
                0
            }

            _ => {
                self.stub(number);
                0
            }
        }
    }

    /// `SV_SetBrushModel`: an inline `*N` model takes the bounds the map's
    /// submodel already spread by one unit, is marked a brush model, and is
    /// linked. `CM_InlineModel` has no contents of its own to copy, so the
    /// contents become -1, meaning "not known", exactly as the C version
    /// leaves them.
    ///
    /// The C version drops the server on a name that is not a brush model and
    /// on a model number the map does not have. Here the message lands in
    /// [`GameImports::errors`] and the entity is left untouched.
    fn set_brush_model(&mut self, data: &mut [u8], ent: i32, name: i32) {
        let name = string(data, name);
        if !name.starts_with('*') {
            self.errors
                .push(format!("SV_SetBrushModel: {name} isn't a brush model"));
            return;
        }

        let number = atoi(&name[1..]);
        let model = if number < 0 || number as usize >= MAX_SUBMODELS {
            None
        } else {
            self.models.get(number as usize).copied()
        };
        let Some(model) = model else {
            self.errors.push(format!(
                "SV_SetBrushModel: {name} is not a model this map has"
            ));
            return;
        };

        store(data, field(ent, shared_entity::S_MODELINDEX), number);
        for axis in 0..3 {
            store_f32(
                data,
                field(ent, shared_entity::R_MINS + axis * 4),
                model.mins[axis],
            );
            store_f32(
                data,
                field(ent, shared_entity::R_MAXS + axis * 4),
                model.maxs[axis],
            );
        }
        store(data, field(ent, shared_entity::R_BMODEL), 1);
        store(data, field(ent, shared_entity::R_CONTENTS), -1);
        self.link_entity(data, ent);
    }

    /// The part of `SV_LinkEntity` a box can be computed from without a world:
    /// the `s.solid` encoding clients predict with, the absolute box and the
    /// link count. The entity grid and its PVS leaf lists are not ported, so
    /// nothing enters a sector and `r.linked` records only that the entity has
    /// bounds.
    fn link_entity(&self, data: &mut [u8], ent: i32) {
        let bmodel = load(data, field(ent, shared_entity::R_BMODEL)) != 0;
        let contents = load(data, field(ent, shared_entity::R_CONTENTS));
        let mins = entity_vec(data, ent, shared_entity::R_MINS);
        let maxs = entity_vec(data, ent, shared_entity::R_MAXS);
        store(
            data,
            field(ent, shared_entity::S_SOLID),
            shared_entity::solid_value(bmodel, contents, mins, maxs),
        );

        let origin = entity_vec(data, ent, shared_entity::R_CURRENT_ORIGIN);
        let angles = entity_vec(data, ent, shared_entity::R_CURRENT_ANGLES);
        let (absmin, absmax) = shared_entity::absolute_box(bmodel, angles, origin, mins, maxs);
        for axis in 0..3 {
            store_f32(
                data,
                field(ent, shared_entity::R_ABSMIN + axis * 4),
                absmin[axis],
            );
            store_f32(
                data,
                field(ent, shared_entity::R_ABSMAX + axis * 4),
                absmax[axis],
            );
        }

        let count = load(data, field(ent, shared_entity::R_LINKCOUNT));
        store(
            data,
            field(ent, shared_entity::R_LINKCOUNT),
            count.wrapping_add(1),
        );
        store(data, field(ent, shared_entity::R_LINKED), 1);
    }

    /// `SV_UnlinkEntity`: the entity leaves the grid. There is no grid here, so
    /// only the flag changes.
    fn unlink_entity(&self, data: &mut [u8], ent: i32) {
        store(data, field(ent, shared_entity::R_LINKED), 0);
    }

    /// `SV_Trace`: the world's brushes, and then the solid entities, which
    /// this does not have yet.
    ///
    /// `G_TRACECAPSULE` comes through here too: without capsule collision the
    /// box around the capsule is swept, which blocks at the box corners where
    /// a capsule would slide past.
    fn trace(&self, data: &mut [u8], parms: &[i32; 10]) {
        let results = parms[1];
        if results == 0 {
            return;
        }
        let start = vec3(data, parms[2]);
        // `CM_BoxTrace` reads a null mins or maxs as the zero box.
        let mins = if parms[3] == 0 {
            [0.0; 3]
        } else {
            vec3(data, parms[3])
        };
        let maxs = if parms[4] == 0 {
            [0.0; 3]
        } else {
            vec3(data, parms[4])
        };
        let end = vec3(data, parms[5]);
        let brush_mask = parms[7];

        let mut trace = match &self.collision {
            Some(world) => world.box_trace(start, end, mins, maxs, brush_mask),
            None => {
                let mut trace = Trace::default();
                trace.endpos = end;
                trace
            }
        };
        // `SV_Trace` names the world as the entity that blocked the move.
        trace.entity_num = if trace.fraction == 1.0 {
            ENTITYNUM_NONE
        } else {
            ENTITYNUM_WORLD
        };
        write_trace(data, results, &trace);
    }

    /// `SV_PointContents`. The C version also ors in the contents of every
    /// entity the point touches; without the entity grid this is the world.
    fn point_contents(&self, data: &[u8], parms: &[i32; 10]) -> i32 {
        let point = vec3(data, parms[1]);
        self.collision
            .as_ref()
            .map(|world| world.point_contents(point))
            .unwrap_or(0)
    }

    fn stub(&mut self, number: i32) {
        *self.unimplemented.entry(number).or_insert(0) += 1;
    }

    /// `Cvar_Update`: the mirror is touched only when the engine's modification
    /// count has moved, and then all of it is refreshed. A handle that is out
    /// of range does nothing here where the C engine drops the client.
    fn update_mirror(&self, data: &mut [u8], target: i32) {
        let handle = load(data, target);
        let Some(name) = self.registered.get(handle.max(0) as usize) else {
            return;
        };
        let Some(cvar) = self.cvars.find(name) else {
            return;
        };
        let count = cvar.modification_count as i32;
        if count == load(data, target + 4) {
            return;
        }
        let (value, integer, text) = (cvar.value, cvar.integer, cvar.string.clone());
        store(data, target + 4, count);
        store_f32(data, target + 8, value);
        store(data, target + 12, integer);
        write_string(data, target + 16, MAX_CVAR_VALUE_STRING, &text);
    }
}

impl Default for GameImports {
    fn default() -> Self {
        Self::new()
    }
}

/// `VM_ArgPtr`: an offset that wraps through the data mask, as every pointer
/// a module passes does.
fn offset(data: &[u8], cell: i32) -> usize {
    (cell as u32 as usize) & (data.len() - 1)
}

fn string(data: &[u8], cell: i32) -> String {
    if cell == 0 {
        return String::new();
    }
    let start = offset(data, cell);
    let bytes = &data[start..];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn load(data: &[u8], cell: i32) -> i32 {
    let at = offset(data, cell);
    let quad = [
        data[at],
        data[(at + 1) & (data.len() - 1)],
        data[(at + 2) & (data.len() - 1)],
        data[(at + 3) & (data.len() - 1)],
    ];
    i32::from_le_bytes(quad)
}

fn load_f32(data: &[u8], cell: i32) -> f32 {
    f32::from_bits(load(data, cell) as u32)
}

fn store(data: &mut [u8], cell: i32, value: i32) {
    let at = offset(data, cell) & !3;
    let mask = data.len() - 1;
    for (i, byte) in value.to_le_bytes().iter().enumerate() {
        data[(at + i) & mask] = *byte;
    }
}

fn store_f32(data: &mut [u8], cell: i32, value: f32) {
    store(data, cell, value.to_bits() as i32);
}

/// Copies `text` and its terminator into the module's buffer, truncating to
/// the buffer the way `Q_strncpyz` does.
fn write_string(data: &mut [u8], cell: i32, size: i32, text: &str) {
    if cell == 0 || size <= 0 {
        return;
    }
    let mask = data.len() - 1;
    let size = (size as usize).min(MAX_WRITE);
    let start = offset(data, cell);
    let bytes = text.as_bytes();
    let count = bytes.len().min(size - 1);
    for i in 0..count {
        data[(start + i) & mask] = bytes[i];
    }
    for i in count..size {
        data[(start + i) & mask] = 0;
    }
}

/// `Com_Memset` into the module's segment. A VM pointer wraps through the data
/// mask, so a count larger than the segment keeps circulating rather than
/// faulting.
fn fill(data: &mut [u8], dst: i32, value: u8, count: usize) {
    let mask = data.len() - 1;
    let start = offset(data, dst);
    for i in 0..count {
        data[(start + i) & mask] = value;
    }
}

/// `Com_Memcpy` between two offsets in the segment, byte at a time so the
/// mask applies to every access.
fn copy(data: &mut [u8], dst: i32, src: i32, count: usize) {
    let mask = data.len() - 1;
    let (d, s) = (offset(data, dst), offset(data, src));
    for i in 0..count {
        let byte = data[(s + i) & mask];
        data[(d + i) & mask] = byte;
    }
}

/// C `strncpy`: stop after the terminator and pad the rest of the count with
/// zeroes.
fn copy_strncpy(data: &mut [u8], dst: i32, src: i32, count: usize) {
    let mask = data.len() - 1;
    let (d, s) = (offset(data, dst), offset(data, src));
    let mut i = 0;
    while i < count {
        let byte = data[(s + i) & mask];
        data[(d + i) & mask] = byte;
        i += 1;
        if byte == 0 {
            break;
        }
    }
    while i < count {
        data[(d + i) & mask] = 0;
        i += 1;
    }
}

fn vec3(data: &[u8], cell: i32) -> [f32; 3] {
    [
        load_f32(data, cell),
        load_f32(data, cell + 4),
        load_f32(data, cell + 8),
    ]
}

fn put_vec3(data: &mut [u8], cell: i32, value: [f32; 3]) {
    for (axis, component) in value.iter().enumerate() {
        store_f32(data, cell + axis as i32 * 4, *component);
    }
}

fn normalised(value: [f32; 3]) -> [f32; 3] {
    let length = (value[0] * value[0] + value[1] * value[1] + value[2] * value[2]).sqrt();
    if length == 0.0 {
        value
    } else {
        [value[0] / length, value[1] / length, value[2] / length]
    }
}

/// `MatrixMultiply` from `q_shared.c`: three row-major 3x3 matrices.
fn matrix_multiply(data: &mut [u8], left: i32, right: i32, out: i32) {
    let a = [
        vec3(data, left),
        vec3(data, left + 12),
        vec3(data, left + 24),
    ];
    let b = [
        vec3(data, right),
        vec3(data, right + 12),
        vec3(data, right + 24),
    ];
    for row in 0..3 {
        let mut product = [0.0f32; 3];
        for (column, slot) in product.iter_mut().enumerate() {
            *slot = a[row][0] * b[0][column]
                + a[row][1] * b[1][column]
                + a[row][2] * b[2][column];
        }
        put_vec3(data, out + row as i32 * 12, product);
    }
}

/// `AngleVectors` from `q_shared.c`: pitch, yaw and roll in degrees to the
/// three basis vectors. A caller that passes 0 for a basis still gets it
/// written, exactly as the C version does when the pointer is not null — in a
/// VM, offset 0 is addressable segment memory.
fn angle_vectors(data: &mut [u8], angles: i32, forward: i32, right: i32, up: i32) {
    const DEG2RAD: f32 = std::f32::consts::PI / 180.0;
    let angles = vec3(data, angles);
    let (sp, cp) = (angles[0] * DEG2RAD).sin_cos();
    let (sy, cy) = (angles[1] * DEG2RAD).sin_cos();
    let (sr, cr) = (angles[2] * DEG2RAD).sin_cos();

    put_vec3(data, forward, [cp * cy, cp * sy, -sp]);
    put_vec3(
        data,
        right,
        [-sr * sp * cy + cr * sy, -sr * sp * sy + cr * cy, -sr * cp],
    );
    put_vec3(
        data,
        up,
        [cr * sp * cy + sr * sy, cr * sp * sy - sr * cy, cr * cp],
    );
}

/// `PerpendicularVector` from `q_shared.c`: project the axis `src` is least
/// aligned with onto the plane through `src`, then normalise.
fn perpendicular_vector(data: &mut [u8], dst: i32, src: i32) {
    let source = vec3(data, src);
    let mut axis = 0usize;
    let mut smallest = 1.0f32;
    for (index, component) in source.iter().enumerate() {
        let magnitude = component.abs();
        if magnitude < smallest {
            axis = index;
            smallest = magnitude;
        }
    }
    let mut seed = [0.0f32; 3];
    seed[axis] = 1.0;

    let normal = normalised(source);
    let projection = normal[0] * seed[0] + normal[1] * seed[1] + normal[2] * seed[2];
    let plane = [
        seed[0] - normal[0] * projection,
        seed[1] - normal[1] * projection,
        seed[2] - normal[2] * projection,
    ];
    put_vec3(data, dst, normalised(plane));
}

/// An entity field's cell: the entity's offset plus the field's, wrapping the
/// way every VM pointer does.
fn field(ent: i32, offset: usize) -> i32 {
    ent.wrapping_add(offset as i32)
}

/// A `vec3_t` at a field inside an entity.
fn entity_vec(data: &[u8], ent: i32, offset: usize) -> [f32; 3] {
    let cell = field(ent, offset);
    [
        load_f32(data, cell),
        load_f32(data, cell + 4),
        load_f32(data, cell + 8),
    ]
}

/// C `atoi`: an optional sign and the leading digits, and zero when there are
/// none. `*12abc` is model 12, as `atoi( name + 1 )` gives it.
fn atoi(text: &str) -> i32 {
    let text = text.trim_start();
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    let value = digits[..end]
        .parse::<i64>()
        .map(|value| value.min(i32::MAX as i64) as i32)
        .unwrap_or(0);
    if negative {
        -value
    } else {
        value
    }
}

/// Writes a `trace_t` into the module's memory: `allsolid`, `startsolid`,
/// `fraction`, `endpos`, the plane with its type and sign bits, `surfaceFlags`,
/// `contents` and `entityNum`. The offsets are the C struct's, whose `cplane_t`
/// is twenty bytes: normal, distance, type, sign bits and two pad bytes.
fn write_trace(data: &mut [u8], results: i32, trace: &Trace) {
    if results == 0 {
        return;
    }
    store(data, results, trace.allsolid as i32);
    store(data, results + 4, trace.startsolid as i32);
    store_f32(data, results + 8, trace.fraction);
    for axis in 0..3 {
        store_f32(data, results + 12 + axis * 4, trace.endpos[axis as usize]);
        store_f32(
            data,
            results + 24 + axis * 4,
            trace.plane.normal[axis as usize],
        );
    }
    store_f32(data, results + 36, trace.plane.dist);
    // `type` and `signbits` share a word with the plane's two pad bytes.
    store(
        data,
        results + 40,
        trace.plane.kind as i32 | (trace.plane.signbits as i32) << 8,
    );
    store(data, results + 44, trace.surface_flags);
    store(data, results + 48, trace.contents);
    store(data, results + 52, trace.entity_num);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blank data segment, with a string written at `at`.
    fn segment_with(at: usize, text: &str) -> Vec<u8> {
        let mut data = vec![0u8; 4096];
        data[at..at + text.len()].copy_from_slice(text.as_bytes());
        data
    }

    fn parms(values: &[i32]) -> [i32; 10] {
        let mut parms = [0i32; 10];
        for (i, value) in values.iter().enumerate() {
            parms[i] = *value;
        }
        parms
    }

    #[test]
    fn printing_collects_the_module_text() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "hello from qagame");
        assert_eq!(imports.call(&mut data, g::PRINT, &parms(&[0, 64])), 0);
        assert_eq!(imports.printed, vec!["hello from qagame".to_string()]);
    }

    #[test]
    fn registering_a_cvar_writes_the_vm_mirror() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "sv_maxclients");
        data[128..129].copy_from_slice(b"8");
        let call = parms(&[0, 100, 64, 128, 0]);
        assert_eq!(imports.call(&mut data, g::CVAR_REGISTER, &call), 0);

        assert_eq!(load(&data, 100), 0, "the first handle is 0");
        assert_eq!(load_f32(&data, 100 + 8), 8.0);
        assert_eq!(load(&data, 100 + 12), 8);
        assert_eq!(&data[116..118], b"8\0", "the string is copied in place");
        assert_eq!(imports.cvars().find("sv_maxclients").unwrap().integer, 8);
    }

    #[test]
    fn a_cvar_update_copies_again_only_when_the_count_moves() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "g_gametype");
        data[128..129].copy_from_slice(b"0");
        imports.call(&mut data, g::CVAR_REGISTER, &parms(&[0, 100, 64, 128, 0]));

        let _ = imports.cvars_mut().set("g_gametype", "4", true, true);
        // A mirror left one count behind is refreshed; a current one is not.
        store(&mut data, 100 + 4, -2);
        data[116..120].copy_from_slice(b"old\0");
        imports.call(&mut data, g::CVAR_UPDATE, &parms(&[0, 100]));
        assert_eq!(&data[116..118], b"4\0");
        assert_eq!(load(&data, 100 + 12), 4);

        let count = load(&data, 100 + 4);
        data[116..119].copy_from_slice(b"zz\0");
        imports.call(&mut data, g::CVAR_UPDATE, &parms(&[0, 100]));
        assert_eq!(load(&data, 100 + 4), count);
        assert_eq!(&data[116..119], b"zz\0", "no copy while in step");
    }

    #[test]
    fn registering_the_same_name_twice_keeps_one_handle() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "g_gametype");
        data[128..129].copy_from_slice(b"3");
        imports.call(&mut data, g::CVAR_REGISTER, &parms(&[0, 100, 64, 128, 0]));
        imports.call(&mut data, g::CVAR_REGISTER, &parms(&[0, 200, 64, 128, 0]));
        assert_eq!(load(&data, 100), load(&data, 200));
    }

    #[test]
    fn a_cvar_is_read_back_through_the_integer_import() {
        let mut imports = GameImports::new();
        imports.seed("sv_maxclients", "12");
        let data = segment_with(64, "sv_maxclients");
        assert_eq!(
            imports.call(&mut data.clone(), g::CVAR_VARIABLE_INTEGER_VALUE, &parms(&[0, 64])),
            12
        );
    }

    #[test]
    fn a_cvar_string_is_copied_into_the_module_buffer() {
        let mut imports = GameImports::new();
        imports.seed("mapname", "q3dm17");
        let mut data = segment_with(64, "mapname");
        imports.call(
            &mut data,
            g::CVAR_VARIABLE_STRING_BUFFER,
            &parms(&[0, 64, 256, 32]),
        );
        assert_eq!(&data[256..263], b"q3dm17\0");
    }

    #[test]
    fn userinfo_round_trips_through_the_imports() {
        let mut imports = GameImports::new();
        imports.set_userinfo(2, "\\name\\Player");

        let mut data = segment_with(64, "unused");
        imports.call(&mut data, g::GET_USERINFO, &parms(&[0, 2, 256, 128]));
        assert_eq!(&data[256..269], b"\\name\\Player\0");

        let mut data = segment_with(64, "\\name\\Bot");
        imports.call(&mut data, g::SET_USERINFO, &parms(&[0, 3, 64]));
        assert_eq!(imports.userinfo(3), Some("\\name\\Bot"));
    }

    #[test]
    fn a_brush_model_takes_the_map_bounds_and_links() {
        let mut imports = GameImports::new();
        imports.set_models(vec![BspModel {
            mins: [-16.0, -16.0, -24.0],
            maxs: [16.0, 16.0, 40.0],
        }]);
        let mut data = segment_with(64, "*0");
        imports.call(&mut data, g::SET_BRUSH_MODEL, &parms(&[0, 1024, 64]));

        assert_eq!(load(&data, 1024 + shared_entity::S_MODELINDEX as i32), 0);
        assert_eq!(load(&data, 1024 + shared_entity::R_BMODEL as i32), 1);
        assert_eq!(load(&data, 1024 + shared_entity::R_CONTENTS as i32), -1);
        assert_eq!(
            load_f32(&data, 1024 + shared_entity::R_MINS as i32),
            -16.0
        );
        assert_eq!(
            load_f32(&data, 1024 + shared_entity::R_MAXS as i32 + 8),
            40.0
        );
        assert_eq!(
            load(&data, 1024 + shared_entity::S_SOLID as i32),
            shared_entity::SOLID_BMODEL
        );
        assert_eq!(load(&data, 1024 + shared_entity::R_LINKED as i32), 1);
        assert_eq!(load(&data, 1024 + shared_entity::R_LINKCOUNT as i32), 1);
        // The absolute box is the unit-spread bounds around the origin.
        assert_eq!(
            load_f32(&data, 1024 + shared_entity::R_ABSMIN as i32),
            -17.0
        );
        assert_eq!(
            load_f32(&data, 1024 + shared_entity::R_ABSMAX as i32 + 8),
            41.0
        );
    }

    #[test]
    fn a_linked_entity_can_be_unlinked() {
        let mut imports = GameImports::new();
        imports.set_models(vec![BspModel {
            mins: [-1.0; 3],
            maxs: [1.0; 3],
        }]);
        let mut data = segment_with(64, "*0");
        imports.call(&mut data, g::SET_BRUSH_MODEL, &parms(&[0, 1024, 64]));
        assert_eq!(load(&data, 1024 + shared_entity::R_LINKED as i32), 1);

        imports.call(&mut data, g::UNLINKENTITY, &parms(&[0, 1024]));
        assert_eq!(load(&data, 1024 + shared_entity::R_LINKED as i32), 0);
    }

    #[test]
    fn a_model_name_that_is_not_a_brush_model_is_reported() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "models/foo.md3");
        imports.call(&mut data, g::SET_BRUSH_MODEL, &parms(&[0, 1024, 64]));
        assert_eq!(imports.errors.len(), 1);
        assert_eq!(load(&data, 1024 + shared_entity::R_BMODEL as i32), 0);
    }

    #[test]
    fn an_inline_model_number_the_map_does_not_have_is_reported() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "*7");
        imports.call(&mut data, g::SET_BRUSH_MODEL, &parms(&[0, 1024, 64]));
        assert_eq!(imports.errors.len(), 1);
    }

    #[test]
    fn atoi_reads_the_leading_digits() {
        assert_eq!(atoi("0"), 0);
        assert_eq!(atoi("12abc"), 12);
        assert_eq!(atoi("-3"), -3);
        assert_eq!(atoi("abc"), 0);
        assert_eq!(atoi("  8"), 8);
    }

    #[test]
    fn config_strings_round_trip() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "mapname\\q3dm1");
        imports.call(&mut data, g::SET_CONFIGSTRING, &parms(&[0, 5, 64]));
        imports.call(&mut data, g::GET_CONFIGSTRING, &parms(&[0, 5, 256, 32]));
        assert_eq!(&data[256..270], b"mapname\\q3dm1\0");
    }

    #[test]
    fn the_entity_lump_is_handed_out_one_token_at_a_time() {
        let mut imports = GameImports::new();
        imports.set_entity_string(b"{ \"classname\" \"worldspawn\" // the world\n}");
        let mut data = vec![0u8; 4096];

        let mut tokens = Vec::new();
        while imports.call(&mut data, g::GET_ENTITY_TOKEN, &parms(&[0, 64, 32])) != 0 {
            tokens.push(string(&data, 64));
        }
        assert_eq!(tokens, ["{", "classname", "worldspawn", "}"]);
        assert_eq!(data[64], 0, "the exhausted call clears the buffer");
    }

    #[test]
    fn a_quoted_entity_value_keeps_its_spaces() {
        let mut imports = GameImports::new();
        imports.set_entity_string(b"{ \"message\" \"hello there\" }");
        let mut data = vec![0u8; 4096];
        let mut tokens = Vec::new();
        while imports.call(&mut data, g::GET_ENTITY_TOKEN, &parms(&[0, 64, 1024])) != 0 {
            tokens.push(string(&data, 64));
        }
        assert_eq!(tokens, ["{", "message", "hello there", "}"]);
    }

    #[test]
    fn a_missing_file_opens_as_minus_one() {
        let mut imports = GameImports::new();
        let mut data = segment_with(64, "maps/q3dm1.bsp");
        let length = imports.call(&mut data, g::FS_FOPEN_FILE, &parms(&[0, 64, 256, 0]));
        assert_eq!(length, -1);
        assert_eq!(load(&data, 256), 0);
    }

    #[test]
    fn a_trace_that_hits_nothing_reports_the_endpoint() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        for (axis, value) in [1.5f32, 2.5, 3.5].iter().enumerate() {
            let bits = value.to_bits();
            data[256 + axis * 4..260 + axis * 4].copy_from_slice(&bits.to_le_bytes());
        }
        imports.call(&mut data, g::TRACE, &parms(&[0, 512, 0, 0, 0, 256, 0, 0]));
        assert_eq!(load_f32(&data, 512 + 8), 1.0);
        assert_eq!(load_f32(&data, 512 + 12), 1.5);
        assert_eq!(load_f32(&data, 512 + 20), 3.5);
        assert_eq!(load(&data, 512 + 52), ENTITYNUM_NONE);
    }

    #[test]
    fn a_trace_through_the_world_writes_every_field_the_game_reads() {
        let mut imports = GameImports::new();
        imports.set_collision(crate::collision::tests::solid_cube());
        let mut data = vec![0u8; 4096];
        let (start, end) = (64usize, 96usize);
        for (axis, value) in [0.0f32, 0.0, 100.0].iter().enumerate() {
            data[start + axis * 4..start + axis * 4 + 4]
                .copy_from_slice(&value.to_bits().to_le_bytes());
        }
        for (axis, value) in [0.0f32, 0.0, -100.0].iter().enumerate() {
            data[end + axis * 4..end + axis * 4 + 4]
                .copy_from_slice(&value.to_bits().to_le_bytes());
        }

        let results = 1024;
        imports.call(
            &mut data,
            g::TRACE,
            &parms(&[
                0,
                results,
                start as i32,
                0,
                0,
                end as i32,
                1023,
                crate::collision::CONTENTS_SOLID,
            ]),
        );

        // The floor at z = 64, one epsilon in front of it.
        assert!((load_f32(&data, results + 8) - 0.179375).abs() < 1e-6);
        assert!((load_f32(&data, results + 20) - 64.125).abs() < 1e-3);
        assert_eq!(load_f32(&data, results + 24), 0.0);
        assert_eq!(load_f32(&data, results + 32), 1.0);
        assert_eq!(load_f32(&data, results + 36), 64.0);
        assert_eq!(load(&data, results + 40) & 0xff, 2, "an axial z plane");
        assert_eq!(load(&data, results + 44), 0x4, "the shader's surface flags");
        assert_eq!(
            load(&data, results + 48),
            crate::collision::CONTENTS_SOLID
        );
        assert_eq!(load(&data, results + 52), ENTITYNUM_WORLD);
        assert_eq!(load(&data, results), 0, "not all solid");
    }

    #[test]
    fn point_contents_answers_from_the_world() {
        let mut imports = GameImports::new();
        imports.set_collision(crate::collision::tests::solid_cube());
        let mut data = vec![0u8; 4096];

        // The origin is inside the cube.
        assert_eq!(
            imports.call(&mut data, g::POINT_CONTENTS, &parms(&[0, 64, 1023])),
            crate::collision::CONTENTS_SOLID
        );

        // Move the point outside it.
        for (axis, value) in [200.0f32, 0.0, 0.0].iter().enumerate() {
            data[64 + axis * 4..64 + axis * 4 + 4]
                .copy_from_slice(&value.to_bits().to_le_bytes());
        }
        assert_eq!(
            imports.call(&mut data, g::POINT_CONTENTS, &parms(&[0, 64, 1023])),
            0
        );
    }

    #[test]
    fn snapvector_rounds_to_nearest() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        for (axis, value) in [1.4f32, -2.6, 0.5].iter().enumerate() {
            data[64 + axis * 4..68 + axis * 4].copy_from_slice(&value.to_bits().to_le_bytes());
        }
        imports.call(&mut data, g::SNAPVECTOR, &parms(&[0, 64]));
        assert_eq!(load_f32(&data, 64), 1.0);
        assert_eq!(load_f32(&data, 68), -3.0);
        assert_eq!(load_f32(&data, 72), 1.0);
    }

    #[test]
    fn memset_and_memcpy_reach_the_module_data() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        imports.call(&mut data, trap::MEMSET, &parms(&[0, 64, 0x5a, 16]));
        assert!(data[64..80].iter().all(|&byte| byte == 0x5a));
        assert_eq!(data[80], 0, "the fill stops at the count");

        data[128..132].copy_from_slice(b"abcd");
        imports.call(&mut data, trap::MEMCPY, &parms(&[0, 96, 128, 4]));
        assert_eq!(&data[96..100], b"abcd");
    }

    #[test]
    fn strncpy_pads_the_count_with_zeroes() {
        let mut imports = GameImports::new();
        let mut data = vec![0xffu8; 4096];
        data[128..135].copy_from_slice(b"hi\0junk");
        let returned = imports.call(&mut data, trap::STRNCPY, &parms(&[0, 96, 128, 6]));
        assert_eq!(returned, 96, "strncpy returns the destination");
        assert_eq!(&data[96..102], b"hi\0\0\0\0");
    }

    #[test]
    fn the_library_floats_arrive_as_bit_patterns() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];

        store_f32(&mut data, 64, 9.0);
        let root = imports.call(&mut data, trap::SQRT, &parms(&[0, 64]));
        assert_eq!(f32::from_bits(root as u32), 3.0);

        store_f32(&mut data, 64, 1.75);
        let floor = imports.call(&mut data, trap::FLOOR, &parms(&[0, 64]));
        assert_eq!(f32::from_bits(floor as u32), 1.0);
        let ceil = imports.call(&mut data, trap::CEIL, &parms(&[0, 64]));
        assert_eq!(f32::from_bits(ceil as u32), 2.0);

        store_f32(&mut data, 64, 0.0);
        let sin = imports.call(&mut data, trap::SIN, &parms(&[0, 64]));
        assert_eq!(f32::from_bits(sin as u32), 0.0);
    }

    #[test]
    fn matrix_multiply_is_row_major() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        for i in 0..9 {
            store_f32(&mut data, 64 + i * 4, if i % 4 == 0 { 1.0 } else { 0.0 });
            store_f32(&mut data, 128 + i * 4, (i + 1) as f32);
        }
        imports.call(&mut data, trap::MATRIXMULTIPLY, &parms(&[0, 64, 128, 192]));
        for i in 0..9 {
            assert_eq!(load_f32(&data, 192 + i * 4), (i + 1) as f32);
        }
    }

    #[test]
    fn zero_angles_give_the_identity_basis() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        imports.call(
            &mut data,
            trap::ANGLEVECTORS,
            &parms(&[0, 64, 80, 96, 112]),
        );
        assert_eq!(vec3(&data, 80), [1.0, 0.0, 0.0]);
        assert_eq!(vec3(&data, 96), [0.0, 1.0, 0.0]);
        assert_eq!(vec3(&data, 112), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn perpendicular_vector_is_unit_length_and_square_to_its_source() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        // A source already off the axes, so no axis can be taken by accident.
        put_vec3(&mut data, 64, [1.0, 1.0, 0.0]);
        imports.call(&mut data, trap::PERPENDICULARVECTOR, &parms(&[0, 128, 64]));
        let perpendicular = vec3(&data, 128);
        let length = (perpendicular[0] * perpendicular[0]
            + perpendicular[1] * perpendicular[1]
            + perpendicular[2] * perpendicular[2])
            .sqrt();
        assert!((length - 1.0).abs() < 1e-6, "length {length}");
        let dot = perpendicular[0] + perpendicular[1];
        assert!(dot.abs() < 1e-6, "dot {dot}");
    }

    #[test]
    fn an_import_with_no_engine_behind_it_is_counted() {
        let mut imports = GameImports::new();
        let mut data = vec![0u8; 4096];
        assert_eq!(imports.call(&mut data, 345, &parms(&[])), 0);
        assert_eq!(imports.unimplemented.get(&345), Some(&1));
    }

    #[test]
    fn the_numbering_matches_the_c_enum() {
        // Spot checks against `gameImport_t`: the first import, the first
        // filesystem one, the entity token, and the botlib block start.
        assert_eq!(g::PRINT, 0);
        assert_eq!(g::FS_FOPEN_FILE, 10);
        assert_eq!(g::GET_ENTITY_TOKEN, 37);
        assert_eq!(g::FS_SEEK, 45);
        // `sharedTraps_t`: the asm's `memset -101` arrives as import 100.
        assert_eq!(trap::MEMSET, 100);
        assert_eq!(trap::MEMCPY, 101);
        assert_eq!(trap::STRNCPY, 102);
        assert_eq!(trap::CEIL, 111);
        assert_eq!(g::BOTLIB_SETUP, 200);
    }
}
