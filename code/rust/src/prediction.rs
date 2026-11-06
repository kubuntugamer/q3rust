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

use std::time::{SystemTime, UNIX_EPOCH};

use crate::assets::Assets;
use crate::bg_misc::{self, ItemType};
use crate::bg_pmove::{
    pmove, Pmove, UserCmd, ENTITYNUM_NONE, STAT_ARMOR, STAT_HEALTH, STAT_WEAPONS, WP_MACHINEGUN,
};
use crate::bsp::Bsp;
use crate::collision::Collision;
use crate::entities;
use crate::game::MapError;
use crate::items::ItemField;

/// The full deflection of a `usercmd_t` move axis: the C client's 127.
const MOVE_SCALE: i32 = 127;
/// Degrees per second the turn keys rotate the view.
const TURN_SPEED: f32 = 180.0;
/// `G_InitPlayer`'s starting clip. Handing the player the machinegun means the
/// readout and the ammo rules start where the game does instead of at zero.
const STARTING_AMMO: i32 = 50;

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
    pitch: f32,
    /// The previous command time, so turning follows the frame length instead
    /// of the frame count.
    last_time: i32,
    /// The weapon asked for, which `PM_Weapon` drops the old one and raises.
    /// It differs from the weapon in hand while a change is playing out.
    requested_weapon: i32,
}

impl Prediction {
    /// Loads `maps/<map>.bsp`, builds its collision model and puts the player
    /// on one of the map's `info_player_deathmatch` spots, facing the way the
    /// map points that spot.
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
        let (spawn, yaw) = pick_spawn(&spawn_points(bsp.entities())).unwrap_or(([0.0; 3], 0.0));

        let world: &'static Collision = Box::leak(Box::new(Collision::load(&bsp)?));
        let mut pm = Pmove::new(spawn, world);
        // `G_InitPlayer`: the machinegun, held, with a clip.
        pm.ps.stats[STAT_WEAPONS] |= 1 << WP_MACHINEGUN;
        pm.ps.ammo[WP_MACHINEGUN as usize] = STARTING_AMMO;
        pm.ps.weapon = WP_MACHINEGUN;
        pm.cmd.weapon = WP_MACHINEGUN;
        Ok(Prediction {
            map: map.to_string(),
            pm,
            yaw,
            pitch: 0.0,
            last_time: 0,
            requested_weapon: WP_MACHINEGUN,
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

    /// `ps.stats[STAT_HEALTH]`.
    pub fn health(&self) -> i32 {
        self.pm.ps.stats[STAT_HEALTH]
    }

    /// `ps.stats[STAT_ARMOR]`.
    pub fn armor(&self) -> i32 {
        self.pm.ps.stats[STAT_ARMOR]
    }

    /// Rounds left in the weapon in hand.
    pub fn ammo(&self) -> i32 {
        usize::try_from(self.pm.ps.weapon)
            .ok()
            .and_then(|slot| self.pm.ps.ammo.get(slot).copied())
            .unwrap_or(0)
    }

    /// The weapon in hand, as a `WP_*`.
    pub fn weapon(&self) -> i32 {
        self.pm.ps.weapon
    }

    /// A weapon key: ask for the weapon in that slot, if the player has it.
    /// Returns whether the weapon is one the player owns, so a key bound to a
    /// weapon still on the floor does nothing.
    pub fn select_weapon(&mut self, weapon: i32) -> bool {
        let owned = usize::try_from(weapon)
            .ok()
            .map(|slot| self.pm.ps.stats[STAT_WEAPONS] & (1 << slot) != 0)
            .unwrap_or(false);
        if !owned {
            return false;
        }
        self.requested_weapon = weapon;
        true
    }

    /// `CG_TouchItem`: take every pickup the player is standing in, and say
    /// what was taken. A weapon the player did not have comes up in hand, the
    /// way it does in a match.
    pub fn collect(&mut self, field: &mut ItemField, now: u32) -> Vec<&'static str> {
        let taken = field.touch(&mut self.pm.ps, self.pm.mv.origin, now);
        for class_name in &taken {
            let Some(item) = bg_misc::find(class_name) else {
                continue;
            };
            if item.item_type == ItemType::Weapon {
                self.requested_weapon = item.tag;
            }
        }
        taken
    }

    /// `cl.viewangles[YAW]`: degrees, growing to the left.
    pub fn yaw(&self) -> f32 {
        self.yaw
    }

    /// `cl.viewangles[PITCH]`: degrees, positive looking down.
    pub fn pitch(&self) -> f32 {
        self.pitch
    }

    /// Mouse look, in degrees. `CL_MouseMove` clamps pitch the same way, so
    /// the view cannot roll over the poles.
    pub fn turn(&mut self, yaw: f32, pitch: f32) {
        self.yaw = (self.yaw + yaw).rem_euclid(360.0);
        self.pitch = (self.pitch + pitch).clamp(-89.0, 89.0);
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
            angles: [self.pitch.round() as i32, self.yaw.round() as i32, 0],
            forward_move: (input.forward as i32 - input.back as i32) * MOVE_SCALE,
            right_move: (input.right as i32 - input.left as i32) * MOVE_SCALE,
            up_move: if input.jump { MOVE_SCALE } else { 0 },
            // The weapon asked for; `PM_Weapon` turns a change into a drop and
            // a raise, and leaves the weapon alone when the two agree.
            weapon: self.requested_weapon,
            ..UserCmd::default()
        };
        pmove(&mut self.pm);
    }
}

/// One `info_player_deathmatch`: a spot the player may start on, and the
/// direction the map author pointed it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpawnPoint {
    pub origin: [f32; 3],
    /// `angle` in degrees, growing to the left like the client's yaw. `None`
    /// when the map leaves the direction open.
    pub angle: Option<f32>,
}

/// `G_FindSpawnPoint`: every deathmatch start the map places, in lump order.
///
/// A block with no `origin` is not a spot: there is nowhere to stand, and the
/// game skips it the same way rather than dropping the player at the origin of
/// the world.
pub fn spawn_points(entities: &[u8]) -> Vec<SpawnPoint> {
    entities::parse(entities)
        .into_iter()
        .filter(|entity| entity.classname.as_deref() == Some("info_player_deathmatch"))
        .filter_map(|entity| {
            Some(SpawnPoint {
                origin: entity.origin()?,
                angle: entity.get("angle").and_then(|value| value.parse().ok()),
            })
        })
        .collect()
}

/// `G_SelectRandomFurthestSpawnPoint` with one player on the map: there is
/// nobody to be far from, so the choice is a random spot. A spot the map turned
/// keeps that direction; a spot it left open gets a new one, so a respawn does
/// not always face the same wall.
///
/// Returns the origin to stand on and the yaw to face.
fn pick_spawn(spots: &[SpawnPoint]) -> Option<([f32; 3], f32)> {
    let spot = spots.get(random_below(spots.len()))?;
    let mut origin = spot.origin;
    // `G_SpawnPlayer` lifts the spawn spot one unit off the floor, so the first
    // ground trace does not start inside it.
    origin[2] += 1.0;
    let yaw = match spot.angle {
        Some(angle) => angle.rem_euclid(360.0),
        None => random_below(360) as f32,
    };
    Some((origin, yaw))
}

/// A number below `bound`, from a xorshift seeded with the clock. Spawning is
/// the one place the client has to choose something, and a dependency for a
/// single coin flip is not worth its build time.
fn random_below(bound: usize) -> usize {
    if bound <= 1 {
        return 0;
    }
    let clocks = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() ^ elapsed.as_secs() as u32)
        .unwrap_or(0x9e37_79b9);
    let mut x = clocks | 1;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x as usize % bound
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::bg_pmove::{WP_BFG, WP_ROCKET_LAUNCHER, WP_SHOTGUN};

    /// Two deathmatch spots with directions, plus a start with no origin and an
    /// intermission spot the player must never appear on.
    const MAP_ENTITIES: &[u8] = br#"{
"classname" "worldspawn"
"message" "test map"
}
{
"classname" "info_player_deathmatch"
"origin" "64 128 16"
"angle" "270"
}
{
"classname" "info_player_deathmatch"
"origin" "512 -96 40"
"angle" "45"
}
{
"classname" "info_player_deathmatch"
"angle" "180"
}
{
"classname" "info_player_intermission"
"origin" "0 0 0"
}
"#;

    #[test]
    fn every_deathmatch_spot_is_a_spawn_point() {
        let spots = spawn_points(MAP_ENTITIES);
        assert_eq!(spots.len(), 2, "the spot with no origin is not one");
        assert_eq!(
            spots[0],
            SpawnPoint {
                origin: [64.0, 128.0, 16.0],
                angle: Some(270.0),
            }
        );
        assert_eq!(spots[1].origin, [512.0, -96.0, 40.0]);
        assert_eq!(spots[1].angle, Some(45.0));
    }

    #[test]
    fn a_spot_without_an_angle_leaves_the_direction_open() {
        let spots =
            spawn_points(b"{ \"classname\" \"info_player_deathmatch\" \"origin\" \"128 64 32\" }");
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].angle, None);
    }

    #[test]
    fn a_map_without_a_spawn_point_has_none() {
        assert!(spawn_points(b"{ \"classname\" \"worldspawn\" }").is_empty());
    }

    #[test]
    fn comments_are_not_tokens() {
        let entities =
            b"{ // a note\n \"classname\" \"info_player_deathmatch\" \"origin\" \"1 2 3\" }";
        assert_eq!(spawn_points(entities)[0].origin, [1.0, 2.0, 3.0]);
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
        let assets = assets_with_map(MAP_ENTITIES);
        Prediction::load(&assets, "q3dm1").unwrap()
    }

    #[test]
    fn loading_a_map_starts_the_player_on_a_spot_it_places() {
        let prediction = prediction_on_a_test_map();
        assert_eq!(prediction.map(), "q3dm1");

        let origin = prediction.origin();
        let spot = spawn_points(MAP_ENTITIES)
            .into_iter()
            .find(|spot| [spot.origin[0], spot.origin[1], spot.origin[2] + 1.0] == origin)
            .unwrap_or_else(|| panic!("{origin:?} is not one of the map's spawns"));
        // The map pointed this spot, so the player faces that way.
        assert_eq!(prediction.yaw(), spot.angle.unwrap());
    }

    #[test]
    fn a_spot_the_map_left_open_still_turns_the_player() {
        let spot = b"{ \"classname\" \"info_player_deathmatch\" \"origin\" \"1 2 3\" }";
        let yaws: Vec<f32> = (0..8)
            .map(|_| pick_spawn(&spawn_points(spot)).unwrap().1)
            .collect();
        assert!(yaws.iter().all(|yaw| (0.0..360.0).contains(yaw)), "{yaws:?}");
    }

    const MAP_WITH_ROCKETS: &[u8] = br#"{
"classname" "worldspawn"
}
{
"classname" "info_player_deathmatch"
"origin" "64 128 16"
"angle" "270"
}
{
"classname" "weapon_rocketlauncher"
"origin" "64 128 17"
}
"#;

    #[test]
    fn a_picked_up_weapon_comes_up_in_hand() {
        let assets = assets_with_map(MAP_WITH_ROCKETS);
        let mut prediction = Prediction::load(&assets, "q3dm1").unwrap();
        let mut field = ItemField::new(&crate::items::item_marks(MAP_WITH_ROCKETS));
        assert_eq!(prediction.weapon(), WP_MACHINEGUN);
        assert_eq!(prediction.ammo(), STARTING_AMMO);

        prediction.step(16, &Input::default());
        assert_eq!(prediction.collect(&mut field, 16), ["weapon_rocketlauncher"]);
        assert!(
            !prediction.select_weapon(WP_SHOTGUN),
            "a weapon still on the floor cannot be selected"
        );

        // Dropping the machinegun and raising the launcher takes time.
        for tick in 1..60 {
            prediction.step(16 + tick * 20, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_ROCKET_LAUNCHER);
        assert_eq!(prediction.ammo(), 10);
    }

    #[test]
    fn a_weapon_key_only_works_for_a_weapon_in_hand() {
        let mut prediction = prediction_on_a_test_map();
        assert!(prediction.select_weapon(WP_MACHINEGUN));
        assert!(!prediction.select_weapon(WP_BFG));
        // The clip counter follows the weapon in hand once a change completes.
        assert_eq!(prediction.ammo(), STARTING_AMMO);
    }

    #[test]
    fn a_loaded_player_holds_the_machinegun_with_a_clip() {
        let prediction = prediction_on_a_test_map();
        assert_eq!(prediction.health(), 100);
        assert_eq!(prediction.armor(), 0);
        assert_eq!(prediction.ammo(), STARTING_AMMO);
    }

    #[test]
    fn a_step_lets_the_player_fall() {
        let mut prediction = prediction_on_a_test_map();
        let start = prediction.origin();
        prediction.step(100, &Input::default());
        assert!(prediction.origin()[2] < start[2], "{:?}", prediction.origin());
        assert!(!prediction.on_ground());
    }
}
