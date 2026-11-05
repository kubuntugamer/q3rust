//! Player movement against the world: `PM_SlideMove`, `PM_StepSlideMove` and
//! the `PM_ClipVelocity` they clip with, from `bg_slidemove.c` and
//! `bg_pmove.c`.
//!
//! In the C tree these live inside the game module: `bg_pmove.c` and
//! `bg_slidemove.c` are compiled into both `qagame` and `cgame`, so the server
//! never runs them and the client runs them to predict its own movement. This
//! is the client's copy, which is why it takes a [`World`] instead of calling
//! `pm->trace`: the same sweep serves the map's brushes in a test and the
//! client's world at run time.
//!
//! The rewrite keeps the C control flow, including the parts that look odd:
//! four bumps, five clip planes, the nudge along a plane that is hit twice, and
//! stopping dead when a third plane is entered. Those are the behaviour the
//! client predicts with; a tidier version would predict differently from the
//! server, and the player would feel the difference as jitter.
//!
//! Two C corners are made defined here rather than copied. `planes[]` gets a
//! zero vector when the velocity is zero, where `VectorNormalize2` leaves the
//! destination untouched, and `endVelocity` starts as the velocity when
//! `gravity` is false, where the C variable is uninitialised but still read by
//! the clip code inside the bump loop. Both are values the C code never means
//! to depend on.

use crate::collision::{Collision, Trace};

/// `MAX_CLIP_PLANES` from `bg_slidemove.c`: the planes one move can collect.
pub const MAX_CLIP_PLANES: usize = 5;
/// `OVERCLIP` from `bg_local.h`: clipping leaves the velocity this much into
/// the plane, so the next trace starts behind it instead of exactly on it.
pub const OVERCLIP: f32 = 1.001;
/// `STEPSIZE` from `bg_local.h`: the tallest lip a walking player climbs.
pub const STEPSIZE: f32 = 18.0;
/// `MIN_WALK_NORMAL` from `bg_local.h`: the flattest normal walked on.
pub const MIN_WALK_NORMAL: f32 = 0.7;
/// How many bumps one slide move makes before giving up.
const NUMBUMPS: usize = 4;

/// `CONTENTS_SOLID` from `q_shared.h`.
pub const CONTENTS_SOLID: i32 = 1;
/// `CONTENTS_PLAYERCLIP`: invisible walls that stop players.
pub const CONTENTS_PLAYERCLIP: i32 = 0x0001_0000;
/// `CONTENTS_BODY`: a player or a corpse.
pub const CONTENTS_BODY: i32 = 0x0200_0000;
/// `MASK_PLAYERSOLID` from `bg_public.h`: what a player's move collides with.
pub const MASK_PLAYERSOLID: i32 = CONTENTS_SOLID | CONTENTS_PLAYERCLIP | CONTENTS_BODY;

/// `playerMins` from `bg_pmove.c`.
pub const PLAYER_MINS: [f32; 3] = [-15.0, -15.0, -24.0];
/// `playerMaxs` from `bg_pmove.c`.
pub const PLAYER_MAXS: [f32; 3] = [15.0, 15.0, 32.0];

/// The step events `PM_StepSlideMove` asks for, `EV_STEP_4` through
/// `EV_STEP_16`. The caller turns these into `EV_` events on the player state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepEvent {
    Step4,
    Step8,
    Step12,
    Step16,
}

/// Anything a move can sweep: the map's brushes, and in the client also the
/// other entities.
pub trait World {
    fn trace(
        &self,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        mask: i32,
    ) -> Trace;

    /// `pm->pointcontents`: the contents bits at a point. A world that does
    /// not model water (the empty test arena, for one) can leave the default,
    /// which is "no water anywhere".
    fn point_contents(&self, point: [f32; 3], pass_entity: i32) -> i32 {
        let _ = (point, pass_entity);
        0
    }
}

impl World for Collision {
    fn trace(
        &self,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        mask: i32,
    ) -> Trace {
        self.box_trace(start, end, mins, maxs, mask)
    }

    /// `pm->pointcontents`: the world's contents at a point. The pass entity
    /// matters only when entities take part in the query, which the map alone
    /// never does.
    fn point_contents(&self, point: [f32; 3], pass_entity: i32) -> i32 {
        let _ = pass_entity;
        Collision::point_contents(self, point)
    }
}

/// The parts of `pmove_t` and `pml_t` that sliding reads and writes.
#[derive(Clone, Debug)]
pub struct MoveState {
    /// `pm->ps->origin`: the feet of the box, not its centre.
    pub origin: [f32; 3],
    /// `pm->ps->velocity`, in units per second.
    pub velocity: [f32; 3],
    pub mins: [f32; 3],
    pub maxs: [f32; 3],
    /// `pm->ps->gravity`, in units per second squared.
    pub gravity: f32,
    /// `pml.frametime`, in seconds.
    pub frametime: f32,
    pub trace_mask: i32,
    pub client_num: i32,
    /// `pml.groundPlane`: the normal of the plane the player stands on, when
    /// the walk code found one.
    pub ground: Option<[f32; 3]>,
    /// `pml.impactSpeed`: the hardest hit this move made, for fall damage.
    pub impact_speed: f32,
    /// `pm->ps->pm_time`: while it is running the original velocity is put
    /// back at the end of a slide.
    pub pm_time: i32,
    /// `PM_AddTouchEnt`: the entities the sweeps hit, in order.
    pub touched: Vec<i32>,
    /// The step events the move asked for.
    pub events: Vec<StepEvent>,
}

impl MoveState {
    /// A standing player at `origin`, not moving.
    pub fn new(origin: [f32; 3]) -> Self {
        MoveState {
            origin,
            velocity: [0.0; 3],
            mins: PLAYER_MINS,
            maxs: PLAYER_MAXS,
            gravity: 800.0,
            frametime: 0.0,
            trace_mask: MASK_PLAYERSOLID,
            client_num: 0,
            ground: None,
            impact_speed: 0.0,
            pm_time: 0,
            touched: Vec::new(),
            events: Vec::new(),
        }
    }

    /// One sweep with this player's box and clip mask.
    fn sweep<W: World + ?Sized>(&self, world: &W, start: [f32; 3], end: [f32; 3]) -> Trace {
        world.trace(start, end, self.mins, self.maxs, self.trace_mask)
    }
}

/// `PM_ClipVelocity`: remove the part of `input` that goes into the plane, with
/// the overbounce that leaves it a little inside.
pub fn clip_velocity(input: [f32; 3], normal: [f32; 3], overbounce: f32) -> [f32; 3] {
    let mut backoff = dot(input, normal);
    if backoff < 0.0 {
        backoff *= overbounce;
    } else {
        backoff /= overbounce;
    }
    [
        input[0] - normal[0] * backoff,
        input[1] - normal[1] * backoff,
        input[2] - normal[2] * backoff,
    ]
}

/// `PM_SlideMove`: move the player as far as the world allows, sliding along
/// what it hits. Returns true when a sweep was clipped, which is what tells
/// [`step_slide_move`] that there may be a step to climb.
pub fn slide_move<W: World + ?Sized>(world: &W, state: &mut MoveState, gravity: bool) -> bool {
    let mut velocity = state.velocity;
    let primal_velocity = velocity;
    // The velocity at the end of the frame, which is what the move ends with
    // when gravity is on: the trace runs at the average, the frame ends at the
    // lower value.
    let mut end_velocity = velocity;

    if gravity {
        end_velocity[2] -= state.gravity * state.frametime;
        velocity[2] = (velocity[2] + end_velocity[2]) * 0.5;
        if let Some(normal) = state.ground {
            // Slide along the ground plane.
            velocity = clip_velocity(velocity, normal, OVERCLIP);
        }
    }

    let mut time_left = state.frametime;

    // Never turn against the ground plane, and never against the velocity the
    // move started with.
    let mut planes = [[0.0f32; 3]; MAX_CLIP_PLANES];
    let mut numplanes = 0;
    if let Some(normal) = state.ground {
        planes[0] = normal;
        numplanes = 1;
    }
    planes[numplanes] = normalize(velocity);
    numplanes += 1;

    let mut bumpcount = 0;
    while bumpcount < NUMBUMPS {
        // Where this bump is trying to get to.
        let end = [
            state.origin[0] + time_left * velocity[0],
            state.origin[1] + time_left * velocity[1],
            state.origin[2] + time_left * velocity[2],
        ];
        let trace = state.sweep(world, state.origin, end);

        if trace.allsolid {
            // Trapped in another solid: drop the fall, keep the sideways
            // acceleration.
            state.velocity = velocity;
            state.velocity[2] = 0.0;
            return true;
        }

        if trace.fraction > 0.0 {
            state.origin = trace.endpos;
        }

        if trace.fraction == 1.0 {
            break; // made the whole distance
        }

        state.touched.push(trace.entity_num);
        time_left -= time_left * trace.fraction;

        if numplanes >= MAX_CLIP_PLANES {
            // Should not happen; stop rather than drop a plane.
            state.velocity = [0.0; 3];
            return true;
        }

        // Hitting the same plane again means the epsilon left the move a hair
        // inside it; nudge out along the normal and retry.
        let mut nudged = false;
        for plane in planes.iter().take(numplanes) {
            if dot(trace.plane.normal, *plane) > 0.99 {
                velocity = add(velocity, trace.plane.normal);
                nudged = true;
                break;
            }
        }
        if nudged {
            bumpcount += 1;
            continue;
        }
        planes[numplanes] = trace.plane.normal;
        numplanes += 1;

        // Clip the velocity so it runs parallel to every plane it enters.
        for i in 0..numplanes {
            let into = dot(velocity, planes[i]);
            if into >= 0.1 {
                continue; // the move does not push into this plane
            }
            if -into > state.impact_speed {
                state.impact_speed = -into;
            }

            let mut clip = clip_velocity(velocity, planes[i], OVERCLIP);
            let mut end_clip = clip_velocity(end_velocity, planes[i], OVERCLIP);

            for j in 0..numplanes {
                if j == i {
                    continue;
                }
                if dot(clip, planes[j]) >= 0.1 {
                    continue; // does not push into the second plane either
                }
                clip = clip_velocity(clip, planes[j], OVERCLIP);
                end_clip = clip_velocity(end_clip, planes[j], OVERCLIP);

                // Clipping to the second plane may have pushed back into the
                // first; then the move has to run along the crease between
                // them.
                if dot(clip, planes[i]) >= 0.0 {
                    continue;
                }
                let dir = normalize(cross(planes[i], planes[j]));
                clip = scale(dir, dot(dir, velocity));
                end_clip = scale(dir, dot(dir, end_velocity));

                // A third plane the crease still runs into stops the move.
                for (k, plane) in planes.iter().enumerate().take(numplanes) {
                    if k == i || k == j {
                        continue;
                    }
                    if dot(clip, *plane) >= 0.1 {
                        continue;
                    }
                    state.velocity = [0.0; 3];
                    return true;
                }
            }

            velocity = clip;
            end_velocity = end_clip;
            break;
        }

        bumpcount += 1;
    }

    if gravity {
        velocity = end_velocity;
    }
    // While a timer such as knockback is running, the move does not change the
    // velocity it started with.
    if state.pm_time != 0 {
        velocity = primal_velocity;
    }
    state.velocity = velocity;

    bumpcount != 0
}

/// `PM_StepSlideMove`: slide, and if that was blocked, try again from a step
/// height higher so a stair or a lip does not stop the player. The walk code
/// only calls the step form while the player is on the ground; this does not
/// check that, as the C version does not either.
pub fn step_slide_move<W: World + ?Sized>(world: &W, state: &mut MoveState, gravity: bool) {
    let start_origin = state.origin;
    let start_velocity = state.velocity;

    if !slide_move(world, state, gravity) {
        return; // made it on the first try
    }

    // How far down there is to stand on.
    let mut down = start_origin;
    down[2] -= STEPSIZE;
    let trace = state.sweep(world, start_origin, down);

    // Never step up while still rising.
    const UP: [f32; 3] = [0.0, 0.0, 1.0];
    if state.velocity[2] > 0.0
        && (trace.fraction == 1.0 || dot(trace.plane.normal, UP) < MIN_WALK_NORMAL)
    {
        return;
    }

    // Can the player stand a step higher at all?
    let mut up = start_origin;
    up[2] += STEPSIZE;
    let trace = state.sweep(world, start_origin, up);
    if trace.allsolid {
        return; // no room to step into
    }

    let step_size = trace.endpos[2] - start_origin[2];
    state.origin = trace.endpos;
    state.velocity = start_velocity;

    // Move again from up there.
    slide_move(world, state, gravity);

    // Drop back down the amount the step lifted the player.
    let mut down = state.origin;
    down[2] -= step_size;
    let trace = state.sweep(world, state.origin, down);
    if !trace.allsolid {
        state.origin = trace.endpos;
    }
    if trace.fraction < 1.0 {
        state.velocity = clip_velocity(state.velocity, trace.plane.normal, OVERCLIP);
    }

    // Tell the client how high the step was, for the view smoothing.
    let delta = state.origin[2] - start_origin[2];
    if delta > 2.0 {
        let event = if delta < 7.0 {
            StepEvent::Step4
        } else if delta < 11.0 {
            StepEvent::Step8
        } else if delta < 15.0 {
            StepEvent::Step12
        } else {
            StepEvent::Step16
        };
        state.events.push(event);
    }
}

pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub(crate) fn scale(a: [f32; 3], by: f32) -> [f32; 3] {
    [a[0] * by, a[1] * by, a[2] * by]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// `VectorNormalize`: the direction, or the zero vector for a zero length,
/// which is where this differs from the C leaving its destination alone.
pub(crate) fn normalize(a: [f32; 3]) -> [f32; 3] {
    let length = dot(a, a).sqrt();
    if length == 0.0 {
        [0.0; 3]
    } else {
        scale(a, 1.0 / length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collision::tests::arena;

    /// A floor at z = 0, a 16-unit step from x = 64 to 128, and a wall from
    /// x = 128 to 192 that is too tall to step onto.
    fn stairs() -> Collision {
        arena(&[
            ([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]),
            ([64.0, -512.0, 0.0], [128.0, 512.0, 16.0]),
            ([128.0, -512.0, 0.0], [192.0, 512.0, 256.0]),
        ])
    }

    /// The same floor, with the wall at the near edge and no step.
    fn wall() -> Collision {
        arena(&[
            ([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]),
            ([64.0, -512.0, 0.0], [128.0, 512.0, 256.0]),
        ])
    }

    /// Standing on the floor: the box's feet are at z = 0.
    fn standing() -> MoveState {
        let mut state = MoveState::new([0.0, 0.0, 24.125]);
        state.frametime = 0.1;
        state
    }

    #[test]
    fn clipping_leaves_the_velocity_one_overclip_inside_the_plane() {
        let out = clip_velocity([100.0, 0.0, -50.0], [0.0, 0.0, 1.0], OVERCLIP);
        assert_eq!(out[0], 100.0);
        assert_eq!(out[1], 0.0);
        assert!((out[2] - 0.05).abs() < 1e-4, "{:?}", out);
    }

    #[test]
    fn a_move_with_nothing_in_the_way_arrives() {
        let world = arena(&[]);
        let mut state = standing();
        state.origin = [0.0, 0.0, 100.0];
        state.velocity = [200.0, -72.0, 0.0];

        assert!(!slide_move(&world, &mut state, false));
        assert!((state.origin[0] - 20.0).abs() < 1e-4);
        assert!((state.origin[1] + 7.2).abs() < 1e-4);
        assert_eq!(state.origin[2], 100.0);
        assert_eq!(state.touched.len(), 0);
    }

    #[test]
    fn a_move_into_a_wall_stops_at_the_surface() {
        let world = wall();
        let mut state = standing();
        state.velocity = [4000.0, 0.0, 0.0];

        assert!(slide_move(&world, &mut state, false));
        // The wall is at 64, the box reaches 15 forward, and the trace stops a
        // SURFACE_CLIP_EPSILON short.
        assert!((state.origin[0] - 48.875).abs() < 1e-3, "{:?}", state.origin);
        // A head-on hit stops dead: the wall and the incoming direction are
        // opposite planes, and the crease between them is nothing.
        assert_eq!(state.velocity[0], 0.0);
        assert!(state.impact_speed > 3900.0);
        assert_eq!(state.touched.len(), 1);
    }

    #[test]
    fn a_diagonal_move_slides_along_the_wall() {
        let world = wall();
        let mut state = standing();
        state.velocity = [4000.0, 4000.0, 0.0];

        assert!(slide_move(&world, &mut state, false));
        // The overclip leaves the box a fraction of a unit off the wall.
        assert!(state.origin[0] > 48.0 && state.origin[0] < 49.0, "{:?}", state.origin);
        // Whatever time the wall took is still spent moving along it.
        assert!(state.origin[1] > 200.0, "{:?}", state.origin);
        assert!((state.velocity[1] - 4000.0).abs() < 1e-3);
    }

    #[test]
    fn gravity_lands_the_player_on_the_floor() {
        let world = wall();
        let mut state = MoveState::new([0.0, 0.0, 100.0]);
        state.frametime = 0.5;

        assert!(slide_move(&world, &mut state, true));
        assert!((state.origin[2] - 24.125).abs() < 1e-3, "{:?}", state.origin);
        // The velocity that goes into the floor is removed.
        assert!(state.velocity[2].abs() < 1.0, "{:?}", state.velocity);
        assert!(state.impact_speed > 100.0);
    }

    #[test]
    fn a_downward_trace_with_no_floor_falls_freely() {
        let world = arena(&[]);
        let mut state = MoveState::new([0.0, 0.0, 100.0]);
        state.frametime = 0.1;

        assert!(!slide_move(&world, &mut state, true));
        // The trace ran at the average velocity; the frame ends at the low one.
        assert!((state.origin[2] - 96.0).abs() < 1e-3);
        assert!((state.velocity[2] + 80.0).abs() < 1e-3, "{:?}", state.velocity);
    }

    #[test]
    fn a_step_slide_climbs_a_sixteen_unit_step() {
        let world = stairs();
        let mut state = standing();
        state.velocity = [4000.0, 0.0, 0.0];

        step_slide_move(&world, &mut state, false);
        // Onto the step (top at 16), stopped by the wall behind it at 128.
        assert!((state.origin[2] - 40.125).abs() < 1e-2, "{:?}", state.origin);
        assert!((state.origin[0] - 112.875).abs() < 1e-2, "{:?}", state.origin);
        assert_eq!(state.events, vec![StepEvent::Step16]);
    }

    #[test]
    fn a_step_slide_does_not_climb_a_wall() {
        let world = wall();
        let mut state = standing();
        state.velocity = [4000.0, 0.0, 0.0];

        step_slide_move(&world, &mut state, false);
        assert!((state.origin[2] - 24.125).abs() < 1e-3, "{:?}", state.origin);
        assert!((state.origin[0] - 48.875).abs() < 1e-3);
        assert!(state.events.is_empty());
    }

    #[test]
    fn a_player_still_rising_does_not_step_up() {
        let world = wall();
        // Mid-air above the floor, so the step-down trace finds nothing.
        let mut state = MoveState::new([0.0, 0.0, 60.0]);
        state.frametime = 0.1;
        state.velocity = [4000.0, 0.0, 200.0];

        step_slide_move(&world, &mut state, false);
        // The slide was blocked by the wall and the jump kept rising, but no
        // step was climbed on the way.
        assert!(state.origin[2] > 60.0, "{:?}", state.origin);
        assert!(state.events.is_empty());
        assert!((state.origin[1]).abs() < 1e-6);
    }

    #[test]
    fn a_player_inside_solid_loses_only_its_fall() {
        let world = wall();
        let mut state = MoveState::new([0.0, 0.0, -32.0]);
        state.velocity = [100.0, 0.0, -300.0];
        state.frametime = 0.1;

        assert!(slide_move(&world, &mut state, true));
        assert_eq!(state.velocity[2], 0.0);
    }

    #[test]
    fn a_knockback_timer_restores_the_velocity_it_started_with() {
        let world = wall();
        let mut state = standing();
        state.velocity = [4000.0, 0.0, 0.0];
        state.pm_time = 100;

        assert!(slide_move(&world, &mut state, false));
        assert_eq!(state.velocity, [4000.0, 0.0, 0.0]);
        // The position still moved to the wall, but the knockback keeps its
        // speed so the next frame repeats the push.
        assert!((state.origin[0] - 48.875).abs() < 1e-3);
    }

    #[test]
    fn the_mask_a_player_moves_with_is_the_playersolid_one() {
        assert_eq!(MASK_PLAYERSOLID, 0x0201_0001);
        assert_eq!(PLAYER_MINS[2], -24.0);
        assert_eq!(PLAYER_MAXS[2], 32.0);
    }
}
