//! Practice targets: the map's `target_position` markers, stood up as
//! something a bullet can stop on.
//!
//! The stock game has no shootable scene objects, so these are the client's
//! own: each marker becomes a body-sized post with health. A shot that reaches
//! one takes a third of its health; the third shot knocks it down for good, so
//! there is something to aim at on a map with no enemies in it yet.

use crate::entities;
use crate::items;
use crate::vk::Vertex3D;

/// The post's box, relative to the marker: a player-sized body on the floor.
pub const TARGET_MINS: [f32; 3] = [-16.0, -16.0, 0.0];
pub const TARGET_MAXS: [f32; 3] = [16.0, 16.0, 64.0];

/// How far a marker sits above the floor: player-origin height, so the post's
/// base lands on the ground under it.
pub const TARGET_BASE: f32 = 24.0;

/// Health of a fresh target.
pub const TARGET_HEALTH: i32 = 100;
/// What one round takes off, so three rounds put it down.
pub const TARGET_HIT_DAMAGE: i32 = 34;
/// How long a post lies down before it stands back up with full health.
pub const TARGET_RECOVERY_MS: u32 = 6_000;

/// One post on the map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub origin: [f32; 3],
    pub health: i32,
    /// Where it fell, once it has: the horizontal direction it was knocked
    /// towards. A post that has fallen is still drawn, lying on the floor.
    pub fell: Option<[f32; 3]>,
    /// When it went down, so it can be known when to get back up. Zero while
    /// it is standing.
    pub fallen_at: u32,
}

impl Target {
    pub fn alive(&self) -> bool {
        self.health > 0 && self.fell.is_none()
    }

    pub fn fallen(&self) -> bool {
        self.fell.is_some()
    }

    /// The colour the post is drawn in: bright while healthy, duller as it is
    /// shot up, and bare wood once it is down.
    pub fn color(&self) -> [u8; 4] {
        if self.fallen() {
            return [110, 92, 78, 255];
        }
        if self.health > TARGET_HEALTH / 2 {
            [235, 120, 55, 255]
        } else {
            [150, 70, 40, 255]
        }
    }

    /// The box it is drawn as. Upright it is a body; down it lies along the
    /// direction it was knocked, which keeps it visible on the floor.
    fn box_at(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        // The markers sit at player height, so the post stands on the floor
        // below them rather than floating where the marker is.
        let mut foot = self.origin;
        foot[2] -= TARGET_BASE;
        match self.fell {
            None => (foot, TARGET_MINS, TARGET_MAXS),
            Some(fall) => {
                // Snap the fall to the nearest axis: the post lies along the
                // line of fire, so it reads from whichever side you shot it.
                let axis = if fall[0].abs() >= fall[1].abs() { 0 } else { 1 };
                let sign = if fall[axis] >= 0.0 { 1.0 } else { -1.0 };
                let mut centre = foot;
                centre[axis] += sign * TARGET_MAXS[2] / 2.0;
                centre[2] += TARGET_MAXS[0].abs();
                let mut mins = [TARGET_MINS[0].abs(), TARGET_MINS[1].abs(), 0.0];
                let mut maxs = mins;
                mins[axis] = -TARGET_MAXS[2] / 2.0;
                maxs[axis] = TARGET_MAXS[2] / 2.0;
                mins[2] = -TARGET_MAXS[0].abs();
                maxs[2] = TARGET_MAXS[0].abs();
                (centre, mins, maxs)
            }
        }
    }
}

/// The posts on the map, and what shooting them does.
pub struct TargetField {
    targets: Vec<Target>,
}

impl TargetField {
    pub fn new(origins: &[[f32; 3]]) -> Self {
        TargetField {
            targets: origins
                .iter()
                .map(|origin| Target {
                    origin: *origin,
                    health: TARGET_HEALTH,
                    fell: None,
                    fallen_at: 0,
                })
                .collect(),
        }
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    pub fn alive(&self) -> usize {
        self.targets.iter().filter(|target| target.alive()).count()
    }

    /// Every post, standing or lying where it fell, as triangles.
    pub fn vertices(&self) -> Vec<Vertex3D> {
        let mut vertices = Vec::new();
        for target in &self.targets {
            let (origin, mins, maxs) = target.box_at();
            vertices.extend(items::cuboid(origin, mins, maxs, target.color()));
        }
        vertices
    }

    /// Knocks a post over towards `at`, which is where it was shot from.
    fn knock_down(&mut self, index: usize, at: [f32; 3], now: u32) {
        let target = &mut self.targets[index];
        let mut fall = [target.origin[0] - at[0], target.origin[1] - at[1], 0.0];
        if fall[0] == 0.0 && fall[1] == 0.0 {
            fall = [1.0, 0.0, 0.0];
        }
        target.health = 0;
        target.fell = Some(fall);
        target.fallen_at = now.max(1);
    }

    /// Stands every post back up whose time on the floor is done, at full
    /// health, so a map never runs out of things to shoot. Returns how many
    /// got up.
    pub fn recover(&mut self, now: u32) -> usize {
        let mut up = 0;
        for target in &mut self.targets {
            if target.fallen_at == 0 {
                continue;
            }
            if now.wrapping_sub(target.fallen_at) < TARGET_RECOVERY_MS {
                continue;
            }
            target.health = TARGET_HEALTH;
            target.fell = None;
            target.fallen_at = 0;
            up += 1;
        }
        up
    }

    /// Fires a shot segment at the posts. Returns the nearest hit's point and
    /// whether that hit put the post down.
    pub fn shoot(
        &mut self,
        from: [f32; 3],
        to: [f32; 3],
        damage: i32,
        now: u32,
    ) -> Option<([f32; 3], bool)> {
        let mut best: Option<(f32, usize, [f32; 3])> = None;
        for (index, target) in self.targets.iter().enumerate() {
            if !target.alive() {
                continue;
            }
            let Some(fraction) = hit_fraction(target.origin, from, to) else {
                continue;
            };
            if best.map(|(at, _, _)| fraction < at).unwrap_or(true) {
                let at = [
                    from[0] + (to[0] - from[0]) * fraction,
                    from[1] + (to[1] - from[1]) * fraction,
                    from[2] + (to[2] - from[2]) * fraction,
                ];
                best = Some((fraction, index, at));
            }
        }

        let (fraction, index, at) = best?;
        self.targets[index].health -= damage;
        let down = self.targets[index].health <= 0;
        if down {
            let from = [
                from[0] + (to[0] - from[0]) * fraction,
                from[1] + (to[1] - from[1]) * fraction,
                from[2] + (to[2] - from[2]) * fraction,
            ];
            self.knock_down(index, from, now);
        }
        Some((at, down))
    }

    /// A rocket's blast: every standing post within `radius` of `at` loses
    /// `damage`, which is what makes a rocket worth aiming at the floor near
    /// a post. Returns how many posts went down.
    /// How many posts are on the floor right now. The client diffs this around
    /// a hit to keep a tally of how many it has put down.
    pub fn downed(&self) -> usize {
        self.targets.iter().filter(|target| !target.alive()).count()
    }

    pub fn blast(&mut self, at: [f32; 3], radius: f32, damage: i32, now: u32) -> usize {
        let mut down = 0;
        for index in 0..self.targets.len() {
            let target = self.targets[index];
            if !target.alive() {
                continue;
            }
            let centre = [
                target.origin[0],
                target.origin[1],
                target.origin[2] + TARGET_MAXS[2] / 2.0,
            ];
            let delta = [
                centre[0] - at[0],
                centre[1] - at[1],
                centre[2] - at[2],
            ];
            let distance =
                (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
            if distance > radius {
                continue;
            }
            self.targets[index].health -= damage;
            if self.targets[index].health <= 0 {
                let mut fall = [
                    target.origin[0] - at[0],
                    target.origin[1] - at[1],
                    0.0,
                ];
                if fall[0] == 0.0 && fall[1] == 0.0 {
                    fall = [1.0, 0.0, 0.0];
                }
                self.targets[index].health = 0;
                self.targets[index].fell = Some(fall);
                self.targets[index].fallen_at = now.max(1);
                down += 1;
            }
        }
        down
    }
}

/// Where a segment first enters the post's box, as a fraction of the segment,
/// or `None` when it misses. A slab test, with the box axis-aligned.
fn hit_fraction(origin: [f32; 3], from: [f32; 3], to: [f32; 3]) -> Option<f32> {
    let min = [
        origin[0] + TARGET_MINS[0],
        origin[1] + TARGET_MINS[1],
        origin[2] + TARGET_MINS[2],
    ];
    let max = [
        origin[0] + TARGET_MAXS[0],
        origin[1] + TARGET_MAXS[1],
        origin[2] + TARGET_MAXS[2],
    ];
    let mut enter = 0.0f32;
    let mut exit = 1.0f32;
    for axis in 0..3 {
        let start = from[axis];
        let delta = to[axis] - start;
        if delta.abs() < f32::EPSILON {
            if start < min[axis] || start > max[axis] {
                return None;
            }
            continue;
        }
        let mut first = (min[axis] - start) / delta;
        let mut second = (max[axis] - start) / delta;
        if first > second {
            std::mem::swap(&mut first, &mut second);
        }
        enter = enter.max(first);
        exit = exit.min(second);
        if enter > exit {
            return None;
        }
    }
    (enter <= 1.0 && exit >= 0.0).then_some(enter.max(0.0))
}

/// Every `target_position` the map's entity lump places. This is the marker
/// q3map writes for a bot goal, reused as a place to put a post.
pub fn positions(bytes: &[u8]) -> Vec<[f32; 3]> {
    entities::parse(bytes)
        .iter()
        .filter(|entity| entity.classname.as_deref() == Some("target_position"))
        .filter_map(|entity| entity.origin())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSTS: &[u8] = b"{ \"classname\" \"worldspawn\" }\n\
        { \"classname\" \"target_position\" \"origin\" \"256 0 24\" }\n\
        { \"classname\" \"target_position\" \"origin\" \"-256 0 24\" }\n\
        { \"classname\" \"info_player_deathmatch\" \"origin\" \"0 0 24\" }";

    #[test]
    fn only_the_maps_target_positions_become_posts() {
        let positions = positions(POSTS);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], [256.0, 0.0, 24.0]);
    }

    #[test]
    fn a_post_on_the_floor_is_counted_as_down_and_stops_being_counted_when_it_gets_up() {
        let mut field = TargetField::new(&positions(POSTS));
        assert_eq!(field.downed(), 0);
        field.knock_down(0, [256.0, 0.0, 24.0], 100);
        assert_eq!(field.downed(), 1);
        field.recover(100 + TARGET_RECOVERY_MS);
        assert_eq!(field.downed(), 0, "a recovered post is not down");
    }

    #[test]
    fn a_shot_into_a_post_hits_it_and_knocks_it_down_in_three() {
        let mut field = TargetField::new(&positions(POSTS));
        assert_eq!(field.alive(), 2);

        let from = [0.0, 0.0, 40.0];
        let to = [600.0, 0.0, 40.0];
        let (at, destroyed) = field.shoot(from, to, TARGET_HIT_DAMAGE, 0).expect("the post is in the way");
        assert!(destroyed == false);
        assert!((at[0] - 240.0).abs() < 0.001, "{at:?}");
        assert_eq!(field.alive(), 2, "one hit does not kill it");

        field.shoot(from, to, TARGET_HIT_DAMAGE, 0);
        let (_, destroyed) = field.shoot(from, to, TARGET_HIT_DAMAGE, 0).expect("still standing");
        assert!(destroyed, "the third hit puts it down");
        assert_eq!(field.alive(), 1);

        // A downed post is no longer a target, but it is still there, lying
        // on the floor along the line it was shot from.
        assert!(
            field.shoot(from, to, TARGET_HIT_DAMAGE, 0).is_none(),
            "the far post is behind this one"
        );
        assert!(field.targets()[0].fallen());
        let fallen = &field.vertices()[..36];
        let top = fallen
            .iter()
            .map(|vertex| vertex.pos[2])
            .fold(f32::MIN, f32::max);
        let floor = field.targets()[0].origin[2] - TARGET_BASE;
        assert!(
            top <= floor + TARGET_MAXS[0].abs() * 2.0 + 0.001,
            "lies flat, top at {top}"
        );
        assert!(
            (top - (floor + TARGET_MAXS[0].abs() * 2.0)).abs() < 0.001,
            "and resting on it"
        );
        assert_eq!(field.vertices().len(), 72, "one lying, one standing");
    }

    #[test]
    fn one_rail_slug_is_enough_and_a_pellet_is_not() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 12, 0);
        assert_eq!(field.alive(), 1, "a pellet does not put a post down");
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 100, 0);
        assert_eq!(field.alive(), 0, "a slug does");
    }

    #[test]
    fn a_rocket_near_a_post_takes_it_down() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        // Just outside its box, so the blast is what does the work.
        assert_eq!(field.blast([0.0, 60.0, 32.0], 120.0, 100, 0), 1);
        assert_eq!(field.alive(), 0);

        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        assert_eq!(field.blast([0.0, 400.0, 32.0], 120.0, 100, 0), 0, "out of range");
        assert_eq!(field.alive(), 1);
    }

    #[test]
    fn a_shot_that_misses_touches_nothing() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        // Upwards past the side of the post, and across in front of it.
        assert!(
            field
                .shoot([0.0, 100.0, 40.0], [0.0, 100.0, 800.0], 34, 0)
                .is_none()
        );
        assert!(
            field
                .shoot([0.0, -100.0, 40.0], [600.0, -100.0, 40.0], 34, 0)
                .is_none()
        );
        assert_eq!(field.alive(), 1);
    }

    #[test]
    fn a_post_dims_as_it_is_shot() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        let fresh = field.targets()[0].color();
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 34, 0);
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 34, 0);
        let hurt = field.targets()[0].color();
        assert_ne!(fresh, hurt, "a hurt post reads differently");
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 34, 0);
        assert_ne!(hurt, field.targets()[0].color(), "a fallen post is bare");
        assert_eq!(field.vertices().len(), 36, "and still drawn");
    }

    #[test]
    fn a_post_gets_back_up_after_its_time_on_the_floor() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        field.shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 200, 500);
        assert_eq!(field.alive(), 0, "one slug is enough");

        // A moment before its time is up it is still down.
        assert_eq!(field.recover(500 + TARGET_RECOVERY_MS - 1), 0);
        assert_eq!(field.alive(), 0);

        assert_eq!(field.recover(500 + TARGET_RECOVERY_MS), 1);
        assert_eq!(field.alive(), 1);
        assert_eq!(field.targets()[0].health, TARGET_HEALTH);
        assert!(!field.targets()[0].fallen());
        // Back on its feet, it can be shot again.
        assert!(field
            .shoot([0.0, 0.0, 40.0], [100.0, 0.0, 40.0], 34, 20_000)
            .is_some());
    }
}
