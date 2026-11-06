//! `bg_pmove.c`: the whole player movement state machine.
//!
//! [`crate::pmove`] has the two sliders, `PM_SlideMove` and
//! `PM_StepSlideMove`, which only need the world and a box. This module is
//! everything around them: the ground and water traces, friction and
//! acceleration, the jump, duck, wade, fly, noclip and dead branches, the
//! animation and timer bookkeeping, and the weapon state machine the client
//! has to predict along with its position.
//!
//! The state is split the way the C is: `pmove_t` becomes [`Pmove`], `pml_t`
//! becomes [`Pml`], and `playerState_t` becomes [`PlayerState`]. One thing is
//! arranged differently: the C `playerState_t` owns the origin, velocity,
//! gravity and `pm_time`, while here those live in the [`MoveState`] the
//! sliders read. That keeps one copy of the fields a sweep mutates instead of
//! copying them in and out around every sweep, so [`Pmove::mv`] is the
//! authoritative position state and [`PlayerState`] holds the rest.
//!
//! The holdable-item branch of `PM_Weapon` reads `giTag` out of
//! [`crate::bg_misc`]'s item table, which is the part of `bg_misc.c` that
//! movement needs. Every branch of the C function is carried over, timings
//! included, because those timings are part of what prediction has to agree
//! with the server on.

use crate::collision::Trace;
use crate::pmove::{
    add, clip_velocity, dot, normalize, scale, slide_move, step_slide_move, MoveState, StepEvent,
    World, CONTENTS_BODY, CONTENTS_SOLID, MASK_PLAYERSOLID, MIN_WALK_NORMAL, OVERCLIP,
};

// ---------------------------------------------------------------------------
// Constants, from `bg_public.h`, `bg_local.h`, `q_shared.h` and `surfaceflags.h`
// ---------------------------------------------------------------------------

/// `pmtype_t`, in declaration order so the `>= PM_DEAD` test reads the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum PmType {
    Normal = 0,
    Noclip = 1,
    Spectator = 2,
    Dead = 3,
    Freeze = 4,
    Intermission = 5,
    SpIntermission = 6,
}

/// `weaponstate_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeaponState {
    Ready,
    Raising,
    Dropping,
    Firing,
}

/// `ENTITYNUM_WORLD`: the map itself, which never goes in the touch list.
pub const ENTITYNUM_WORLD: i32 = 0;
/// `ENTITYNUM_NONE`: no entity, which is how "in the air" is stored.
pub const ENTITYNUM_NONE: i32 = 1023;
/// `MAXTOUCH`.
pub const MAXTOUCH: usize = 32;

/// `MINS_Z`: the bottom of the standing box.
pub const MINS_Z: i32 = -24;
/// `DEFAULT_VIEWHEIGHT`.
pub const DEFAULT_VIEWHEIGHT: i32 = 26;
/// `CROUCH_VIEWHEIGHT`.
pub const CROUCH_VIEWHEIGHT: i32 = 12;
/// `DEAD_VIEWHEIGHT`.
pub const DEAD_VIEWHEIGHT: i32 = -16;

/// `JUMP_VELOCITY`.
pub const JUMP_VELOCITY: f32 = 270.0;
/// `TIMER_LAND`.
pub const TIMER_LAND: i32 = 130;
/// `TIMER_GESTURE`: `(34*66+50)`.
pub const TIMER_GESTURE: i32 = 34 * 66 + 50;
/// `ANIM_TOGGLEBIT`: flipped whenever an animation restarts.
pub const ANIM_TOGGLEBIT: i32 = 128;

/// `CONTENTS_LAVA`.
pub const CONTENTS_LAVA: i32 = 8;
/// `CONTENTS_SLIME`.
pub const CONTENTS_SLIME: i32 = 16;
/// `CONTENTS_WATER`.
pub const CONTENTS_WATER: i32 = 32;
/// `MASK_WATER`.
pub const MASK_WATER: i32 = CONTENTS_WATER | CONTENTS_LAVA | CONTENTS_SLIME;

/// `SURF_NODAMAGE`: bounce pads and the like.
pub const SURF_NODAMAGE: i32 = 0x1;
/// `SURF_SLICK`: ice.
pub const SURF_SLICK: i32 = 0x2;
/// `SURF_METALSTEPS`.
pub const SURF_METALSTEPS: i32 = 0x1000;
/// `SURF_NOSTEPS`.
pub const SURF_NOSTEPS: i32 = 0x2000;

/// `PMF_DUCKED`.
pub const PMF_DUCKED: i32 = 1;
/// `PMF_JUMP_HELD`.
pub const PMF_JUMP_HELD: i32 = 2;
/// `PMF_BACKWARDS_JUMP`.
pub const PMF_BACKWARDS_JUMP: i32 = 8;
/// `PMF_BACKWARDS_RUN`.
pub const PMF_BACKWARDS_RUN: i32 = 16;
/// `PMF_TIME_LAND`: `pm_time` is the time before another jump.
pub const PMF_TIME_LAND: i32 = 32;
/// `PMF_TIME_KNOCKBACK`: `pm_time` is an air-accelerate-only time.
pub const PMF_TIME_KNOCKBACK: i32 = 64;
/// `PMF_TIME_WATERJUMP`.
pub const PMF_TIME_WATERJUMP: i32 = 256;
/// `PMF_RESPAWNED`: cleared once attack and use are up.
pub const PMF_RESPAWNED: i32 = 512;
/// `PMF_USE_ITEM_HELD`.
pub const PMF_USE_ITEM_HELD: i32 = 1024;
/// `PMF_GRAPPLE_PULL`.
pub const PMF_GRAPPLE_PULL: i32 = 2048;
/// `PMF_INVULEXPAND`.
pub const PMF_INVULEXPAND: i32 = 16384;
/// `PMF_ALL_TIMES`.
pub const PMF_ALL_TIMES: i32 = PMF_TIME_WATERJUMP | PMF_TIME_LAND | PMF_TIME_KNOCKBACK;

/// `EF_TALK`.
pub const EF_TALK: i32 = 0x0000_1000;
/// `EF_FIRING`.
pub const EF_FIRING: i32 = 0x0000_0100;

/// `PW_QUAD`.
pub const PW_QUAD: usize = 1;
/// `PW_BATTLESUIT`.
pub const PW_BATTLESUIT: usize = 2;
/// `PW_HASTE`.
pub const PW_HASTE: usize = 3;
/// `PW_INVIS`.
pub const PW_INVIS: usize = 4;
/// `PW_REGEN`.
pub const PW_REGEN: usize = 5;
/// `PW_FLIGHT`.
pub const PW_FLIGHT: usize = 6;
/// `PW_REDFLAG`.
pub const PW_REDFLAG: usize = 7;
/// `PW_BLUEFLAG`.
pub const PW_BLUEFLAG: usize = 8;
/// `PW_INVULNERABILITY`.
pub const PW_INVULNERABILITY: usize = 14;

/// `WP_NONE`.
pub const WP_NONE: i32 = 0;
/// `WP_GAUNTLET`.
pub const WP_GAUNTLET: i32 = 1;
/// `WP_MACHINEGUN`.
pub const WP_MACHINEGUN: i32 = 2;
/// `WP_SHOTGUN`.
pub const WP_SHOTGUN: i32 = 3;
/// `WP_GRENADE_LAUNCHER`.
pub const WP_GRENADE_LAUNCHER: i32 = 4;
/// `WP_ROCKET_LAUNCHER`.
pub const WP_ROCKET_LAUNCHER: i32 = 5;
/// `WP_LIGHTNING`.
pub const WP_LIGHTNING: i32 = 6;
/// `WP_RAILGUN`.
pub const WP_RAILGUN: i32 = 7;
/// `WP_PLASMAGUN`.
pub const WP_PLASMAGUN: i32 = 8;
/// `WP_BFG`.
pub const WP_BFG: i32 = 9;
/// `WP_GRAPPLING_HOOK`.
pub const WP_GRAPPLING_HOOK: i32 = 10;
/// `WP_NUM_WEAPONS`.
pub const WP_NUM_WEAPONS: i32 = 11;

/// `STAT_HEALTH`.
pub const STAT_HEALTH: usize = 0;
/// `STAT_HOLDABLE_ITEM`.
pub const STAT_HOLDABLE_ITEM: usize = 1;
/// `STAT_WEAPONS`.
pub const STAT_WEAPONS: usize = 2;
/// `STAT_ARMOR`. Baseq3 numbering, without the Missionpack row the C puts
/// between this and `STAT_HOLDABLE_ITEM`.
pub const STAT_ARMOR: usize = 3;
/// `STAT_MAX_HEALTH`.
pub const STAT_MAX_HEALTH: usize = 6;

/// `PERS_TEAM`.
pub const PERS_TEAM: usize = 3;
/// `TEAM_SPECTATOR`.
pub const TEAM_SPECTATOR: i32 = 3;

/// `animNumber_t` values used by the move.
pub const TORSO_GESTURE: i32 = 6;
pub const TORSO_ATTACK: i32 = 7;
pub const TORSO_ATTACK2: i32 = 8;
pub const TORSO_DROP: i32 = 9;
pub const TORSO_RAISE: i32 = 10;
pub const TORSO_STAND: i32 = 11;
pub const TORSO_STAND2: i32 = 12;
pub const LEGS_WALKCR: i32 = 13;
pub const LEGS_WALK: i32 = 14;
pub const LEGS_RUN: i32 = 15;
pub const LEGS_BACK: i32 = 16;
pub const LEGS_SWIM: i32 = 17;
pub const LEGS_JUMP: i32 = 18;
pub const LEGS_LAND: i32 = 19;
pub const LEGS_JUMPB: i32 = 20;
pub const LEGS_LANDB: i32 = 21;
pub const LEGS_IDLE: i32 = 22;
pub const LEGS_IDLECR: i32 = 23;
pub const LEGS_BACKCR: i32 = 32;
pub const LEGS_BACKWALK: i32 = 33;

/// `entity_event_t` values raised by the move.
pub const EV_FOOTSTEP: i32 = 1;
pub const EV_FOOTSTEP_METAL: i32 = 2;
pub const EV_FOOTSPLASH: i32 = 3;
pub const EV_SWIM: i32 = 5;
pub const EV_STEP_4: i32 = 6;
pub const EV_STEP_8: i32 = 7;
pub const EV_STEP_12: i32 = 8;
pub const EV_STEP_16: i32 = 9;
pub const EV_FALL_SHORT: i32 = 10;
pub const EV_FALL_MEDIUM: i32 = 11;
pub const EV_FALL_FAR: i32 = 12;
pub const EV_JUMP: i32 = 14;
pub const EV_WATER_TOUCH: i32 = 15;
pub const EV_WATER_LEAVE: i32 = 16;
pub const EV_WATER_UNDER: i32 = 17;
pub const EV_WATER_CLEAR: i32 = 18;
pub const EV_NOAMMO: i32 = 21;
pub const EV_CHANGE_WEAPON: i32 = 22;
pub const EV_FIRE_WEAPON: i32 = 23;
pub const EV_USE_ITEM0: i32 = 24;
pub const EV_TAUNT: i32 = 76;

/// `BUTTON_*`.
pub const BUTTON_ATTACK: i32 = 1;
pub const BUTTON_TALK: i32 = 2;
pub const BUTTON_USE_HOLDABLE: i32 = 4;
pub const BUTTON_GESTURE: i32 = 8;
pub const BUTTON_WALKING: i32 = 16;

/// `pm_stopspeed`.
pub const PM_STOPSPEED: f32 = 100.0;
/// `pm_duckScale`.
pub const PM_DUCK_SCALE: f32 = 0.25;
/// `pm_swimScale`.
pub const PM_SWIM_SCALE: f32 = 0.50;
/// `pm_accelerate`.
pub const PM_ACCELERATE: f32 = 10.0;
/// `pm_airaccelerate`.
pub const PM_AIRACCELERATE: f32 = 1.0;
/// `pm_wateraccelerate`.
pub const PM_WATERACCELERATE: f32 = 4.0;
/// `pm_flyaccelerate`.
pub const PM_FLYACCELERATE: f32 = 8.0;
/// `pm_friction`.
pub const PM_FRICTION: f32 = 6.0;
/// `pm_waterfriction`.
pub const PM_WATERFRICTION: f32 = 1.0;
/// `pm_flightfriction`.
pub const PM_FLIGHTFRICTION: f32 = 3.0;
/// `pm_spectatorfriction`.
pub const PM_SPECTATORFRICTION: f32 = 5.0;

/// `usercmd_t`. The C stores the three moves in a signed char; they are `i32`
/// here so the arithmetic reads like the rest of the file, while the wire
/// format still clamps them on the way in.
#[derive(Clone, Copy, Debug, Default)]
pub struct UserCmd {
    pub server_time: i32,
    pub angles: [i32; 3],
    pub buttons: i32,
    pub weapon: i32,
    pub forward_move: i32,
    pub right_move: i32,
    pub up_move: i32,
}

/// The part of `playerState_t` the move reads and writes. Position, velocity,
/// gravity and `pm_time` are in [`Pmove::mv`].
#[derive(Clone, Debug)]
pub struct PlayerState {
    pub viewangles: [f32; 3],
    /// `delta_angles`: added to the command angles to make the view angles and
    /// corrected when the pitch clamp bites.
    pub delta_angles: [i32; 3],
    pub pm_type: PmType,
    pub pm_flags: i32,
    /// `ps->speed`: the player's maximum ground speed.
    pub speed: f32,
    pub ground_entity_num: i32,
    pub viewheight: i32,
    pub legs_anim: i32,
    pub legs_timer: i32,
    pub torso_anim: i32,
    pub torso_timer: i32,
    pub movement_dir: i32,
    pub bob_cycle: i32,
    pub weapon: i32,
    pub weapon_state: WeaponState,
    pub weapon_time: i32,
    pub ammo: [i32; 16],
    pub stats: [i32; 16],
    pub persistant: [i32; 16],
    pub powerups: [i32; 16],
    pub e_flags: i32,
    pub command_time: i32,
    pub pmove_frame_count: i32,
    pub grapple_point: [f32; 3],
    /// `BG_AddPredictableEventToPlayerstate`: the events this move raised, in
    /// order. The C keeps the last two in `ps->events[]`; a queue keeps them
    /// all for the caller without the two-slot wraparound.
    pub events: Vec<i32>,
}

impl PlayerState {
    /// A live player at full health, standing on nothing.
    pub fn new() -> Self {
        PlayerState {
            viewangles: [0.0; 3],
            delta_angles: [0; 3],
            pm_type: PmType::Normal,
            pm_flags: 0,
            speed: 320.0,
            ground_entity_num: ENTITYNUM_NONE,
            viewheight: DEFAULT_VIEWHEIGHT,
            legs_anim: 0,
            legs_timer: 0,
            torso_anim: 0,
            torso_timer: 0,
            movement_dir: 0,
            bob_cycle: 0,
            weapon: WP_NONE,
            weapon_state: WeaponState::Ready,
            weapon_time: 0,
            ammo: [0; 16],
            stats: [0; 16],
            persistant: [0; 16],
            powerups: [0; 16],
            e_flags: 0,
            command_time: 0,
            pmove_frame_count: 0,
            grapple_point: [0.0; 3],
            events: Vec::new(),
        }
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self::new()
    }
}

/// `pml_t`: the per-frame scratch space, cleared at the start of every
/// `PmoveSingle` so the client and the server cannot drift apart through stale
/// values.
#[derive(Clone, Debug)]
pub struct Pml {
    pub forward: [f32; 3],
    pub right: [f32; 3],
    pub up: [f32; 3],
    pub frametime: f32,
    pub msec: i32,
    pub walking: bool,
    pub ground_plane: bool,
    pub ground_trace: Trace,
    pub previous_origin: [f32; 3],
    pub previous_velocity: [f32; 3],
    pub previous_waterlevel: i32,
}

impl Default for Pml {
    fn default() -> Self {
        Pml {
            forward: [0.0; 3],
            right: [0.0; 3],
            up: [0.0; 3],
            frametime: 0.0,
            msec: 0,
            walking: false,
            ground_plane: false,
            ground_trace: Trace::default(),
            previous_origin: [0.0; 3],
            previous_velocity: [0.0; 3],
            previous_waterlevel: 0,
        }
    }
}

/// `pmove_t`, with the playerState split described in the module docs.
pub struct Pmove<'w> {
    /// The world the sweeps and point queries go to: the map's brushes in a
    /// test, the client's collision model at run time.
    pub world: &'w (dyn World + 'w),
    /// Position, velocity, gravity, the box and the `pm_time` timer.
    pub mv: MoveState,
    pub ps: PlayerState,
    pub cmd: UserCmd,
    pub pml: Pml,
    pub water_type: i32,
    pub water_level: i32,
    pub xyspeed: f32,
    /// `touchents[MAXTOUCH]`; capped at [`MAXTOUCH`].
    pub touchents: Vec<i32>,
    pub tracemask: i32,
    pub debug_level: i32,
    pub no_footsteps: bool,
    pub gauntlet_hit: bool,
    pub pmove_fixed: bool,
    pub pmove_msec: i32,
}

impl<'w> Pmove<'w> {
    /// A standing player at `origin` with the default box and clip mask.
    pub fn new(origin: [f32; 3], world: &'w (dyn World + 'w)) -> Self {
        let mut ps = PlayerState::new();
        ps.stats[STAT_HEALTH] = 100;
        ps.stats[STAT_MAX_HEALTH] = 100;
        Pmove {
            world,
            mv: MoveState::new(origin),
            ps,
            cmd: UserCmd::default(),
            pml: Pml::default(),
            water_type: 0,
            water_level: 0,
            xyspeed: 0.0,
            touchents: Vec::new(),
            tracemask: MASK_PLAYERSOLID,
            debug_level: 0,
            no_footsteps: false,
            gauntlet_hit: false,
            pmove_fixed: false,
            pmove_msec: 8,
        }
    }

    /// One sweep with the current box and clip mask.
    fn sweep(&self, start: [f32; 3], end: [f32; 3]) -> Trace {
        self.world
            .trace(start, end, self.mv.mins, self.mv.maxs, self.tracemask)
    }

    /// `pm->pointcontents` with this move's pass entity.
    fn point_contents(&self, point: [f32; 3]) -> i32 {
        self.world.point_contents(point, self.mv.client_num)
    }

    /// `PM_AddTouchEnt`: the world is skipped, a repeat is ignored, and the
    /// list is capped at [`MAXTOUCH`].
    fn add_touch_ent(&mut self, entity_num: i32) {
        if entity_num == ENTITYNUM_WORLD {
            return;
        }
        if self.touchents.len() == MAXTOUCH {
            return;
        }
        if self.touchents.contains(&entity_num) {
            return;
        }
        self.touchents.push(entity_num);
    }

    /// `PM_AddEvent`.
    fn add_event(&mut self, event: i32) {
        self.ps.events.push(event);
    }

    /// Fold what the slider recorded into the pmove results.
    fn drain_slide(&mut self) {
        for entity in std::mem::take(&mut self.mv.touched) {
            self.add_touch_ent(entity);
        }
        for event in std::mem::take(&mut self.mv.events) {
            let code = match event {
                StepEvent::Step4 => EV_STEP_4,
                StepEvent::Step8 => EV_STEP_8,
                StepEvent::Step12 => EV_STEP_12,
                StepEvent::Step16 => EV_STEP_16,
            };
            self.add_event(code);
        }
    }

    /// `PM_StartTorsoAnim`.
    fn start_torso_anim(&mut self, anim: i32) {
        if self.ps.pm_type >= PmType::Dead {
            return;
        }
        self.ps.torso_anim = ((self.ps.torso_anim & ANIM_TOGGLEBIT) ^ ANIM_TOGGLEBIT) | anim;
    }

    /// `PM_StartLegsAnim`.
    fn start_legs_anim(&mut self, anim: i32) {
        if self.ps.pm_type >= PmType::Dead {
            return;
        }
        if self.ps.legs_timer > 0 {
            return; // a high priority animation is running
        }
        self.ps.legs_anim = ((self.ps.legs_anim & ANIM_TOGGLEBIT) ^ ANIM_TOGGLEBIT) | anim;
    }

    /// `PM_ContinueLegsAnim`.
    fn continue_legs_anim(&mut self, anim: i32) {
        if (self.ps.legs_anim & !ANIM_TOGGLEBIT) == anim {
            return;
        }
        if self.ps.legs_timer > 0 {
            return;
        }
        self.start_legs_anim(anim);
    }

    /// `PM_ContinueTorsoAnim`.
    fn continue_torso_anim(&mut self, anim: i32) {
        if (self.ps.torso_anim & !ANIM_TOGGLEBIT) == anim {
            return;
        }
        if self.ps.torso_timer > 0 {
            return;
        }
        self.start_torso_anim(anim);
    }

    /// `PM_ForceLegsAnim`.
    fn force_legs_anim(&mut self, anim: i32) {
        self.ps.legs_timer = 0;
        self.start_legs_anim(anim);
    }

    /// `PM_Friction`: ground, water, flight and spectator friction, with slope
    /// movement ignored while walking.
    fn friction(&mut self) {
        let mut vec = self.mv.velocity;
        if self.pml.walking {
            vec[2] = 0.0; // ignore slope movement
        }

        let speed = length(vec);
        if speed < 1.0 {
            self.mv.velocity[0] = 0.0;
            self.mv.velocity[1] = 0.0; // allow sinking underwater
            return;
        }

        let mut drop = 0.0;

        if self.water_level <= 1
            && self.pml.walking
            && (self.pml.ground_trace.surface_flags & SURF_SLICK) == 0
            && (self.ps.pm_flags & PMF_TIME_KNOCKBACK) == 0
        {
            let control = if speed < PM_STOPSPEED {
                PM_STOPSPEED
            } else {
                speed
            };
            drop += control * PM_FRICTION * self.pml.frametime;
        }

        if self.water_level != 0 {
            drop += speed * PM_WATERFRICTION * self.water_level as f32 * self.pml.frametime;
        }

        if self.ps.powerups[PW_FLIGHT] != 0 {
            drop += speed * PM_FLIGHTFRICTION * self.pml.frametime;
        }

        if self.ps.pm_type == PmType::Spectator {
            drop += speed * PM_SPECTATORFRICTION * self.pml.frametime;
        }

        let mut newspeed = speed - drop;
        if newspeed < 0.0 {
            newspeed = 0.0;
        }
        newspeed /= speed;

        self.mv.velocity = scale(self.mv.velocity, newspeed);
    }

    /// `PM_Accelerate`, the Quake 2 style acceleration the C keeps behind its
    /// `#if 1`.
    fn accelerate(&mut self, wishdir: [f32; 3], wishspeed: f32, accel: f32) {
        let current_speed = dot(self.mv.velocity, wishdir);
        let add_speed = wishspeed - current_speed;
        if add_speed <= 0.0 {
            return;
        }
        let mut accel_speed = accel * self.pml.frametime * wishspeed;
        if accel_speed > add_speed {
            accel_speed = add_speed;
        }
        for i in 0..3 {
            self.mv.velocity[i] += accel_speed * wishdir[i];
        }
    }

    /// `PM_CmdScale`: the scale that turns the raw -127..127 movement into the
    /// player's speed without the diagonal `sqrt(2)`.
    fn cmd_scale(&self) -> f32 {
        let mut max = self.cmd.forward_move.abs();
        if self.cmd.right_move.abs() > max {
            max = self.cmd.right_move.abs();
        }
        if self.cmd.up_move.abs() > max {
            max = self.cmd.up_move.abs();
        }
        if max == 0 {
            return 0.0;
        }

        let total = ((self.cmd.forward_move * self.cmd.forward_move
            + self.cmd.right_move * self.cmd.right_move
            + self.cmd.up_move * self.cmd.up_move) as f32)
            .sqrt();
        self.ps.speed * max as f32 / (127.0 * total)
    }

    /// `PM_SetMovementDir`: the leg rotation relative to the facing direction.
    fn set_movement_dir(&mut self) {
        let f = self.cmd.forward_move;
        let r = self.cmd.right_move;
        if f != 0 || r != 0 {
            self.ps.movement_dir = match (r.signum(), f.signum()) {
                (0, 1) => 0,
                (-1, 1) => 1,
                (-1, 0) => 2,
                (-1, -1) => 3,
                (0, -1) => 4,
                (1, -1) => 5,
                (1, 0) => 6,
                _ => 7,
            };
        } else if self.ps.movement_dir == 2 {
            self.ps.movement_dir = 1;
        } else if self.ps.movement_dir == 6 {
            self.ps.movement_dir = 7;
        }
    }

    /// `PM_CheckJump`.
    fn check_jump(&mut self) -> bool {
        if (self.ps.pm_flags & PMF_RESPAWNED) != 0 {
            return false; // don't allow jump until all buttons are up
        }

        if self.cmd.up_move < 10 {
            return false; // not holding jump
        }

        if (self.ps.pm_flags & PMF_JUMP_HELD) != 0 {
            // clear upmove so cmdscale doesn't lower running speed
            self.cmd.up_move = 0;
            return false;
        }

        self.pml.ground_plane = false; // jumping away
        self.pml.walking = false;
        self.ps.pm_flags |= PMF_JUMP_HELD;

        self.ps.ground_entity_num = ENTITYNUM_NONE;
        self.mv.velocity[2] = JUMP_VELOCITY;
        self.add_event(EV_JUMP);

        if self.cmd.forward_move >= 0 {
            self.force_legs_anim(LEGS_JUMP);
            self.ps.pm_flags &= !PMF_BACKWARDS_JUMP;
        } else {
            self.force_legs_anim(LEGS_JUMPB);
            self.ps.pm_flags |= PMF_BACKWARDS_JUMP;
        }

        true
    }

    /// `PM_CheckWaterJump`.
    fn check_water_jump(&mut self) -> bool {
        if self.mv.pm_time != 0 {
            return false;
        }
        if self.water_level != 2 {
            return false;
        }

        let mut flat_forward = [self.pml.forward[0], self.pml.forward[1], 0.0];
        flat_forward = normalize(flat_forward);

        let mut spot = add(scale(flat_forward, 30.0), self.mv.origin);
        spot[2] += 4.0;
        if (self.point_contents(spot) & CONTENTS_SOLID) == 0 {
            return false;
        }

        spot[2] += 16.0;
        if self.point_contents(spot) != 0 {
            return false;
        }

        // jump out of water
        self.mv.velocity = scale(self.pml.forward, 200.0);
        self.mv.velocity[2] = 350.0;

        self.ps.pm_flags |= PMF_TIME_WATERJUMP;
        self.mv.pm_time = 2000;

        true
    }

    /// `PM_WaterJumpMove`: no control, just the fall, ending as soon as the
    /// player is going down again.
    fn water_jump_move(&mut self) {
        self.step_slide(true);

        self.mv.velocity[2] -= self.mv.gravity * self.pml.frametime;
        if self.mv.velocity[2] < 0.0 {
            self.ps.pm_flags &= !PMF_ALL_TIMES;
            self.mv.pm_time = 0;
        }
    }

    /// `PM_WaterMove`.
    fn water_move(&mut self) {
        if self.check_water_jump() {
            self.water_jump_move();
            return;
        }

        self.friction();

        let mut wishvel = [0.0f32; 3];
        let scale = self.cmd_scale();
        if scale == 0.0 {
            wishvel[2] = -60.0; // sink towards bottom
        } else {
            for i in 0..3 {
                wishvel[i] = scale * self.pml.forward[i] * self.cmd.forward_move as f32
                    + scale * self.pml.right[i] * self.cmd.right_move as f32;
            }
            wishvel[2] += scale * self.cmd.up_move as f32;
        }

        let wishspeed_raw = length(wishvel);
        let mut wishdir = normalize(wishvel);
        let mut wishspeed = wishspeed_raw;
        if wishspeed > self.ps.speed * PM_SWIM_SCALE {
            wishspeed = self.ps.speed * PM_SWIM_SCALE;
        }

        self.accelerate(wishdir, wishspeed, PM_WATERACCELERATE);

        // make sure we can go up slopes easily under water
        if self.pml.ground_plane && dot(self.mv.velocity, self.pml.ground_trace.plane.normal) < 0.0
        {
            let vel = length(self.mv.velocity);
            self.mv.velocity = clip_velocity(
                self.mv.velocity,
                self.pml.ground_trace.plane.normal,
                OVERCLIP,
            );
            wishdir = normalize(self.mv.velocity);
            self.mv.velocity = crate::pmove::scale(wishdir, vel);
        }

        self.slide(false);
    }

    /// `PM_FlyMove`: only with the flight powerup.
    fn fly_move(&mut self) {
        self.friction();

        let mut wishvel = [0.0f32; 3];
        let scale = self.cmd_scale();
        if scale != 0.0 {
            for i in 0..3 {
                wishvel[i] = scale * self.pml.forward[i] * self.cmd.forward_move as f32
                    + scale * self.pml.right[i] * self.cmd.right_move as f32;
            }
            wishvel[2] += scale * self.cmd.up_move as f32;
        }

        let wishspeed = length(wishvel);
        let wishdir = normalize(wishvel);

        self.accelerate(wishdir, wishspeed, PM_FLYACCELERATE);

        self.step_slide(false);
    }

    /// `PM_AirMove`.
    fn air_move(&mut self) {
        self.friction();

        let fmove = self.cmd.forward_move;
        let smove = self.cmd.right_move;
        let scale = self.cmd_scale();

        // set the movementDir so clients can rotate the legs for strafing
        self.set_movement_dir();

        // project moves down to flat plane
        self.pml.forward[2] = 0.0;
        self.pml.right[2] = 0.0;
        self.pml.forward = normalize(self.pml.forward);
        self.pml.right = normalize(self.pml.right);

        let mut wishvel = [0.0f32; 3];
        for i in 0..2 {
            wishvel[i] =
                self.pml.forward[i] * fmove as f32 + self.pml.right[i] * smove as f32;
        }

        let wishspeed_raw = length(wishvel);
        let wishdir = normalize(wishvel);
        let wishspeed = wishspeed_raw * scale;

        // not on ground, so little effect on velocity
        self.accelerate(wishdir, wishspeed, PM_AIRACCELERATE);

        // we may have a ground plane that is very steep, even though we don't
        // have a groundentity; slide along the steep plane
        if self.pml.ground_plane {
            self.mv.velocity = clip_velocity(
                self.mv.velocity,
                self.pml.ground_trace.plane.normal,
                OVERCLIP,
            );
        }

        self.step_slide(true);
    }

    /// `PM_GrappleMove`.
    fn grapple_move(&mut self) {
        let v = add(self.ps.grapple_point, scale(self.pml.forward, -16.0));
        let mut vel = [v[0] - self.mv.origin[0], v[1] - self.mv.origin[1], v[2] - self.mv.origin[2]];
        let vlen = length(vel);
        vel = normalize(vel);

        if vlen <= 100.0 {
            vel = scale(vel, 10.0 * vlen);
        } else {
            vel = scale(vel, 800.0);
        }

        self.mv.velocity = vel;
        self.pml.ground_plane = false;
    }

    /// `PM_WalkMove`.
    fn walk_move(&mut self) {
        if self.water_level > 2
            && dot(self.pml.forward, self.pml.ground_trace.plane.normal) > 0.0
        {
            // begin swimming
            self.water_move();
            return;
        }

        if self.check_jump() {
            // jumped away
            if self.water_level > 1 {
                self.water_move();
            } else {
                self.air_move();
            }
            return;
        }

        self.friction();

        let fmove = self.cmd.forward_move;
        let smove = self.cmd.right_move;
        let scale = self.cmd_scale();

        // set the movementDir so clients can rotate the legs for strafing
        self.set_movement_dir();

        // project moves down to flat plane
        self.pml.forward[2] = 0.0;
        self.pml.right[2] = 0.0;

        // project the forward and right directions onto the ground plane
        self.pml.forward = clip_velocity(
            self.pml.forward,
            self.pml.ground_trace.plane.normal,
            OVERCLIP,
        );
        self.pml.right =
            clip_velocity(self.pml.right, self.pml.ground_trace.plane.normal, OVERCLIP);

        self.pml.forward = normalize(self.pml.forward);
        self.pml.right = normalize(self.pml.right);

        // when going up or down slopes the wish velocity should not be zero
        let mut wishvel = [0.0f32; 3];
        for i in 0..3 {
            wishvel[i] =
                self.pml.forward[i] * fmove as f32 + self.pml.right[i] * smove as f32;
        }

        let wishspeed_raw = length(wishvel);
        let wishdir = normalize(wishvel);
        let mut wishspeed = wishspeed_raw * scale;

        // clamp the speed lower if ducking
        if (self.ps.pm_flags & PMF_DUCKED) != 0 {
            if wishspeed > self.ps.speed * PM_DUCK_SCALE {
                wishspeed = self.ps.speed * PM_DUCK_SCALE;
            }
        }

        // clamp the speed lower if wading or walking on the bottom
        if self.water_level != 0 {
            let water_scale = self.water_level as f32 / 3.0;
            let water_scale = 1.0 - (1.0 - PM_SWIM_SCALE) * water_scale;
            if wishspeed > self.ps.speed * water_scale {
                wishspeed = self.ps.speed * water_scale;
            }
        }

        // when a player gets hit, they temporarily lose full control, which
        // allows them to be moved a bit
        let slick = (self.pml.ground_trace.surface_flags & SURF_SLICK) != 0
            || (self.ps.pm_flags & PMF_TIME_KNOCKBACK) != 0;
        let accelerate = if slick {
            PM_AIRACCELERATE
        } else {
            PM_ACCELERATE
        };

        self.accelerate(wishdir, wishspeed, accelerate);

        if slick {
            self.mv.velocity[2] -= self.mv.gravity * self.pml.frametime;
        }

        let vel = length(self.mv.velocity);

        // slide along the ground plane
        self.mv.velocity = clip_velocity(
            self.mv.velocity,
            self.pml.ground_trace.plane.normal,
            OVERCLIP,
        );

        // don't decrease velocity when going up or down a slope
        self.mv.velocity = normalize(self.mv.velocity);
        self.mv.velocity = crate::pmove::scale(self.mv.velocity, vel);

        // don't do anything if standing still
        if self.mv.velocity[0] == 0.0 && self.mv.velocity[1] == 0.0 {
            return;
        }

        self.step_slide(false);
    }

    /// `PM_DeadMove`: extra friction on the ground, and no air control.
    fn dead_move(&mut self) {
        if !self.pml.walking {
            return;
        }

        let mut forward = length(self.mv.velocity);
        forward -= 20.0;
        if forward <= 0.0 {
            self.mv.velocity = [0.0; 3];
        } else {
            self.mv.velocity = normalize(self.mv.velocity);
            self.mv.velocity = scale(self.mv.velocity, forward);
        }
    }

    /// `PM_NoclipMove`: straight-line flight with no world at all.
    fn noclip_move(&mut self) {
        self.ps.viewheight = DEFAULT_VIEWHEIGHT;

        // friction
        let speed = length(self.mv.velocity);
        if speed < 1.0 {
            self.mv.velocity = [0.0; 3];
        } else {
            let friction = PM_FRICTION * 1.5; // extra friction
            let control = if speed < PM_STOPSPEED {
                PM_STOPSPEED
            } else {
                speed
            };
            let drop = control * friction * self.pml.frametime;

            let mut newspeed = speed - drop;
            if newspeed < 0.0 {
                newspeed = 0.0;
            }
            newspeed /= speed;

            self.mv.velocity = scale(self.mv.velocity, newspeed);
        }

        // accelerate
        let scale = self.cmd_scale();
        let fmove = self.cmd.forward_move;
        let smove = self.cmd.right_move;

        let mut wishvel = [0.0f32; 3];
        for i in 0..3 {
            wishvel[i] =
                self.pml.forward[i] * fmove as f32 + self.pml.right[i] * smove as f32;
        }
        wishvel[2] += self.cmd.up_move as f32;

        let wishspeed = length(wishvel) * scale;
        let wishdir = normalize(wishvel);

        self.accelerate(wishdir, wishspeed, PM_ACCELERATE);

        // move
        self.mv.origin = add(
            self.mv.origin,
            crate::pmove::scale(self.mv.velocity, self.pml.frametime),
        );
    }

    /// `PM_FootstepForSurface`.
    fn footstep_for_surface(&self) -> i32 {
        if (self.pml.ground_trace.surface_flags & SURF_NOSTEPS) != 0 {
            return 0;
        }
        if (self.pml.ground_trace.surface_flags & SURF_METALSTEPS) != 0 {
            return EV_FOOTSTEP_METAL;
        }
        EV_FOOTSTEP
    }

    /// `PM_CrashLand`: the landing animation, and the fall damage event worked
    /// out from the speed the player actually arrived at.
    fn crash_land(&mut self) {
        if (self.ps.pm_flags & PMF_BACKWARDS_JUMP) != 0 {
            self.force_legs_anim(LEGS_LANDB);
        } else {
            self.force_legs_anim(LEGS_LAND);
        }

        self.ps.legs_timer = TIMER_LAND;

        // calculate the exact velocity on landing
        let dist = self.mv.origin[2] - self.pml.previous_origin[2];
        let vel = self.pml.previous_velocity[2];
        let acc = -self.mv.gravity;

        let a = acc / 2.0;
        let b = vel;
        let c = -dist;

        let den = b * b - 4.0 * a * c;
        if den < 0.0 {
            return;
        }
        let t = (-b - den.sqrt()) / (2.0 * a);

        let mut delta = vel + t * acc;
        delta = delta * delta * 0.0001;

        // ducking while falling doubles damage
        if (self.ps.pm_flags & PMF_DUCKED) != 0 {
            delta *= 2.0;
        }

        // never take falling damage if completely underwater
        if self.water_level == 3 {
            return;
        }

        // reduce falling damage if there is standing water
        if self.water_level == 2 {
            delta *= 0.25;
        }
        if self.water_level == 1 {
            delta *= 0.5;
        }

        if delta < 1.0 {
            return;
        }

        // SURF_NODAMAGE is used for bounce pads where you don't ever want to
        // take damage or play a crunch sound
        if (self.pml.ground_trace.surface_flags & SURF_NODAMAGE) == 0 {
            if delta > 60.0 {
                self.add_event(EV_FALL_FAR);
            } else if delta > 40.0 {
                // this is a pain grunt, so don't play it if dead
                if self.ps.stats[STAT_HEALTH] > 0 {
                    self.add_event(EV_FALL_MEDIUM);
                }
            } else if delta > 7.0 {
                self.add_event(EV_FALL_SHORT);
            } else {
                let event = self.footstep_for_surface();
                self.add_event(event);
            }
        }

        // start footstep cycle over
        self.ps.bob_cycle = 0;
    }

    /// `PM_CorrectAllSolid`: jitter the player around its origin to find a
    /// spot that is not inside a brush, then trace down from there.
    fn correct_all_solid(&mut self, trace: &mut Trace) -> bool {
        for i in -1..=1 {
            for j in -1..=1 {
                for k in -1..=1 {
                    let mut point = self.mv.origin;
                    point[0] += i as f32;
                    point[1] += j as f32;
                    point[2] += k as f32;
                    *trace = self.sweep(point, point);
                    if !trace.allsolid {
                        let down = [self.mv.origin[0], self.mv.origin[1], self.mv.origin[2] - 0.25];
                        *trace = self.sweep(self.mv.origin, down);
                        self.pml.ground_trace = *trace;
                        return true;
                    }
                }
            }
        }

        self.ps.ground_entity_num = ENTITYNUM_NONE;
        self.pml.ground_plane = false;
        self.pml.walking = false;

        false
    }

    /// `PM_GroundTraceMissed`: freefall, plus the jump animation if the player
    /// has just left a ledge with nothing beneath it for a while.
    fn ground_trace_missed(&mut self) {
        if self.ps.ground_entity_num != ENTITYNUM_NONE {
            // we just transitioned into freefall
            let mut point = self.mv.origin;
            point[2] -= 64.0;

            let trace = self.sweep(self.mv.origin, point);
            if trace.fraction == 1.0 {
                if self.cmd.forward_move >= 0 {
                    self.force_legs_anim(LEGS_JUMP);
                    self.ps.pm_flags &= !PMF_BACKWARDS_JUMP;
                } else {
                    self.force_legs_anim(LEGS_JUMPB);
                    self.ps.pm_flags |= PMF_BACKWARDS_JUMP;
                }
            }
        }

        self.ps.ground_entity_num = ENTITYNUM_NONE;
        self.pml.ground_plane = false;
        self.pml.walking = false;
    }

    /// `PM_GroundTrace`.
    fn ground_trace(&mut self) {
        let point = [self.mv.origin[0], self.mv.origin[1], self.mv.origin[2] - 0.25];

        let mut trace = self.sweep(self.mv.origin, point);
        self.pml.ground_trace = trace;

        // do something corrective if the trace starts in a solid...
        if trace.allsolid && !self.correct_all_solid(&mut trace) {
            return;
        }

        // if the trace didn't hit anything, we are in free fall
        if trace.fraction == 1.0 {
            self.ground_trace_missed();
            self.pml.ground_plane = false;
            self.pml.walking = false;
            return;
        }

        // check if getting thrown off the ground
        if self.mv.velocity[2] > 0.0 && dot(self.mv.velocity, trace.plane.normal) > 10.0 {
            // go into jump animation
            if self.cmd.forward_move >= 0 {
                self.force_legs_anim(LEGS_JUMP);
                self.ps.pm_flags &= !PMF_BACKWARDS_JUMP;
            } else {
                self.force_legs_anim(LEGS_JUMPB);
                self.ps.pm_flags |= PMF_BACKWARDS_JUMP;
            }

            self.ps.ground_entity_num = ENTITYNUM_NONE;
            self.pml.ground_plane = false;
            self.pml.walking = false;
            return;
        }

        // slopes that are too steep will not be considered onground
        if trace.plane.normal[2] < MIN_WALK_NORMAL {
            self.ps.ground_entity_num = ENTITYNUM_NONE;
            self.pml.ground_plane = true;
            self.pml.walking = false;
            return;
        }

        self.pml.ground_plane = true;
        self.pml.walking = true;

        // hitting solid ground will end a waterjump
        if (self.ps.pm_flags & PMF_TIME_WATERJUMP) != 0 {
            self.ps.pm_flags &= !(PMF_TIME_WATERJUMP | PMF_TIME_LAND);
            self.mv.pm_time = 0;
        }

        if self.ps.ground_entity_num == ENTITYNUM_NONE {
            // just hit the ground
            self.crash_land();

            // don't do landing time if we were just going down a slope
            if self.pml.previous_velocity[2] < -200.0 {
                // don't allow another jump for a little while
                self.ps.pm_flags |= PMF_TIME_LAND;
                self.mv.pm_time = 250;
            }
        }

        self.ps.ground_entity_num = trace.entity_num;

        self.add_touch_ent(trace.entity_num);
    }

    /// `PM_SetWaterLevel`: the water level at the feet, waist and eyes.
    fn set_water_level(&mut self) {
        self.water_level = 0;
        self.water_type = 0;

        let mut point = self.mv.origin;
        point[2] += MINS_Z as f32 + 1.0;
        let cont = self.point_contents(point);

        if (cont & MASK_WATER) != 0 {
            let sample2 = self.ps.viewheight - MINS_Z;
            let sample1 = sample2 / 2;

            self.water_type = cont;
            self.water_level = 1;
            point[2] = self.mv.origin[2] + MINS_Z as f32 + sample1 as f32;
            let cont = self.point_contents(point);
            if (cont & MASK_WATER) != 0 {
                self.water_level = 2;
                point[2] = self.mv.origin[2] + MINS_Z as f32 + sample2 as f32;
                let cont = self.point_contents(point);
                if (cont & MASK_WATER) != 0 {
                    self.water_level = 3;
                }
            }
        }
    }

    /// `PM_CheckDuck`: sets the box and the view height.
    fn check_duck(&mut self) {
        if self.ps.powerups[PW_INVULNERABILITY] != 0 {
            if (self.ps.pm_flags & PMF_INVULEXPAND) != 0 {
                // invulnerability sphere has a 42 units radius
                self.mv.mins = [-42.0; 3];
                self.mv.maxs = [42.0; 3];
            } else {
                self.mv.mins = [-15.0, -15.0, MINS_Z as f32];
                self.mv.maxs = [15.0, 15.0, 16.0];
            }
            self.ps.pm_flags |= PMF_DUCKED;
            self.ps.viewheight = CROUCH_VIEWHEIGHT;
            return;
        }
        self.ps.pm_flags &= !PMF_INVULEXPAND;

        self.mv.mins[0] = -15.0;
        self.mv.mins[1] = -15.0;
        self.mv.maxs[0] = 15.0;
        self.mv.maxs[1] = 15.0;
        self.mv.mins[2] = MINS_Z as f32;

        if self.ps.pm_type == PmType::Dead {
            self.mv.maxs[2] = -8.0;
            self.ps.viewheight = DEAD_VIEWHEIGHT;
            return;
        }

        if self.cmd.up_move < 0 {
            // duck
            self.ps.pm_flags |= PMF_DUCKED;
        } else if (self.ps.pm_flags & PMF_DUCKED) != 0 {
            // try to stand up
            self.mv.maxs[2] = 32.0;
            let trace = self.sweep(self.mv.origin, self.mv.origin);
            if !trace.allsolid {
                self.ps.pm_flags &= !PMF_DUCKED;
            }
        }

        if (self.ps.pm_flags & PMF_DUCKED) != 0 {
            self.mv.maxs[2] = 16.0;
            self.ps.viewheight = CROUCH_VIEWHEIGHT;
        } else {
            self.mv.maxs[2] = 32.0;
            self.ps.viewheight = DEFAULT_VIEWHEIGHT;
        }
    }

    /// `PM_Footsteps`: the walk cycle and the footstep events.
    fn footsteps(&mut self) {
        self.xyspeed =
            (self.mv.velocity[0] * self.mv.velocity[0] + self.mv.velocity[1] * self.mv.velocity[1])
                .sqrt();

        if self.ps.ground_entity_num == ENTITYNUM_NONE {
            if self.ps.powerups[PW_INVULNERABILITY] != 0 {
                self.continue_legs_anim(LEGS_IDLECR);
            }
            // airborne leaves position in cycle intact, but doesn't advance
            if self.water_level > 1 {
                self.continue_legs_anim(LEGS_SWIM);
            }
            return;
        }

        // if not trying to move
        if self.cmd.forward_move == 0 && self.cmd.right_move == 0 {
            if self.xyspeed < 5.0 {
                self.ps.bob_cycle = 0; // start at beginning of cycle again
                if (self.ps.pm_flags & PMF_DUCKED) != 0 {
                    self.continue_legs_anim(LEGS_IDLECR);
                } else {
                    self.continue_legs_anim(LEGS_IDLE);
                }
            }
            return;
        }

        let mut footstep = false;
        let bobmove;

        if (self.ps.pm_flags & PMF_DUCKED) != 0 {
            bobmove = 0.5; // ducked characters bob much faster
            if (self.ps.pm_flags & PMF_BACKWARDS_RUN) != 0 {
                self.continue_legs_anim(LEGS_BACKCR);
            } else {
                self.continue_legs_anim(LEGS_WALKCR);
            }
            // ducked characters never play footsteps
        } else if (self.cmd.buttons & BUTTON_WALKING) == 0 {
            bobmove = 0.4; // faster speeds bob faster
            if (self.ps.pm_flags & PMF_BACKWARDS_RUN) != 0 {
                self.continue_legs_anim(LEGS_BACK);
            } else {
                self.continue_legs_anim(LEGS_RUN);
            }
            footstep = true;
        } else {
            bobmove = 0.3; // walking bobs slow
            if (self.ps.pm_flags & PMF_BACKWARDS_RUN) != 0 {
                self.continue_legs_anim(LEGS_BACKWALK);
            } else {
                self.continue_legs_anim(LEGS_WALK);
            }
        }

        // check for footstep / splash sounds
        let old = self.ps.bob_cycle;
        self.ps.bob_cycle = ((old as f32 + bobmove * self.pml.msec as f32) as i32) & 255;

        // if we just crossed a cycle boundary, play an appropriate footstep
        if ((old + 64) ^ (self.ps.bob_cycle + 64)) & 128 != 0 {
            if self.water_level == 0 {
                // on ground will only play sounds if running
                if footstep && !self.no_footsteps {
                    let event = self.footstep_for_surface();
                    self.add_event(event);
                }
            } else if self.water_level == 1 {
                self.add_event(EV_FOOTSPLASH);
            } else if self.water_level == 2 {
                self.add_event(EV_SWIM);
            }
            // no sound when completely underwater
        }
    }

    /// `PM_WaterEvents`: the entering, leaving and head-under events.
    fn water_events(&mut self) {
        if self.pml.previous_waterlevel == 0 && self.water_level != 0 {
            self.add_event(EV_WATER_TOUCH);
        }
        if self.pml.previous_waterlevel != 0 && self.water_level == 0 {
            self.add_event(EV_WATER_LEAVE);
        }
        if self.pml.previous_waterlevel != 3 && self.water_level == 3 {
            self.add_event(EV_WATER_UNDER);
        }
        if self.pml.previous_waterlevel == 3 && self.water_level != 3 {
            self.add_event(EV_WATER_CLEAR);
        }
    }

    /// `PM_BeginWeaponChange`.
    fn begin_weapon_change(&mut self, weapon: i32) {
        if weapon <= WP_NONE || weapon >= WP_NUM_WEAPONS {
            return;
        }

        if (self.ps.stats[STAT_WEAPONS] & (1 << weapon)) == 0 {
            return;
        }

        if self.ps.weapon_state == WeaponState::Dropping {
            return;
        }

        self.add_event(EV_CHANGE_WEAPON);
        self.ps.weapon_state = WeaponState::Dropping;
        self.ps.weapon_time += 200;
        self.start_torso_anim(TORSO_DROP);
    }

    /// `PM_FinishWeaponChange`.
    fn finish_weapon_change(&mut self) {
        let mut weapon = self.cmd.weapon;
        if weapon < WP_NONE || weapon >= WP_NUM_WEAPONS {
            weapon = WP_NONE;
        }

        if (self.ps.stats[STAT_WEAPONS] & (1 << weapon)) == 0 {
            weapon = WP_NONE;
        }

        self.ps.weapon = weapon;
        self.ps.weapon_state = WeaponState::Raising;
        self.ps.weapon_time += 250;
        self.start_torso_anim(TORSO_RAISE);
    }

    /// `PM_TorsoAnimation`.
    fn torso_animation(&mut self) {
        if self.ps.weapon_state == WeaponState::Ready {
            if self.ps.weapon == WP_GAUNTLET {
                self.continue_torso_anim(TORSO_STAND2);
            } else {
                self.continue_torso_anim(TORSO_STAND);
            }
        }
    }

    /// `PM_Weapon`: the weapon timings, the holdable items and the
    /// fire/change events.
    fn weapon(&mut self) {
        // don't allow attack until all buttons are up
        if (self.ps.pm_flags & PMF_RESPAWNED) != 0 {
            return;
        }

        // ignore if spectator
        if self.ps.persistant[PERS_TEAM] == TEAM_SPECTATOR {
            return;
        }

        // check for dead player
        if self.ps.stats[STAT_HEALTH] <= 0 {
            self.ps.weapon = WP_NONE;
            return;
        }

        // check for item using
        if (self.cmd.buttons & BUTTON_USE_HOLDABLE) != 0 {
            if (self.ps.pm_flags & PMF_USE_ITEM_HELD) == 0 {
                let tag = crate::bg_misc::tag_of(self.ps.stats[STAT_HOLDABLE_ITEM]);
                if tag == crate::bg_misc::HI_MEDKIT
                    && self.ps.stats[STAT_HEALTH]
                        >= self.ps.stats[STAT_MAX_HEALTH] + 25
                {
                    // don't use medkit if at max health
                } else {
                    self.ps.pm_flags |= PMF_USE_ITEM_HELD;
                    self.add_event(EV_USE_ITEM0 + tag);
                    self.ps.stats[STAT_HOLDABLE_ITEM] = 0;
                }
                return;
            }
        } else {
            self.ps.pm_flags &= !PMF_USE_ITEM_HELD;
        }

        // make weapon function
        if self.ps.weapon_time > 0 {
            self.ps.weapon_time -= self.pml.msec;
        }

        // check for weapon change
        // can't change if weapon is firing, but can change again if lowering
        // or raising
        if self.ps.weapon_time <= 0 || self.ps.weapon_state != WeaponState::Firing {
            if self.ps.weapon != self.cmd.weapon {
                let weapon = self.cmd.weapon;
                self.begin_weapon_change(weapon);
            }
        }

        if self.ps.weapon_time > 0 {
            return;
        }

        // change weapon if time
        if self.ps.weapon_state == WeaponState::Dropping {
            self.finish_weapon_change();
            return;
        }

        if self.ps.weapon_state == WeaponState::Raising {
            self.ps.weapon_state = WeaponState::Ready;
            if self.ps.weapon == WP_GAUNTLET {
                self.start_torso_anim(TORSO_STAND2);
            } else {
                self.start_torso_anim(TORSO_STAND);
            }
            return;
        }

        // check for fire
        if (self.cmd.buttons & BUTTON_ATTACK) == 0 {
            self.ps.weapon_time = 0;
            self.ps.weapon_state = WeaponState::Ready;
            return;
        }

        // start the animation even if out of ammo
        if self.ps.weapon == WP_GAUNTLET {
            // the gauntlet only "fires" when it actually hits something
            if !self.gauntlet_hit {
                self.ps.weapon_time = 0;
                self.ps.weapon_state = WeaponState::Ready;
                return;
            }
            self.start_torso_anim(TORSO_ATTACK2);
        } else {
            self.start_torso_anim(TORSO_ATTACK);
        }

        self.ps.weapon_state = WeaponState::Firing;

        // check for out of ammo
        let weapon = self.ps.weapon as usize;
        if self.ps.ammo.get(weapon).copied().unwrap_or(0) == 0 {
            self.add_event(EV_NOAMMO);
            self.ps.weapon_time += 500;
            return;
        }

        // take an ammo away if not infinite
        if self.ps.ammo[weapon] != -1 {
            self.ps.ammo[weapon] -= 1;
        }

        // fire weapon
        self.add_event(EV_FIRE_WEAPON);

        let mut add_time = match self.ps.weapon {
            WP_SHOTGUN => 1000,
            WP_GRENADE_LAUNCHER => 800,
            WP_ROCKET_LAUNCHER => 800,
            WP_LIGHTNING => 50,
            WP_RAILGUN => 1500,
            WP_MACHINEGUN => 100,
            WP_PLASMAGUN => 100,
            WP_BFG => 200,
            WP_GRAPPLING_HOOK => 400,
            WP_GAUNTLET => 400,
            _ => 400,
        };

        if self.ps.powerups[PW_HASTE] != 0 {
            add_time = (add_time as f32 / 1.3) as i32;
        }

        self.ps.weapon_time += add_time;
    }

    /// `PM_Animate`.
    fn animate(&mut self) {
        if (self.cmd.buttons & BUTTON_GESTURE) != 0 {
            if self.ps.torso_timer == 0 {
                self.start_torso_anim(TORSO_GESTURE);
                self.ps.torso_timer = TIMER_GESTURE;
                self.add_event(EV_TAUNT);
            }
        }
    }

    /// `PM_DropTimers`.
    fn drop_timers(&mut self) {
        if self.mv.pm_time != 0 {
            if self.pml.msec >= self.mv.pm_time {
                self.ps.pm_flags &= !PMF_ALL_TIMES;
                self.mv.pm_time = 0;
            } else {
                self.mv.pm_time -= self.pml.msec;
            }
        }

        if self.ps.legs_timer > 0 {
            self.ps.legs_timer -= self.pml.msec;
            if self.ps.legs_timer < 0 {
                self.ps.legs_timer = 0;
            }
        }

        if self.ps.torso_timer > 0 {
            self.ps.torso_timer -= self.pml.msec;
            if self.ps.torso_timer < 0 {
                self.ps.torso_timer = 0;
            }
        }
    }

    /// The slider half of `PM_SlideMove`, with the ground plane this move
    /// found handed to the slider.
    fn slide(&mut self, gravity: bool) {
        // The sliders keep the frame time and clip mask in the move state.
        self.mv.frametime = self.pml.frametime;
        self.mv.trace_mask = self.tracemask;
        self.mv.ground = if self.pml.ground_plane {
            Some(self.pml.ground_trace.plane.normal)
        } else {
            None
        };
        let world = self.world;
        slide_move(world, &mut self.mv, gravity);
        self.drain_slide();
    }

    /// `PM_StepSlideMove`.
    fn step_slide(&mut self, gravity: bool) {
        self.mv.frametime = self.pml.frametime;
        self.mv.trace_mask = self.tracemask;
        self.mv.ground = if self.pml.ground_plane {
            Some(self.pml.ground_trace.plane.normal)
        } else {
            None
        };
        let world = self.world;
        step_slide_move(world, &mut self.mv, gravity);
        self.drain_slide();
    }

    /// `PmoveSingle`.
    pub fn pmove_single(&mut self) {
        // clear results
        self.touchents.clear();
        self.water_type = 0;
        self.water_level = 0;

        if self.ps.stats[STAT_HEALTH] <= 0 {
            self.tracemask &= !CONTENTS_BODY; // corpses can fly through bodies
        }

        // make sure walking button is clear if they are running, to avoid
        // proxy no-footsteps cheats
        if self.cmd.forward_move.abs() > 64 || self.cmd.right_move.abs() > 64 {
            self.cmd.buttons &= !BUTTON_WALKING;
        }

        // set the talk balloon flag
        if (self.cmd.buttons & BUTTON_TALK) != 0 {
            self.ps.e_flags |= EF_TALK;
        } else {
            self.ps.e_flags &= !EF_TALK;
        }

        // set the firing flag for continuous beam weapons
        if (self.ps.pm_flags & PMF_RESPAWNED) == 0
            && self.ps.pm_type != PmType::Intermission
            && (self.cmd.buttons & BUTTON_ATTACK) != 0
            && self
                .ps
                .ammo
                .get(self.ps.weapon as usize)
                .copied()
                .unwrap_or(0)
                != 0
        {
            self.ps.e_flags |= EF_FIRING;
        } else {
            self.ps.e_flags &= !EF_FIRING;
        }

        // clear the respawned flag if attack and use are cleared
        if self.ps.stats[STAT_HEALTH] > 0
            && (self.cmd.buttons & (BUTTON_ATTACK | BUTTON_USE_HOLDABLE)) == 0
        {
            self.ps.pm_flags &= !PMF_RESPAWNED;
        }

        // if talk button is down, dissallow all other input
        // this is to prevent any possible intercept proxy from adding fake
        // talk balloons
        if (self.cmd.buttons & BUTTON_TALK) != 0 {
            // keep the talk button set tho for when the cmd.serverTime > 66
            // msec and the same cmd is used multiple times in Pmove
            self.cmd.buttons = BUTTON_TALK;
            self.cmd.forward_move = 0;
            self.cmd.right_move = 0;
            self.cmd.up_move = 0;
        }

        // clear all pmove local vars
        self.pml = Pml::default();

        // determine the time
        self.pml.msec = self.cmd.server_time - self.ps.command_time;
        if self.pml.msec < 1 {
            self.pml.msec = 1;
        } else if self.pml.msec > 200 {
            self.pml.msec = 200;
        }
        self.ps.command_time = self.cmd.server_time;

        // save old org in case we get stuck
        self.pml.previous_origin = self.mv.origin;

        // save old velocity for crashlanding
        self.pml.previous_velocity = self.mv.velocity;

        self.pml.frametime = self.pml.msec as f32 * 0.001;

        // update the viewangles
        update_view_angles(&mut self.ps, &self.cmd);

        let (forward, right, up) = angle_vectors(self.ps.viewangles);
        self.pml.forward = forward;
        self.pml.right = right;
        self.pml.up = up;

        if self.cmd.up_move < 10 {
            // not holding jump
            self.ps.pm_flags &= !PMF_JUMP_HELD;
        }

        // decide if backpedaling animations should be used
        if self.cmd.forward_move < 0 {
            self.ps.pm_flags |= PMF_BACKWARDS_RUN;
        } else if self.cmd.forward_move > 0
            || (self.cmd.forward_move == 0 && self.cmd.right_move != 0)
        {
            self.ps.pm_flags &= !PMF_BACKWARDS_RUN;
        }

        if self.ps.pm_type >= PmType::Dead {
            self.cmd.forward_move = 0;
            self.cmd.right_move = 0;
            self.cmd.up_move = 0;
        }

        if self.ps.pm_type == PmType::Spectator {
            self.check_duck();
            self.fly_move();
            self.drop_timers();
            return;
        }

        if self.ps.pm_type == PmType::Noclip {
            self.noclip_move();
            self.drop_timers();
            return;
        }

        if self.ps.pm_type == PmType::Freeze {
            return; // no movement at all
        }

        if self.ps.pm_type == PmType::Intermission || self.ps.pm_type == PmType::SpIntermission {
            return; // no movement at all
        }

        // set watertype, and waterlevel
        self.set_water_level();
        self.pml.previous_waterlevel = self.water_level;

        // set mins, maxs, and viewheight
        self.check_duck();

        // set groundentity
        self.ground_trace();

        if self.ps.pm_type == PmType::Dead {
            self.dead_move();
        }

        self.drop_timers();

        if self.ps.powerups[PW_FLIGHT] != 0 {
            // flight powerup doesn't allow jump and has different friction
            self.fly_move();
        } else if (self.ps.pm_flags & PMF_GRAPPLE_PULL) != 0 {
            self.grapple_move();
            // We can wiggle a bit
            self.air_move();
        } else if (self.ps.pm_flags & PMF_TIME_WATERJUMP) != 0 {
            self.water_jump_move();
        } else if self.water_level > 1 {
            // swimming
            self.water_move();
        } else if self.pml.walking {
            // walking on ground
            self.walk_move();
        } else {
            // airborne
            self.air_move();
        }

        self.animate();

        // set groundentity, watertype, and waterlevel
        self.ground_trace();
        self.set_water_level();

        // weapons
        self.weapon();

        // torso animation
        self.torso_animation();

        // footstep events / legs animations
        self.footsteps();

        // entering / leaving water splashes
        self.water_events();

        // snap some parts of playerstate to save network bandwidth
        snap_vector(&mut self.mv.velocity);
    }
}

/// `PM_UpdateViewAngles`: clamp the pitch to straight up and down, then turn
/// the command angles into view angles.
pub fn update_view_angles(ps: &mut PlayerState, cmd: &UserCmd) {
    if ps.pm_type == PmType::Intermission || ps.pm_type == PmType::SpIntermission {
        return; // no view changes at all
    }

    if ps.pm_type != PmType::Spectator && ps.stats[STAT_HEALTH] <= 0 {
        return; // no view changes at all
    }

    // circularly clamp the angles with deltas
    for i in 0..3 {
        let mut temp = cmd.angles[i] + ps.delta_angles[i];
        if i == 0 {
            // don't let the player look up or down more than 90 degrees
            if temp > 16000 {
                ps.delta_angles[i] = 16000 - cmd.angles[i];
                temp = 16000;
            } else if temp < -16000 {
                ps.delta_angles[i] = -16000 - cmd.angles[i];
                temp = -16000;
            }
        }
        ps.viewangles[i] = short_to_angle(temp);
    }
}

/// `Pmove`: chop a long frame into pieces so the movement does not depend on
/// the client's frame rate.
pub fn pmove(pm: &mut Pmove) {
    let final_time = pm.cmd.server_time;

    if final_time < pm.ps.command_time {
        return; // should not happen
    }

    if final_time > pm.ps.command_time + 1000 {
        pm.ps.command_time = final_time - 1000;
    }

    pm.ps.pmove_frame_count = (pm.ps.pmove_frame_count + 1) & 63;

    // chop the move up if it is too long, to prevent framerate dependent
    // behavior
    while pm.ps.command_time != final_time {
        let mut msec = final_time - pm.ps.command_time;

        if pm.pmove_fixed {
            if msec > pm.pmove_msec {
                msec = pm.pmove_msec;
            }
        } else if msec > 66 {
            msec = 66;
        }
        pm.cmd.server_time = pm.ps.command_time + msec;
        pm.pmove_single();

        if (pm.ps.pm_flags & PMF_JUMP_HELD) != 0 {
            pm.cmd.up_move = 20;
        }
    }
}

fn length(v: [f32; 3]) -> f32 {
    dot(v, v).sqrt()
}

/// `AngleVectors`: pitch, yaw and roll in degrees to three basis vectors.
fn angle_vectors(angles: [f32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    const DEG2RAD: f32 = std::f32::consts::PI / 180.0;
    let (sp, cp) = (angles[0] * DEG2RAD).sin_cos();
    let (sy, cy) = (angles[1] * DEG2RAD).sin_cos();
    let (sr, cr) = (angles[2] * DEG2RAD).sin_cos();

    let forward = [cp * cy, cp * sy, -sp];
    let right = [
        -sr * sp * cy + cr * sy,
        -sr * sp * sy + cr * cy,
        -sr * cp,
    ];
    let up = [cr * sp * cy + sr * sy, cr * sp * sy - sr * cy, cr * cp];
    (forward, right, up)
}

/// `SHORT2ANGLE`.
fn short_to_angle(value: i32) -> f32 {
    value as f32 * (360.0 / 65536.0)
}

/// `trap_SnapVector`: round the components to whole units so tiny prediction
/// differences do not leak into the network state.
fn snap_vector(v: &mut [f32; 3]) {
    for component in v.iter_mut() {
        *component = (*component + 0.5).floor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collision::tests::arena;

    /// A floor at z = 0 with a wall from x = 64 to 128 that is too tall to
    /// step onto.
    fn room() -> crate::collision::Collision {
        arena(&[
            ([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]),
            ([64.0, -512.0, 0.0], [128.0, 512.0, 256.0]),
        ])
    }

    /// The room plus a water volume: traces see the brushes, point queries
    /// add `CONTENTS_WATER` inside the box.
    struct WaterWorld {
        solid: crate::collision::Collision,
        water: [[f32; 3]; 2],
    }

    impl World for WaterWorld {
        fn trace(
            &self,
            start: [f32; 3],
            end: [f32; 3],
            mins: [f32; 3],
            maxs: [f32; 3],
            mask: i32,
        ) -> Trace {
            self.solid.box_trace(start, end, mins, maxs, mask)
        }

        fn point_contents(&self, point: [f32; 3], _pass_entity: i32) -> i32 {
            let mut contents =
                crate::collision::Collision::point_contents(&self.solid, point);
            if (0..3).all(|i| point[i] >= self.water[0][i] && point[i] <= self.water[1][i]) {
                contents |= CONTENTS_WATER;
            }
            contents
        }
    }

    /// A player standing in the air above the floor, with a command time of
    /// zero and a tenth of a second per frame.
    fn falling<'w>(world: &'w (dyn World + 'w)) -> Pmove<'w> {
        let mut pm = Pmove::new([0.0, 0.0, 100.0], world);
        pm.cmd.server_time = 100;
        pm
    }

    /// Run frames until the player is standing on something.
    fn settle(pm: &mut Pmove) {
        for _ in 0..40 {
            pm.cmd.server_time += 100;
            pm.pmove_single();
            if pm.ps.ground_entity_num != ENTITYNUM_NONE {
                return;
            }
        }
        panic!("the player never landed: {:?}", pm.mv.origin);
    }

    #[test]
    fn a_falling_player_lands_on_the_floor() {
        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        assert!((pm.mv.origin[2] - 24.125).abs() < 1e-2, "{:?}", pm.mv.origin);
        assert_eq!(pm.ps.ground_entity_num, ENTITYNUM_WORLD);
        assert!(pm.pml.walking);
        assert!(pm.mv.velocity[2].abs() < 1.0, "{:?}", pm.mv.velocity);
    }

    #[test]
    fn a_jump_leaves_the_ground_going_up() {
        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        pm.cmd.server_time += 100;
        pm.cmd.up_move = 20;
        pm.pmove_single();

        assert_eq!(pm.ps.ground_entity_num, ENTITYNUM_NONE);
        assert!(!pm.pml.walking);
        assert!(pm.mv.velocity[2] > 100.0, "{:?}", pm.mv.velocity);
        assert!(pm.ps.events.contains(&EV_JUMP));
        assert_eq!(pm.ps.legs_anim & !ANIM_TOGGLEBIT, LEGS_JUMP);
    }

    #[test]
    fn ducking_lowers_the_box_and_the_eyes() {
        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        pm.cmd.server_time += 100;
        pm.cmd.up_move = -1;
        pm.pmove_single();

        assert_eq!(pm.mv.maxs[2], 16.0);
        assert_eq!(pm.ps.viewheight, CROUCH_VIEWHEIGHT);
        assert_ne!(pm.ps.pm_flags & PMF_DUCKED, 0);
    }

    #[test]
    fn firing_uses_a_round_and_sets_the_weapon_timer() {
        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        pm.ps.weapon = WP_MACHINEGUN;
        pm.ps.ammo[WP_MACHINEGUN as usize] = 100;
        pm.ps.stats[STAT_WEAPONS] |= 1 << WP_MACHINEGUN;
        pm.cmd.weapon = WP_MACHINEGUN;
        pm.cmd.buttons = BUTTON_ATTACK;
        pm.cmd.server_time += 100;
        pm.pmove_single();

        assert_eq!(pm.ps.ammo[WP_MACHINEGUN as usize], 99);
        assert_eq!(pm.ps.weapon_time, 100);
        assert_eq!(pm.ps.weapon_state, WeaponState::Firing);
        assert!(pm.ps.events.contains(&EV_FIRE_WEAPON));
    }

    /// A standing player at the usual floor height, one frame in.
    fn standing_in<'w>(world: &'w (dyn World + 'w)) -> Pmove<'w> {
        let mut pm = Pmove::new([0.0, 0.0, 24.125], world);
        pm.cmd.server_time = 100;
        pm.pmove_single();
        pm
    }

    #[test]
    fn water_levels_follow_the_feet_waist_and_eyes() {
        // The feet sample sits 1.125 above the floor, the waist at 25.125 and
        // the eyes at 50.125, so each top covers exactly one more sample.
        for (top, expected) in [(2.0, 1), (30.0, 2), (100.0, 3)] {
            let world = WaterWorld {
                solid: room(),
                water: [[-512.0, -512.0, -64.0], [512.0, 512.0, top]],
            };
            let pm = standing_in(&world);
            assert_eq!(pm.water_level, expected, "water top {top}");
            assert_ne!(pm.water_type & CONTENTS_WATER, 0, "water top {top}");
        }
    }

    #[test]
    fn a_water_jump_pushes_off_a_ledge() {
        // Chest-high water, with a ledge at x = 20 whose top is above the
        // surface, so the jump spot is solid and the spot above it is clear.
        let solid = arena(&[
            ([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]),
            ([20.0, -512.0, 0.0], [40.0, 512.0, 40.0]),
        ]);
        let world = WaterWorld {
            solid,
            water: [[-512.0, -512.0, -64.0], [512.0, 512.0, 30.0]],
        };
        let pm = standing_in(&world);

        assert_eq!(pm.water_level, 2);
        assert_ne!(pm.ps.pm_flags & PMF_TIME_WATERJUMP, 0);
        assert_eq!(pm.mv.pm_time, 2000);
        assert_eq!(pm.mv.velocity[0], 200.0);
        // The waterjump timer makes the slide put back the velocity it started
        // with (350), then the frame's fall comes off once on top of that.
        assert!((pm.mv.velocity[2] - 270.0).abs() < 1.0, "{:?}", pm.mv.velocity);
    }

    #[test]
    fn swimming_sinks_without_input() {
        let world = WaterWorld {
            solid: room(),
            water: [[-512.0, -512.0, -64.0], [512.0, 512.0, 100.0]],
        };
        let mut pm = Pmove::new([0.0, 0.0, 40.0], &world);
        pm.cmd.server_time = 100;
        pm.pmove_single();

        assert_eq!(pm.water_level, 3);
        assert!(pm.mv.velocity[2] < 0.0, "{:?}", pm.mv.velocity);
        assert!(pm.mv.origin[2] < 40.0, "{:?}", pm.mv.origin);
    }

    #[test]
    fn a_medkit_is_not_used_at_the_health_cap() {
        use crate::bg_misc::{BG_ITEMLIST, HI_MEDKIT};

        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        let medkit = BG_ITEMLIST
            .iter()
            .position(|item| item.tag == HI_MEDKIT)
            .unwrap() as i32;
        pm.ps.stats[STAT_HOLDABLE_ITEM] = medkit;
        pm.ps.stats[STAT_HEALTH] = pm.ps.stats[STAT_MAX_HEALTH] + 25;
        pm.cmd.buttons = BUTTON_USE_HOLDABLE;
        pm.cmd.server_time += 100;
        pm.pmove_single();

        assert_eq!(pm.ps.stats[STAT_HOLDABLE_ITEM], medkit);
        assert_eq!(pm.ps.pm_flags & PMF_USE_ITEM_HELD, 0);
        assert!(!pm.ps.events.iter().any(|&e| e == EV_USE_ITEM0 + HI_MEDKIT));
    }

    #[test]
    fn a_holdable_is_used_once_and_cleared() {
        use crate::bg_misc::{BG_ITEMLIST, HI_TELEPORTER};

        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);

        let teleporter = BG_ITEMLIST
            .iter()
            .position(|item| item.tag == HI_TELEPORTER)
            .unwrap() as i32;
        pm.ps.stats[STAT_HOLDABLE_ITEM] = teleporter;
        pm.cmd.buttons = BUTTON_USE_HOLDABLE;
        pm.cmd.server_time += 100;
        pm.pmove_single();

        assert_eq!(pm.ps.stats[STAT_HOLDABLE_ITEM], 0);
        assert_ne!(pm.ps.pm_flags & PMF_USE_ITEM_HELD, 0);
        assert!(pm.ps.events.contains(&(EV_USE_ITEM0 + HI_TELEPORTER)));

        // Holding the button down does not use the now-empty slot again.
        pm.ps.events.clear();
        pm.cmd.server_time += 100;
        pm.pmove_single();
        assert!(pm.ps.events.is_empty());
    }

    #[test]
    fn noclip_ignores_the_wall() {
        let world = room();
        let mut pm = Pmove::new([0.0, 0.0, 24.125], &world);
        pm.ps.pm_type = PmType::Noclip;
        pm.cmd.forward_move = 127;

        for frame in 1..=4 {
            pm.cmd.server_time = frame * 100;
            pm.pmove_single();
        }

        assert!(pm.mv.origin[0] > 64.0, "{:?}", pm.mv.origin);
    }

    #[test]
    fn a_walking_player_stops_at_the_wall() {
        let world = room();
        let mut pm = falling(&world);
        settle(&mut pm);
        pm.cmd.forward_move = 127;

        for _ in 0..9 {
            pm.cmd.server_time += 100;
            pm.pmove_single();
        }

        // The box reaches 15 forward and the trace stops a clip epsilon short
        // of the wall at 64.
        assert!(pm.mv.origin[0] < 49.0, "{:?}", pm.mv.origin);
        assert!(pm.mv.origin[0] > 47.0, "{:?}", pm.mv.origin);
        assert_eq!(pm.ps.ground_entity_num, ENTITYNUM_WORLD);
    }

    #[test]
    fn a_pmove_chops_a_long_frame_into_sixty_six_millisecond_pieces() {
        let world = room();
        let mut pm = falling(&world);
        pm.cmd.server_time = 200;
        pmove(&mut pm);

        assert_eq!(pm.ps.command_time, 200);
        assert_eq!(pm.ps.pmove_frame_count, 1);
        // Two 66 msec pieces and a 68 msec remainder still add up to the
        // command time; the origin moved every piece.
        assert!(pm.mv.origin[2] < 100.0);
    }
}
