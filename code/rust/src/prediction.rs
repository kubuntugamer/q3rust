//! Client-side movement prediction: the map's collision model and a player's
//! `pmove_t`, driven by the keys the client is holding.
//!
//! This is the client half of `bg_pmove.c`. The server has its own copy in the
//! game module; here the same [`crate::bg_pmove`] code runs against the map's
//! brushes, so the view moves without waiting for anything from the network.
//! `CL_BaseMove` and the usercmd packing in `cl_input.c` are what turn held keys
//! into a [`UserCmd`]; the move itself is the shared code.
//!
//! The collision model is leaked to `'static`. [`Pmove`] borrows the world it
//! sweeps against and the client owns both, so a leaked box gives the borrow a
//! stable address without making the player self-referential. The game data
//! layer keeps its pak mappings alive the same way.

use crate::assets::Assets;
use crate::bg_pmove::{pmove, Pmove, UserCmd, ENTITYNUM_NONE};
use crate::bsp::Bsp;
use crate::collision::Collision;
use crate::game::MapError;

/// The full deflection of a `usercmd_t` move axis: the C client's 127.
const MOVE_SCALE: i32 = 127;
/// Degrees per second the turn keys rotate the view.
const TURN_SPEED: f32 = 180.0;

/// The movement keys the client is holding. The client sets these from SDL
/// key events; [`Prediction::step`] turns them into a command.
#[derive(Clone, Copy, Debug, Default)]
pub struct Input {
    pub forward: bool,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub turn_left: bool,
    pub turn_right: bool,
}

/// A predicted player on a loaded map.
pub struct Prediction {
    map: String,
    pm: Pmove<'static>,
    yaw: f32,
    /// The previous command time, so turning follows the frame length instead
    /// of the frame count.
    last_time: i32,
}

impl Prediction {
    /// Loads `maps/<map>.bsp`, builds its collision model and puts the player
    /// on the first `info_player_deathmatch`.
    ///
    /// `G_SpawnPlayer` lifts the spawn spot one unit off the floor; the same
    /// nudge here keeps the first ground trace from starting inside it.
    pub fn load(assets: &Assets, map: &str) -> Result<Self, MapError> {
        let path = format!("maps/{map}.bsp");
        let bytes = assets
            .read(&path)
            .ok_or_else(|| MapError::Missing(path.clone()))?
            .into_owned();
        let bsp = Bsp::load(bytes)?;
        let mut spawn = spawn_point(bsp.entities()).unwrap_or([0.0; 3]);
        spawn[2] += 1.0;

        let world: &'static Collision = Box::leak(Box::new(Collision::load(&bsp)?));
        Ok(Prediction {
            map: map.to_string(),
            pm: Pmove::new(spawn, world),
            yaw: 0.0,
            last_time: 0,
        })
    }

    pub fn map(&self) -> &str {
        &self.map
    }

    pub fn origin(&self) -> [f32; 3] {
        self.pm.mv.origin
    }

    /// `xyspeed`: horizontal speed, which is what the player feels.
    pub fn speed(&self) -> f32 {
        let velocity = self.pm.mv.velocity;
        (velocity[0] * velocity[0] + velocity[1] * velocity[1]).sqrt()
    }

    pub fn on_ground(&self) -> bool {
        self.pm.ps.ground_entity_num != ENTITYNUM_NONE
    }

    /// Builds this frame's command and runs `Pmove` over it.
    ///
    /// `now` is the client's millisecond clock. `Pmove` chops a long gap into
    /// 66 ms pieces, so a stalled frame catches up instead of teleporting.
    pub fn step(&mut self, now: u32, input: &Input) {
        let now = now as i32;
        let elapsed = (now - self.last_time).clamp(0, 1000) as f32 / 1000.0;
        self.last_time = now;

        // Yaw grows to the left, the way `angle_vectors` builds the basis.
        let turn = input.turn_left as i32 - input.turn_right as i32;
        self.yaw = (self.yaw + turn as f32 * TURN_SPEED * elapsed).rem_euclid(360.0);

        self.pm.cmd = UserCmd {
            server_time: now,
            angles: [0, self.yaw.round() as i32, 0],
            forward_move: (input.forward as i32 - input.back as i32) * MOVE_SCALE,
            right_move: (input.right as i32 - input.left as i32) * MOVE_SCALE,
            up_move: if input.jump { MOVE_SCALE } else { 0 },
            ..UserCmd::default()
        };
        pmove(&mut self.pm);
    }
}

/// `G_FindSpawnPoint`: the origin of the first `info_player_deathmatch`.
fn spawn_point(entities: &[u8]) -> Option<[f32; 3]> {
    let text = String::from_utf8_lossy(entities);
    let tokens = tokenize(&text);

    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] != "{" {
            i += 1;
            continue;
        }
        i += 1;

        let mut classname = None;
        let mut origin = None;
        while i < tokens.len() && tokens[i] != "}" {
            let key = tokens[i].as_str();
            let value = tokens.get(i + 1).map(String::as_str).unwrap_or("");
            match key {
                "classname" => classname = Some(value),
                "origin" => origin = parse_origin(value),
                _ => {}
            }
            i += 2;
        }
        i += 1; // the closing brace

        if classname == Some("info_player_deathmatch") {
            if let Some(origin) = origin {
                return Some(origin);
            }
        }
    }
    None
}

fn parse_origin(value: &str) -> Option<[f32; 3]> {
    let mut parts = value.split_whitespace().map(str::parse::<f32>);
    Some([
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    ])
}

/// Splits an entity lump into braces and quoted strings, dropping `//`
/// comments. The game's `G_ParseEntityList` reads the same text through the
/// engine's tokenizer.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
        } else if ch == '{' || ch == '}' {
            tokens.push(ch.to_string());
            chars.next();
        } else if ch == '"' {
            chars.next();
            let mut token = String::new();
            for ch in chars.by_ref() {
                if ch == '"' {
                    break;
                }
                token.push(ch);
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() || ch == '"' || ch == '{' || ch == '}' {
                    break;
                }
                token.push(ch);
                chars.next();
            }
            if token.starts_with("//") {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            } else {
                tokens.push(token);
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn the_spawn_is_the_deathmatch_point() {
        let entities = br#"{
"classname" "worldspawn"
"message" "test map"
}
{
"classname" "info_player_intermission"
"origin" "0 0 0"
}
{
"origin" "128 64 32"
"classname" "info_player_deathmatch"
}
"#;
        assert_eq!(spawn_point(entities), Some([128.0, 64.0, 32.0]));
    }

    #[test]
    fn a_map_without_a_spawn_point_has_none() {
        assert_eq!(spawn_point(b"{ \"classname\" \"worldspawn\" }"), None);
    }

    /// A one-pak mount holding a BSP with the given entity lump, removed from
    /// disk once it is mounted. Each mount gets its own directory, because the
    /// tests run in parallel.
    fn assets_with_map(entities: &[u8]) -> Assets {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "q3rust-prediction-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bsp = crate::bsp::tests::build_bsp(entities);
        std::fs::write(
            dir.join("pak0.pk3"),
            crate::pak::tests::build_zip(b"", &[("maps/q3dm1.bsp", bsp.as_slice(), 0)]),
        )
        .unwrap();
        let assets = Assets::mount(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assets
    }

    fn prediction_on_a_test_map() -> Prediction {
        let entities = b"{ \"classname\" \"worldspawn\" }\n\
                         { \"classname\" \"info_player_deathmatch\" \"origin\" \"64 128 16\" }";
        let assets = assets_with_map(entities);
        Prediction::load(&assets, "q3dm1").unwrap()
    }

    #[test]
    fn loading_a_map_starts_the_player_at_the_spawn() {
        let prediction = prediction_on_a_test_map();
        assert_eq!(prediction.map(), "q3dm1");
        // One unit up, the nudge `G_SpawnPlayer` gives the spot.
        assert_eq!(prediction.origin(), [64.0, 128.0, 17.0]);
    }

    #[test]
    fn a_step_lets_the_player_fall() {
        let mut prediction = prediction_on_a_test_map();
        let start = prediction.origin();
        prediction.step(100, &Input::default());
        assert!(prediction.origin()[2] < start[2], "{:?}", prediction.origin());
        assert!(!prediction.on_ground());
    }

    #[test]
    fn comments_are_not_tokens() {
        let entities =
            b"{ // a note\n \"classname\" \"info_player_deathmatch\" \"origin\" \"1 2 3\" }";
        assert_eq!(spawn_point(entities), Some([1.0, 2.0, 3.0]));
    }
}
