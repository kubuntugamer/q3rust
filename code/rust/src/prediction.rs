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
    pmove, Pmove, UserCmd, ENTITYNUM_NONE, EV_FALL_FAR, EV_FALL_MEDIUM, STAT_ARMOR, STAT_HEALTH,
    STAT_WEAPONS, WP_BFG, WP_GAUNTLET, WP_GRENADE_LAUNCHER, WP_LIGHTNING, WP_MACHINEGUN, WP_PLASMAGUN, WP_RAILGUN,
    WP_ROCKET_LAUNCHER, WP_SHOTGUN,
};
use crate::bsp::Bsp;
use crate::collision::Collision;
use crate::entities;
use crate::game::MapError;
use crate::items::ItemField;
use crate::pmove::{World, MASK_PLAYERSOLID};
use crate::world::DEFAULT_VIEW_HEIGHT;

/// The full deflection of a `usercmd_t` move axis: the C client's 127.
const MOVE_SCALE: i32 = 127;
/// Degrees per second the turn keys rotate the view.
const TURN_SPEED: f32 = 180.0;
/// `G_InitPlayer`'s starting clip. Handing the player the machinegun means the
/// readout and the ammo rules start where the game does instead of at zero.
const STARTING_AMMO: i32 = 50;
/// `MAX_SHOT_RANGE`: a shot that hits nothing travels this far.
const SHOT_RANGE: f32 = 8192.0;
/// How far a gauntlet swipe reaches.
const GAUNTLET_RANGE: f32 = 32.0;
/// What `ClientEvents` takes off for a hard landing, and for a medium one.
const FALL_DAMAGE_FAR: i32 = 10;
const FALL_DAMAGE_MEDIUM: i32 = 5;
/// `player_die`'s `respawnTime`: how long a body lies before it comes back.
const RESPAWN_DELAY: u32 = 1_700;
/// A rocket's speed, from `g_weapon.c`: 900 units a second.
const ROCKET_SPEED: f32 = 900.0;
/// `sv_gravity`: what pulls a lobbed grenade down.
const GRAVITY: f32 = 800.0;
/// How much of its speed a bouncing grenade keeps.
const BOUNCE: f32 = 0.6;
/// How long the BFG has to be held before it lets go.
const BFG_CHARGE_MS: i32 = 1_200;
/// A plasma ball is quicker and shorter-lived than a rocket.
const PLASMA_SPEED: f32 = 2_000.0;
/// How long a projectile lives before it goes off in the air.
const PROJECTILE_LIFE_MS: i32 = 5_000;
/// How wide a rocket's blast reaches.
pub const ROCKET_RADIUS: f32 = 120.0;
/// A plasma ball bursts in a smaller radius.
pub const PLASMA_RADIUS: f32 = 60.0;

/// Reflects a velocity off a surface and keeps `keep` of its speed, which is
/// how a grenade skips along a wall or the floor.
pub fn bounce_off(velocity: [f32; 3], normal: [f32; 3], keep: f32) -> [f32; 3] {
    let into = velocity[0] * normal[0] + velocity[1] * normal[1] + velocity[2] * normal[2];
    [
        (velocity[0] - 2.0 * into * normal[0]) * keep,
        (velocity[1] - 2.0 * into * normal[1]) * keep,
        (velocity[2] - 2.0 * into * normal[2]) * keep,
    ]
}

/// Something in flight. It travels a frame at a time rather than landing the
/// instant the trigger is pulled; a rocket and a plasma ball differ in speed,
/// size and what they leave behind.
#[derive(Clone, Copy, Debug)]
pub struct Projectile {
    pub origin: [f32; 3],
    velocity: [f32; 3],
    born: i32,
    damage: i32,
    radius: f32,
    /// Blue for plasma, warm for a rocket.
    pub plasma: bool,
    /// Whether the round falls as it flies, the way a grenade does.
    pub gravity: bool,
}

/// Where a projectile went off, and what it does there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blast {
    pub at: [f32; 3],
    pub radius: f32,
    pub damage: i32,
}

/// One round leaving the weapon: where it started, where it stopped, and
/// whether it stopped on something.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    pub from: [f32; 3],
    pub to: [f32; 3],
    pub hit: bool,
    /// Blast radius in units, or 0 for a plain bullet.
    pub splash: f32,
    /// What this round takes off whatever it hits.
    pub damage: i32,
}

/// What a weapon does when the trigger is pulled: how often it fires, how many
/// pellets leave, how far they spread, and whether the hit explodes.
struct Pattern {
    weapon: i32,
    interval: i32,
    pellets: usize,
    /// Full cone width in degrees; a machinegun fires one pellet straight.
    spread: f32,
    splash: f32,
    /// Per round, or per pellet for the shotgun.
    damage: i32,
    /// How fast the round flies, or 0 for a shot that lands at once.
    speed: f32,
    /// How wide its blast is, or 0 for a round that only hits what it touches.
    radius: f32,
    /// Whether it is plasma rather than a rocket.
    plasma: bool,
    /// Whether it falls as it flies, the way a grenade does.
    gravity: bool,
}

const PATTERNS: [Pattern; 9] = [
    Pattern {
        weapon: WP_MACHINEGUN,
        interval: 100,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        damage: 34,
        speed: 0.0,
        radius: 0.0,
        plasma: false,
        gravity: false,
    },
    Pattern {
        weapon: WP_SHOTGUN,
        interval: 1000,
        pellets: 11,
        spread: 6.0,
        splash: 0.0,
        // A pellet, not a shell: eleven of them at close range add up.
        damage: 12,
        speed: 0.0,
        radius: 0.0,
        plasma: false,
        gravity: false,
    },
    Pattern {
        weapon: WP_ROCKET_LAUNCHER,
        interval: 800,
        pellets: 1,
        spread: 0.0,
        splash: 120.0,
        damage: 100,
        speed: ROCKET_SPEED,
        radius: ROCKET_RADIUS,
        plasma: false,
        gravity: false,
    },
    Pattern {
        weapon: WP_RAILGUN,
        interval: 1_500,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // One slug, straight through: a post goes down in a single shot.
        damage: 100,
        speed: 0.0,
        radius: 0.0,
        plasma: false,
        gravity: false,
    },
    Pattern {
        weapon: WP_PLASMAGUN,
        interval: 100,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // Balls that fly faster than a rocket and burst smaller.
        damage: 20,
        speed: PLASMA_SPEED,
        radius: PLASMA_RADIUS,
        plasma: true,
        gravity: false,
    },
    Pattern {
        weapon: WP_GRENADE_LAUNCHER,
        interval: 800,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // Slower than a rocket, and it bursts as wide as one, falling as it
        // goes so it has to be lobbed.
        damage: 100,
        speed: 700.0,
        radius: ROCKET_RADIUS,
        plasma: false,
        gravity: true,
    },
    Pattern {
        weapon: WP_BFG,
        interval: 2_000,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // Slow to fire, slow in the air, and it clears a room.
        damage: 200,
        speed: 400.0,
        radius: 300.0,
        plasma: true,
        gravity: false,
    },
    Pattern {
        weapon: WP_GAUNTLET,
        interval: 400,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // Barely past arm's length, and it hurts.
        damage: 50,
        speed: 0.0,
        radius: 0.0,
        plasma: false,
        gravity: false,
    },
    Pattern {
        weapon: WP_LIGHTNING,
        interval: 50,
        pellets: 1,
        spread: 0.0,
        splash: 0.0,
        // A thin, fast stream: small bites taken twenty times a second.
        damage: 8,
        speed: 0.0,
        radius: 0.0,
        plasma: false,
        gravity: false,
    },
];

impl Shot {
    /// The direction the round travelled, as a unit vector. A shooter's flash
    /// sits along it, just past the muzzle.
    pub fn forward(&self) -> [f32; 3] {
        let delta = [
            self.to[0] - self.from[0],
            self.to[1] - self.from[1],
            self.to[2] - self.from[2],
        ];
        let length = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        if length == 0.0 {
            return [1.0, 0.0, 0.0];
        }
        [delta[0] / length, delta[1] / length, delta[2] / length]
    }
}

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
    /// The trigger.
    pub attack: bool,
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
    /// When the last round left, so the trigger cannot outrun the weapon.
    last_shot: i32,
    /// Every spot the map offers, kept so a respawn has one to choose.
    spawns: Vec<SpawnPoint>,
    /// When the body comes back, or 0 while the player is alive.
    respawn_at: u32,
    /// When the BFG's trigger went down, while it is charging.
    charge_started: Option<i32>,
    /// Projectiles in the air, and where the ones that went off did so.
    projectiles: Vec<Projectile>,
    blasts: Vec<Blast>,
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
        let spawns = spawn_points(bsp.entities());
        let (spawn, yaw) = pick_spawn(&spawns).unwrap_or(([0.0; 3], 0.0));

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
            last_shot: -1000,
            spawns,
            respawn_at: 0,
            charge_started: None,
            projectiles: Vec::new(),
            blasts: Vec::new(),
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

    /// Whether the map's walls leave a clear line between two places. Used by a
    /// practice post to tell whether it has a shot instead of firing through
    /// the level.
    pub fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        let trace = self.pm.world.trace(
            from,
            to,
            [0.0; 3],
            [0.0; 3],
            crate::collision::CONTENTS_SOLID,
        );
        trace.fraction == 1.0
    }

    /// Whether a point is inside the map's solid geometry. A bolt asks this so
    /// it can stop on the floor or a wall instead of passing through it.
    pub fn solid(&self, point: [f32; 3]) -> bool {
        self.pm.world.point_contents(point, 0) & crate::collision::CONTENTS_SOLID != 0
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

    /// The trigger, for the length of one round: spends a cartridge, then
    /// traces the bullet from the eye along the view to whatever stops it.
    ///
    /// Only the machinegun fires so far; the other weapons need their own shot
    /// patterns, which is a bigger port than one hitscan line.
    pub fn fire(&mut self, now: u32) -> Vec<Shot> {
        let Some(pattern) = PATTERNS
            .iter()
            .find(|pattern| pattern.weapon == self.pm.ps.weapon)
        else {
            return Vec::new();
        };
        let now = now as i32;
        if now - self.last_shot < pattern.interval {
            return Vec::new();
        }

        // The BFG has to be held down: the first press starts the charge and
        // only a full one lets the ball go.
        if pattern.weapon == WP_BFG {
            match self.charge_started {
                None => {
                    self.charge_started = Some(now);
                    return Vec::new();
                }
                Some(start) if now - start < BFG_CHARGE_MS => return Vec::new(),
                Some(_) => self.charge_started = None,
            }
        } else if self.charge_started.is_some() {
            // Letting go of the charge by changing weapon drops it.
            self.charge_started = None;
        }
        let slot = pattern.weapon as usize;
        // The gauntlet is free to swing: it has no cells or shells to spend.
        let free = pattern.weapon == WP_GAUNTLET;
        let ammo = self.pm.ps.ammo.get(slot).copied().unwrap_or(0);
        if !free && ammo <= 0 {
            return Vec::new();
        }
        self.last_shot = now;
        if !free {
            if let Some(clip) = self.pm.ps.ammo.get_mut(slot) {
                *clip = ammo - 1;
            }
        }

        let from = [
            self.pm.mv.origin[0],
            self.pm.mv.origin[1],
            self.pm.mv.origin[2] + DEFAULT_VIEW_HEIGHT,
        ];

        // A rocket or a plasma ball is not a hitscan: it leaves the muzzle and
        // travels, and the blast comes from `advance_rockets` when it arrives.
        if pattern.speed > 0.0 {
            let (yaw, pitch) = (self.yaw.to_radians(), self.pitch.to_radians());
            let forward = [
                pitch.cos() * yaw.cos(),
                pitch.cos() * yaw.sin(),
                -pitch.sin(),
            ];
            let muzzle = [
                from[0] + forward[0] * 16.0,
                from[1] + forward[1] * 16.0,
                from[2] + forward[2] * 16.0,
            ];
            self.projectiles.push(Projectile {
                origin: muzzle,
                velocity: [
                    forward[0] * pattern.speed,
                    forward[1] * pattern.speed,
                    forward[2] * pattern.speed,
                ],
                born: now,
                damage: pattern.damage,
                radius: pattern.radius,
                plasma: pattern.plasma,
                gravity: pattern.gravity,
            });
            return vec![Shot {
                from,
                to: muzzle,
                hit: false,
                splash: 0.0,
                damage: pattern.damage,
            }];
        }

        let mut shots = Vec::with_capacity(pattern.pellets);
        for pellet in 0..pattern.pellets {
            // A spread is a fan across the cone, deterministic so a replay
            // looks the same; the shotgun's pellets scatter around the centre.
            let step = if pattern.pellets > 1 {
                pellet as f32 / (pattern.pellets - 1) as f32 - 0.5
            } else {
                0.0
            };
            let sideways = step * pattern.spread;
            let lift = (((pellet * 5) % 7) as f32 / 6.0 - 0.5) * pattern.spread;
            let (yaw, pitch) = (
                (self.yaw + sideways).to_radians(),
                (self.pitch + lift).to_radians(),
            );
            // `AngleVectors`: a positive pitch looks down, and yaw grows to the
            // left, which is the basis the camera uses too.
            let forward = [
                pitch.cos() * yaw.cos(),
                pitch.cos() * yaw.sin(),
                -pitch.sin(),
            ];
            // The gauntlet is a swipe, not a shot: it only reaches where an arm
            // would.
            let range = if pattern.weapon == WP_GAUNTLET {
                GAUNTLET_RANGE
            } else {
                SHOT_RANGE
            };
            let to = [
                from[0] + forward[0] * range,
                from[1] + forward[1] * range,
                from[2] + forward[2] * range,
            ];

            // Shots stop on the same surfaces the player's body does; bodies
            // and glass are part of `MASK_SHOT`, which has nothing to hit yet.
            let trace = self
                .pm
                .world
                .trace(from, to, [0.0; 3], [0.0; 3], MASK_PLAYERSOLID);
            shots.push(Shot {
                from,
                to: trace.endpos,
                hit: trace.fraction < 1.0,
                splash: pattern.splash,
                damage: pattern.damage,
            });
        }
        shots
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
        // A body that has waited out its death comes back before anything else
        // happens this frame.
        if self.respawn_at != 0 && now >= self.respawn_at {
            self.respawn();
        }
        // A corpse does not walk, whatever keys are held.
        let still = Input::default();
        let input = if self.health() <= 0 { &still } else { input };

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

        // The move reports what happened to the body; damage is applied where
        // the server would apply it.
        let events = std::mem::take(&mut self.pm.ps.events);
        self.apply_events(&events);

        self.advance_rockets(elapsed);

        // `player_die`: health that has run out starts the respawn clock.
        if self.health() <= 0 && self.respawn_at == 0 {
            self.respawn_at = self.last_time.max(0) as u32 + RESPAWN_DELAY;
        }
    }

    /// Moves every projectile along its path. One that reaches a wall, or
    /// outlives `PROJECTILE_LIFE_MS`, goes off where it stopped.
    fn advance_rockets(&mut self, seconds: f32) {
        let mut in_flight = Vec::with_capacity(self.projectiles.len());
        for mut shot in std::mem::take(&mut self.projectiles) {
            // A grenade falls the way the game's gravity would pull it.
            if shot.gravity {
                shot.velocity[2] -= GRAVITY * seconds;
            }
            let to = [
                shot.origin[0] + shot.velocity[0] * seconds,
                shot.origin[1] + shot.velocity[1] * seconds,
                shot.origin[2] + shot.velocity[2] * seconds,
            ];
            let trace = self
                .pm
                .world
                .trace(shot.origin, to, [0.0; 3], [0.0; 3], MASK_PLAYERSOLID);
            let old = self.last_time - shot.born >= PROJECTILE_LIFE_MS;
            if trace.fraction < 1.0 && shot.gravity && !old {
                // A grenade skips off what it hits instead of bursting, losing
                // energy each time; it bursts when its time runs out.
                let normal = trace.plane.normal;
                shot.velocity = bounce_off(shot.velocity, normal, BOUNCE);
                shot.origin = [
                    trace.endpos[0] + normal[0] * 0.5,
                    trace.endpos[1] + normal[1] * 0.5,
                    trace.endpos[2] + normal[2] * 0.5,
                ];
                in_flight.push(shot);
            } else if trace.fraction < 1.0 || old {
                self.blasts.push(Blast {
                    at: trace.endpos,
                    radius: shot.radius,
                    damage: shot.damage,
                });
            } else {
                in_flight.push(Projectile {
                    origin: trace.endpos,
                    ..shot
                });
            }
        }
        self.projectiles = in_flight;
    }

    /// How far a held charge has got, from nothing to ready.
    pub fn charge(&self, now: u32) -> f32 {
        match self.charge_started {
            Some(start) => ((now as i32 - start) as f32 / BFG_CHARGE_MS as f32).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// The projectiles still in the air, for drawing.
    pub fn projectiles(&self) -> &[Projectile] {
        &self.projectiles
    }

    /// Takes the blasts that happened this frame, so the client plays each one
    /// once.
    pub fn take_blasts(&mut self) -> Vec<Blast> {
        std::mem::take(&mut self.blasts)
    }

    /// `PutClientInServer`: a fresh body on one of the map's spots, with the
    /// health, armour and starting weapon the game gives a new player.
    pub fn respawn(&mut self) {
        let (origin, yaw) = pick_spawn(&self.spawns).unwrap_or(([0.0; 3], 0.0));
        self.pm.mv.origin = origin;
        self.pm.mv.velocity = [0.0; 3];
        self.pm.ps.stats[STAT_HEALTH] = 100;
        self.pm.ps.stats[STAT_ARMOR] = 0;
        self.pm.ps.stats[STAT_WEAPONS] = 1 << WP_MACHINEGUN;
        self.pm.ps.ammo[WP_MACHINEGUN as usize] = STARTING_AMMO;
        self.pm.ps.weapon = WP_MACHINEGUN;
        self.pm.ps.ground_entity_num = ENTITYNUM_NONE;
        self.requested_weapon = WP_MACHINEGUN;
        self.last_shot = -1000;
        self.charge_started = None;
        self.yaw = yaw;
        self.pitch = 0.0;
        self.respawn_at = 0;
    }

    /// Whether the player is waiting to come back.
    pub fn dead(&self) -> bool {
        self.respawn_at != 0
    }

    /// The events the move raised. Only landings hurt so far: nothing on the
    /// map can shoot back yet.
    pub fn apply_events(&mut self, events: &[i32]) {
        for event in events {
            match *event {
                EV_FALL_FAR => {
                    self.hurt(FALL_DAMAGE_FAR);
                }
                EV_FALL_MEDIUM => {
                    self.hurt(FALL_DAMAGE_MEDIUM);
                }
                _ => {}
            }
        }
    }

    /// Takes `amount` off the health, never below zero, and returns what is
    /// left. A dead player is reported, not yet respawned.
    pub fn hurt(&mut self, amount: i32) -> i32 {
        let health = (self.pm.ps.stats[STAT_HEALTH] - amount).max(0);
        self.pm.ps.stats[STAT_HEALTH] = health;
        health
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
    fn the_open_air_is_in_plain_view() {
        let prediction = prediction_on_a_test_map();
        let origin = prediction.origin();
        let above = [origin[0], origin[1], origin[2] + 64.0];
        assert!(prediction.sees(origin, above));
        assert!(prediction.sees(above, origin));
    }

    #[test]
    fn the_trigger_spends_a_round_at_the_machineguns_rate() {
        let mut prediction = prediction_on_a_test_map();
        assert_eq!(prediction.ammo(), STARTING_AMMO);

        let shot = prediction.fire(1_000).pop().expect("the first press fires");
        assert!(!shot.hit, "the test map has no geometry to stop a bullet");
        let travel = ((shot.to[0] - shot.from[0]).powi(2)
            + (shot.to[1] - shot.from[1]).powi(2)
            + (shot.to[2] - shot.from[2]).powi(2))
        .sqrt();
        assert!(travel > 1_000.0, "the round went nowhere: {:?}", shot);
        assert_eq!(prediction.ammo(), STARTING_AMMO - 1);

        assert!(prediction.fire(1_050).is_empty(), "too soon for a second round");
        assert!(!prediction.fire(1_100).is_empty());
        assert_eq!(prediction.ammo(), STARTING_AMMO - 2);
    }

    #[test]
    fn a_round_travels_along_a_unit_direction() {
        let mut prediction = prediction_on_a_test_map();
        prediction.turn(90.0 - prediction.yaw(), 0.0);
        let forward = prediction.fire(1_000).pop().unwrap().forward();
        let length = (forward[0] * forward[0] + forward[1] * forward[1] + forward[2] * forward[2])
            .sqrt();
        assert!((length - 1.0).abs() < 1e-4, "{forward:?}");
        assert!((forward[1] - 1.0).abs() < 1e-4, "{forward:?}");
    }

    #[test]
    fn the_trigger_looks_where_the_player_looks() {
        let mut prediction = prediction_on_a_test_map();
        prediction.turn(90.0 - prediction.yaw(), 0.0);
        let shot = prediction.fire(1_000).pop().unwrap();
        assert!(shot.to[1] > shot.from[1], "{:?}", shot);
        assert!((shot.to[0] - shot.from[0]).abs() < 1.0, "{:?}", shot);

        let mut prediction = prediction_on_a_test_map();
        prediction.turn(0.0, 90.0);
        let shot = prediction.fire(1_000).pop().unwrap();
        assert!(shot.to[2] < shot.from[2], "looking down shoots down: {:?}", shot);
    }

    #[test]
    fn an_empty_clip_stops_firing() {
        let mut prediction = prediction_on_a_test_map();
        let mut now = 0;
        for _ in 0..STARTING_AMMO {
            now += 100;
            assert!(!prediction.fire(now as u32).is_empty());
        }
        assert_eq!(prediction.ammo(), 0);
        assert!(prediction.fire((now + 500) as u32).is_empty());
    }

    #[test]
    fn dying_comes_back_whole_on_a_map_spot() {
        let mut prediction = prediction_on_a_test_map();
        prediction.hurt(200);
        assert_eq!(prediction.health(), 0);
        assert!(!prediction.dead(), "the clock starts on the next move");

        prediction.step(1_000, &Input::default());
        assert!(prediction.dead());
        assert_eq!(prediction.health(), 0, "a body stays down for a moment");

        prediction.step(1_000 + RESPAWN_DELAY, &Input::default());
        assert!(!prediction.dead());
        assert_eq!(prediction.health(), 100);
        assert_eq!(prediction.ammo(), STARTING_AMMO);
        // The spot it chose, on the last frame's drop: the move still applies
        // gravity to the fresh body on the frame it comes back.
        let origin = prediction.origin();
        assert!(
            spawn_points(MAP_ENTITIES)
                .iter()
                .any(|spot| spot.origin[0] == origin[0] && spot.origin[1] == origin[1]),
            "{origin:?} is not above one of the map's spawns"
        );
    }

    #[test]
    fn the_shotgun_fires_a_spread_for_one_shell() {
        let mut prediction = prediction_on_a_test_map();
        prediction.select_weapon(WP_MACHINEGUN);
        // Give the player the shotgun and its shells, as a pickup would.
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_SHOTGUN;
        prediction.pm.ps.ammo[WP_SHOTGUN as usize] = 10;
        assert!(prediction.select_weapon(WP_SHOTGUN));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_SHOTGUN);

        let now = 1_000 + 40 * 50;
        let shots = prediction.fire(now);
        assert_eq!(shots.len(), 11, "a full load of pellets");
        assert_eq!(prediction.ammo(), 9, "one shell, not one per pellet");
        assert!(prediction.fire(now + 500).is_empty(), "the pump takes a second");

        // Every pellet is inside the cone the pattern names.
        let centre = [
            prediction.origin()[0],
            prediction.origin()[1],
            prediction.origin()[2] + DEFAULT_VIEW_HEIGHT,
        ];
        for shot in &shots {
            let side = [shot.to[0] - centre[0], shot.to[1] - centre[1]];
            let off = side[1].atan2(side[0]).to_degrees();
            let turned = (off - prediction.yaw() + 180.0).rem_euclid(360.0) - 180.0;
            assert!(turned.abs() <= 3.5, "pellet {turned} degrees off centre");
        }
    }

    #[test]
    fn the_gauntlet_only_reaches_an_arms_length() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_GAUNTLET;
        assert!(prediction.select_weapon(WP_GAUNTLET));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_GAUNTLET);

        let now = 1_000 + 40 * 50;
        let swipe = prediction.fire(now).pop().expect("the gauntlet swings");
        let reach = ((swipe.to[0] - swipe.from[0]).powi(2)
            + (swipe.to[1] - swipe.from[1]).powi(2)
            + (swipe.to[2] - swipe.from[2]).powi(2))
        .sqrt();
        assert!(reach <= GAUNTLET_RANGE + 0.001, "it reaches {reach}");
        assert!(prediction.fire(now + 100).is_empty(), "and swings slowly");
        assert_eq!(prediction.fire(now + 400).len(), 1);
    }

    #[test]
    fn the_bfg_has_to_be_held_before_it_fires() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_BFG;
        prediction.pm.ps.ammo[WP_BFG as usize] = 2;
        assert!(prediction.select_weapon(WP_BFG));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        let now = 1_000 + 40 * 50;

        // The first press starts a charge and nothing leaves.
        assert!(prediction.fire(now).is_empty());
        assert!(prediction.charge(now) < 0.05);
        assert_eq!(prediction.ammo(), 2, "nothing spent yet");

        // Half a charge is still not a shot.
        assert!(prediction.fire(now + 600).is_empty());
        assert!(prediction.charge(now + 600) > 0.4);

        // Held for the full time, it lets go.
        assert_eq!(prediction.fire(now + BFG_CHARGE_MS as u32).len(), 1);
        assert_eq!(prediction.ammo(), 1);
        assert_eq!(prediction.charge(now + 1_200), 0.0, "the charge is spent");
    }

    #[test]
    fn the_bfg_is_slow_and_wipes_a_room() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_BFG;
        prediction.pm.ps.ammo[WP_BFG as usize] = 2;
        assert!(prediction.select_weapon(WP_BFG));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_BFG);

        let now = 1_000 + 40 * 50;
        // Two presses: one starts the charge, the next lets it go once full.
        assert!(prediction.fire(now).is_empty());
        assert_eq!(prediction.fire(now + BFG_CHARGE_MS as u32).len(), 1);
        assert_eq!(prediction.ammo(), 1);
        let ball = prediction.projectiles()[0];
        assert_eq!(ball.damage, 200);
        assert_eq!(ball.radius, 300.0, "a blast wider than a rocket's");
        assert!(
            prediction.fire(now + 1_400).is_empty(),
            "the next shot has to charge too"
        );
    }

    #[test]
    fn a_grenade_skips_off_a_floor() {
        let out = bounce_off([10.0, 0.0, -400.0], [0.0, 0.0, 1.0], BOUNCE);
        assert!((out[0] - 6.0).abs() < 1e-3, "it keeps some travel: {out:?}");
        assert!(out[2] > 200.0, "and it comes back up: {out:?}");

        // Against a wall it comes away instead of stopping.
        let out = bounce_off([300.0, 0.0, 0.0], [-1.0, 0.0, 0.0], BOUNCE);
        assert!(out[0] < 0.0, "it turns away: {out:?}");
    }

    #[test]
    fn a_grenade_falls_as_it_flies() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_GRENADE_LAUNCHER;
        prediction.pm.ps.ammo[WP_GRENADE_LAUNCHER as usize] = 5;
        assert!(prediction.select_weapon(WP_GRENADE_LAUNCHER));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        let now = 1_000 + 40 * 50;
        assert_eq!(prediction.fire(now).len(), 1);
        let start = prediction.projectiles()[0].origin;

        prediction.step(now + 100, &Input::default());
        // Fired level, so any drop is gravity's doing.
        let after = prediction
            .projectiles()
            .first()
            .expect("the grenade is still in the air")
            .origin;
        assert!(after[2] < start[2], "the grenade drops: {after:?}");
    }

    #[test]
    fn the_lightning_gun_streams_and_the_grenade_lobs() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |=
            (1 << WP_LIGHTNING) | (1 << WP_GRENADE_LAUNCHER);
        prediction.pm.ps.ammo[WP_LIGHTNING as usize] = 100;
        prediction.pm.ps.ammo[WP_GRENADE_LAUNCHER as usize] = 10;

        // The lightning gun is the fast one: two bites inside 100 ms.
        assert!(prediction.select_weapon(WP_LIGHTNING));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        let now = 1_000 + 40 * 50;
        assert_eq!(prediction.fire(now).len(), 1);
        assert_eq!(prediction.fire(now + 50).len(), 1, "twenty bites a second");
        assert_eq!(prediction.ammo(), 98, "a cell per bite");

        // The grenade launcher lobs a round that bursts wide.
        assert!(prediction.select_weapon(WP_GRENADE_LAUNCHER));
        for tick in 0..40 {
            prediction.step(now + 500 + tick * 50, &Input::default());
        }
        let later = now + 500 + 40 * 50;
        assert_eq!(prediction.weapon(), WP_GRENADE_LAUNCHER);
        assert_eq!(prediction.fire(later).len(), 1);
        let round = prediction.projectiles()[0];
        assert!(!round.plasma, "a grenade is not plasma");
        assert_eq!(round.radius, ROCKET_RADIUS);
    }

    #[test]
    fn the_plasma_gun_throws_fast_balls_that_burst_small() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_PLASMAGUN;
        prediction.pm.ps.ammo[WP_PLASMAGUN as usize] = 50;
        assert!(prediction.select_weapon(WP_PLASMAGUN));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_PLASMAGUN);

        let now = 1_000 + 40 * 50;
        let shots = prediction.fire(now);
        assert_eq!(shots.len(), 1);
        assert_eq!(prediction.ammo(), 49);
        let ball = prediction.projectiles()[0];
        assert!(ball.plasma, "it is plasma");
        assert!((ball.radius - PLASMA_RADIUS).abs() < 0.001, "a small burst");

        // It covers ground faster than a rocket does.
        let start = ball.origin;
        prediction.step(now + 50, &Input::default());
        if let Some(moved) = prediction.projectiles().first() {
            let gone = ((moved.origin[0] - start[0]).powi(2)
                + (moved.origin[1] - start[1]).powi(2)
                + (moved.origin[2] - start[2]).powi(2))
            .sqrt();
            assert!(gone > 10.0, "the ball travels: {gone}");
        }
    }

    #[test]
    fn the_rocket_launcher_fires_one_round_that_splashes() {
        let mut prediction = prediction_on_a_test_map();
        prediction.pm.ps.stats[STAT_WEAPONS] |= 1 << WP_ROCKET_LAUNCHER;
        prediction.pm.ps.ammo[WP_ROCKET_LAUNCHER as usize] = 5;
        assert!(prediction.select_weapon(WP_ROCKET_LAUNCHER));
        for tick in 0..40 {
            prediction.step(1_000 + tick * 50, &Input::default());
        }
        assert_eq!(prediction.weapon(), WP_ROCKET_LAUNCHER);

        let now = 1_000 + 40 * 50;
        let shots = prediction.fire(now);
        assert_eq!(shots.len(), 1, "one round leaves the launcher");
        assert_eq!(prediction.ammo(), 4);
        assert_eq!(prediction.projectiles().len(), 1, "and it flies");
        let start = prediction.projectiles()[0].origin;

        // It travels rather than landing at once.
        prediction.step(now + 100, &Input::default());
        let moved = prediction.projectiles().first().map(|shot| shot.origin);
        assert!(moved.is_some_and(|at| at != start), "the rocket moves");

        // Given enough time it goes off, wherever it stopped.
        prediction.step(now + 6_000, &Input::default());
        assert!(prediction.projectiles().is_empty());
        let blasts = prediction.take_blasts();
        assert_eq!(blasts.len(), 1, "one blast, once");
        assert_eq!(blasts[0].damage, 100, "a rocket's own damage");
        assert_eq!(blasts[0].radius, ROCKET_RADIUS);
        assert!(prediction.take_blasts().is_empty(), "and it is only reported once");
    }

    #[test]
    fn a_hard_landing_hurts_and_health_stops_at_zero() {
        let mut prediction = prediction_on_a_test_map();
        assert_eq!(prediction.health(), 100);

        prediction.apply_events(&[EV_FALL_MEDIUM]);
        assert_eq!(prediction.health(), 100 - FALL_DAMAGE_MEDIUM);

        prediction.apply_events(&[EV_FALL_FAR]);
        assert_eq!(
            prediction.health(),
            100 - FALL_DAMAGE_MEDIUM - FALL_DAMAGE_FAR
        );

        // A small fall raises `EV_FALL_SHORT`, which costs nothing.
        prediction.apply_events(&[crate::bg_pmove::EV_FALL_SHORT]);
        assert_eq!(
            prediction.health(),
            100 - FALL_DAMAGE_MEDIUM - FALL_DAMAGE_FAR
        );

        for _ in 0..20 {
            prediction.hurt(FALL_DAMAGE_FAR);
        }
        assert_eq!(prediction.health(), 0, "health never falls below zero");
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
