//! The game module: `vm/<name>.qvm` loaded through the assets layer, the
//! `gameExport_t` commands, and the server's side of the call.
//!
//! The QVM has no export table and no `dllEntry` — `vmMain` is required to be
//! the first function in the image (`g_main.c` says so above its definition) —
//! so `VM_Call(vm, GAME_INIT, ...)` is just the interpreter starting at code
//! offset 0 with the command in the first argument cell. [`Game::call`] is
//! that, with [`GameImports`] answering the module's imports as it runs.
//!
//! The command names are `gameExport_t`: `GAME_INIT` 0 through
//! `BOTAI_START_FRAME` 10. `GAME_API_VERSION` is 8 for this build, which is
//! what an out-of-tree `qagame` compiled against the same headers expects.

use std::fmt;

use crate::assets::Assets;
use crate::bsp::{self, Bsp};
use crate::collision::Collision;
use crate::game_imports::GameImports;
use crate::vm::{Vm, VmError};

/// `GAME_API_VERSION` from `g_public.h`.
pub const GAME_API_VERSION: i32 = 8;

/// The module's entry points, in `gameExport_t` order.
pub mod game_export {
    pub const GAME_INIT: i32 = 0;
    pub const GAME_SHUTDOWN: i32 = 1;
    pub const GAME_CLIENT_CONNECT: i32 = 2;
    pub const GAME_CLIENT_BEGIN: i32 = 3;
    pub const GAME_CLIENT_USERINFO_CHANGED: i32 = 4;
    pub const GAME_CLIENT_DISCONNECT: i32 = 5;
    pub const GAME_CLIENT_COMMAND: i32 = 6;
    pub const GAME_CLIENT_THINK: i32 = 7;
    pub const GAME_RUN_FRAME: i32 = 8;
    pub const GAME_CONSOLE_COMMAND: i32 = 9;
    pub const BOTAI_START_FRAME: i32 = 10;
}

#[derive(Debug)]
pub enum GameError {
    /// `vm/<name>.qvm` is not in any mounted pak.
    Missing(String),
    /// The image is there but the interpreter will not load it.
    Vm(VmError),
}

impl fmt::Display for GameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GameError::Missing(path) => write!(f, "{path} is not in any mounted pak"),
            GameError::Vm(e) => write!(f, "loading the module: {e}"),
        }
    }
}

impl std::error::Error for GameError {}

impl From<VmError> for GameError {
    fn from(e: VmError) -> Self {
        GameError::Vm(e)
    }
}

#[derive(Debug)]
pub enum MapError {
    /// `maps/<map>.bsp` is not in any mounted pak.
    Missing(String),
    /// The file is there but is not a Q3 BSP.
    Bsp(bsp::BspError),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::Missing(path) => write!(f, "{path} is not in any mounted pak"),
            MapError::Bsp(e) => write!(f, "reading the map: {e}"),
        }
    }
}

impl std::error::Error for MapError {}

impl From<bsp::BspError> for MapError {
    fn from(e: bsp::BspError) -> Self {
        MapError::Bsp(e)
    }
}

/// A loaded game module and the imports it calls.
pub struct Game {
    vm: Vm,
    imports: GameImports,
}

impl Game {
    /// Loads `vm/<name>.qvm`, the path `VM_Restart` rebuilds, and gives the
    /// module the paks to read its own files from.
    pub fn load(assets: Assets, name: &str) -> Result<Self, GameError> {
        let qpath = format!("vm/{name}.qvm");
        let bytes = assets
            .read(&qpath)
            .ok_or_else(|| GameError::Missing(qpath.clone()))?
            .into_owned();
        let vm = Vm::load(name, &bytes)?;
        Ok(Game {
            vm,
            imports: GameImports::new().with_assets(assets),
        })
    }

    /// Loads a module from bytes already in hand, with no pak behind it.
    pub fn from_bytes(name: &str, bytes: &[u8]) -> Result<Self, GameError> {
        Ok(Game {
            vm: Vm::load(name, bytes)?,
            imports: GameImports::new(),
        })
    }

    /// Gives the module a map's entity lump before `GAME_INIT`, as
    /// `SV_InitGameVM` sets `sv.entityParsePoint = CM_EntityString()`. The map
    /// name goes into `mapname`, which the module registers at init.
    pub fn start_map(&mut self, map: &str) -> Result<(), MapError> {
        let path = format!("maps/{map}.bsp");
        let bytes = self
            .imports
            .assets()
            .and_then(|assets| assets.read(&path))
            .ok_or_else(|| MapError::Missing(path.clone()))?
            .into_owned();
        let bsp = Bsp::load(bytes)?;
        // `CM_LoadMap` has the submodels in hand before the game asks for one
        // through `trap_SetBrushModel`, so they arrive here with the entity
        // lump.
        self.imports.set_models(bsp.models()?);
        // `CM_LoadMap` has the whole collision model built before the game runs
        // its first trace.
        self.imports.set_collision(Collision::load(&bsp)?);
        self.imports.set_entity_string(bsp.entities());
        self.imports.seed("mapname", map);
        Ok(())
    }

    pub fn name(&self) -> &str {
        self.vm.name()
    }

    /// Reads a string the module returned as an offset, such as the refusal
    /// text from `GAME_CLIENT_CONNECT`. The server has to print that text.
    pub fn string_at(&self, cell: i32) -> Option<&str> {
        self.vm.string_at(cell)
    }

    /// The shared entity the module reported through `G_LOCATE_GAME_DATA`. That
    /// address is the start of its `gentity_t`, so this is where the server
    /// reads `s` and `r` from when it links an entity or builds a snapshot. The
    /// slice is the module's own stride, so it covers `sharedEntity_t` and every
    /// field the game put after it.
    pub fn entity(&self, index: i32) -> Option<&[u8]> {
        let data = self.imports.game_data?;
        if index < 0 || index >= data.entity_count || data.entity_size <= 0 {
            return None;
        }
        let base = (data.entities as u32 as usize) & self.vm.data_mask() as usize;
        let start = base + index as usize * data.entity_size as usize;
        let bytes = self.vm.data().get(start..)?;
        bytes.get(..data.entity_size as usize)
    }

    pub fn imports(&self) -> &GameImports {
        &self.imports
    }

    pub fn imports_mut(&mut self) -> &mut GameImports {
        &mut self.imports
    }

    /// `VM_Call`: enters `vmMain` with `command` in the first argument cell.
    pub fn call(&mut self, command: i32, args: &[i32]) -> Result<i32, VmError> {
        let Game { vm, imports } = self;
        vm.call(command, args, |data, number, parms| {
            imports.call(data, number, parms)
        })
    }

    /// `GAME_INIT`, called once per level. The module parses its own entities
    /// through `G_GET_ENTITY_TOKEN`, which reads the lump `start_map` gave it.
    pub fn init(&mut self, level_time: i32, random_seed: i32, restart: bool) -> Result<i32, VmError> {
        self.call(
            game_export::GAME_INIT,
            &[level_time, random_seed, restart as i32],
        )
    }

    pub fn shutdown(&mut self, restart: bool) -> Result<i32, VmError> {
        self.call(game_export::GAME_SHUTDOWN, &[restart as i32])
    }

    pub fn run_frame(&mut self, level_time: i32) -> Result<i32, VmError> {
        self.call(game_export::GAME_RUN_FRAME, &[level_time])
    }

    pub fn client_begin(&mut self, client_num: i32) -> Result<i32, VmError> {
        self.call(game_export::GAME_CLIENT_BEGIN, &[client_num])
    }

    pub fn client_think(&mut self, client_num: i32) -> Result<i32, VmError> {
        self.call(game_export::GAME_CLIENT_THINK, &[client_num])
    }

    pub fn client_disconnect(&mut self, client_num: i32) -> Result<i32, VmError> {
        self.call(game_export::GAME_CLIENT_DISCONNECT, &[client_num])
    }

    /// Returns the module's offset for the refusal string, or 0 when the
    /// client is allowed in.
    pub fn client_connect(
        &mut self,
        client_num: i32,
        first_time: bool,
        is_bot: bool,
    ) -> Result<i32, VmError> {
        self.call(
            game_export::GAME_CLIENT_CONNECT,
            &[client_num, first_time as i32, is_bot as i32],
        )
    }

    /// Returns true when the module recognised the command it was given,
    /// which is `qfalse` in the C code, so the server only falls back to its
    /// own table on 0.
    pub fn console_command(&mut self) -> Result<bool, VmError> {
        Ok(self.call(game_export::GAME_CONSOLE_COMMAND, &[])? != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{op, VM_MAGIC};

    const HEADER_LEN: usize = 32;
    const DATA_LEN: usize = 256;

    /// Bytes of operand that follow each opcode these tests emit. The real
    /// table is private to the interpreter, which this image is fed through
    /// anyway, so a wrong width here would show up as `TruncatedCode`.
    fn width(opcode: u8) -> usize {
        match opcode {
            op::ARG => 1,
            op::ENTER | op::CONST | op::LOCAL | op::LEAVE => 4,
            _ => 0,
        }
    }

    /// An image with `instructions` as its code and `strings` copied into the
    /// data segment at their offsets.
    fn image(instructions: &[(u8, i32)], strings: &[(usize, &str)]) -> Vec<u8> {
        let mut code = Vec::new();
        for &(opcode, operand) in instructions {
            code.push(opcode);
            match width(opcode) {
                1 => code.push(operand as u8),
                4 => code.extend_from_slice(&operand.to_le_bytes()),
                _ => {}
            }
        }

        let data_offset = HEADER_LEN + code.len();
        let mut image = Vec::new();
        for field in [
            VM_MAGIC as i32,
            instructions.len() as i32,
            HEADER_LEN as i32,
            code.len() as i32,
            data_offset as i32,
            DATA_LEN as i32,
            0,
            0,
        ] {
            image.extend_from_slice(&field.to_le_bytes());
        }
        image.extend_from_slice(&code);
        image.resize(data_offset + DATA_LEN, 0);
        for (at, text) in strings {
            image[data_offset + at..data_offset + at + text.len()].copy_from_slice(text.as_bytes());
        }
        image
    }

    /// `return command + 1;`, which proves the command reached the module.
    fn echo_command() -> Vec<u8> {
        image(
            &[
                (op::ENTER, 8),
                (op::LOCAL, 16),
                (op::LOAD4, 0),
                (op::CONST, 1),
                (op::ADD, 0),
                (op::LEAVE, 8),
            ],
            &[],
        )
    }

    fn assets_with(name: &str, files: &[(&str, Vec<u8>)]) -> Assets {
        let dir = std::env::temp_dir().join(format!("q3rust-game-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entries: Vec<(&str, &[u8], u16)> = files
            .iter()
            .map(|(path, bytes)| (*path, bytes.as_slice(), 0))
            .collect();
        std::fs::write(
            dir.join("pak0.pk3"),
            crate::pak::tests::build_zip(b"", &entries),
        )
        .unwrap();
        let assets = Assets::mount(&dir).unwrap();
        // The mapping stays valid after the directory goes, as the client's
        // paks do.
        std::fs::remove_dir_all(&dir).unwrap();
        assets
    }

    #[test]
    fn the_command_numbers_match_game_export() {
        assert_eq!(game_export::GAME_INIT, 0);
        assert_eq!(game_export::GAME_RUN_FRAME, 8);
        assert_eq!(game_export::BOTAI_START_FRAME, 10);
        assert_eq!(GAME_API_VERSION, 8);
    }

    #[test]
    fn the_call_number_reaches_vm_main() {
        let mut game = Game::from_bytes("qagame", &echo_command()).unwrap();
        assert_eq!(game.call(game_export::GAME_RUN_FRAME, &[]), Ok(9));
        assert_eq!(game.run_frame(0), Ok(9));
    }

    #[test]
    fn an_import_runs_against_the_module_data_segment() {
        // trap_Print( "hi" ): push the string, put it in argument 0, call
        // import 0 (which the module spells -1), and return its result.
        let bytes = image(
            &[
                (op::ENTER, 16),
                (op::CONST, 64),
                (op::ARG, 8),
                (op::CONST, -1),
                (op::CALL, 0),
                (op::LEAVE, 16),
            ],
            &[(64, "hi\0")],
        );
        let mut game = Game::from_bytes("qagame", &bytes).unwrap();
        assert_eq!(game.init(0, 0, false), Ok(0));
        assert_eq!(game.imports().printed, vec!["hi".to_string()]);
    }

    #[test]
    fn the_module_is_read_out_of_the_paks() {
        let assets = assets_with("pak", &[("vm/qagame.qvm", echo_command())]);
        let mut game = Game::load(assets, "qagame").unwrap();
        assert_eq!(game.name(), "qagame");
        assert_eq!(game.call(game_export::GAME_INIT, &[0, 0, 0]), Ok(1));
    }

    #[test]
    fn a_module_that_is_not_there_names_the_path_it_looked_for() {
        let assets = assets_with("missing", &[("readme.txt", b"nothing".to_vec())]);
        match Game::load(assets, "qagame") {
            Err(GameError::Missing(path)) => assert_eq!(path, "vm/qagame.qvm"),
            Err(other) => panic!("expected a missing module, got {other}"),
            Ok(_) => panic!("a module that is not in the pak was loaded"),
        }
    }

    #[test]
    fn starting_a_map_hands_the_module_the_entity_lump() {
        let entity = b"{ \"classname\" \"worldspawn\" \"message\" \"test map\" }\n";
        let assets = assets_with(
            "map",
            &[
                ("vm/qagame.qvm", echo_command()),
                ("maps/q3dm1.bsp", crate::bsp::tests::build_bsp(entity)),
            ],
        );
        let mut game = Game::load(assets, "qagame").unwrap();
        game.start_map("q3dm1").unwrap();

        let mut tokens = Vec::new();
        while let Some(token) = game.imports_mut().next_entity_token() {
            tokens.push(token);
        }
        assert_eq!(
            tokens,
            ["{", "classname", "worldspawn", "message", "test map", "}"]
        );
        assert_eq!(
            game.imports().cvars().find("mapname").unwrap().string,
            "q3dm1"
        );
    }

    #[test]
    fn a_map_that_is_not_in_the_paks_names_the_path() {
        let assets = assets_with("nomap", &[("vm/qagame.qvm", echo_command())]);
        let mut game = Game::load(assets, "qagame").unwrap();
        match game.start_map("q3dm9") {
            Err(MapError::Missing(path)) => assert_eq!(path, "maps/q3dm9.bsp"),
            Err(other) => panic!("expected a missing map, got {other}"),
            Ok(_) => panic!("a map that is not in the pak was started"),
        }
    }
}
