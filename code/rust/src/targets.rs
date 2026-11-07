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
/// How much longer it stays down each further time it is put down in one life,
/// and the longest it will ever lie there.
pub const TARGET_RECOVERY_STEP_MS: u32 = 2_000;
pub const TARGET_RECOVERY_MAX_MS: u32 = 12_000;

/// How long a post that has been put down `downs` times in one life stays on
/// the floor: the first time is the plain recovery, each one after that adds a
/// step, up to the cap.
fn recovery_for(downs: u32) -> u32 {
    let extra = downs
        .saturating_sub(1)
        .saturating_mul(TARGET_RECOVERY_STEP_MS)
        .min(TARGET_RECOVERY_MAX_MS - TARGET_RECOVERY_MS);
    TARGET_RECOVERY_MS + extra
}

/// `G_RadiusDamage`: a blast lands for all of `damage` at its centre and fades
/// to nothing at the edge of its radius. Without this a rocket that burst a
/// room's width away would take a post down exactly as surely as one at its
/// feet, and near enough was the same as on target.
fn splash_damage(damage: i32, distance: f32, radius: f32) -> i32 {
    if radius <= 0.0 || distance >= radius {
        return 0;
    }
    (damage as f32 * (1.0 - distance / radius)) as i32
}

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
    /// How many times this post has been put down since the player last came
    /// back from the dead, so a fight gets harder the longer it runs.
    pub downs: u32,
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
                    downs: 0,
                })
                .collect(),
        }
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// How long the posts knocked down at `when` will lie there, so the line
    /// that says a post went down can say how long *that* post's wait is
    /// rather than the longest wait of anything already on the floor. Zero
    /// when nothing went down at that moment.
    pub fn wait_at(&self, when: u32) -> u32 {
        let when = when.max(1);
        self.targets
            .iter()
            .filter(|target| target.fallen_at == when)
            .map(|target| recovery_for(target.downs))
            .max()
            .unwrap_or(0)
    }

    /// Wipes how many times each post has been put down, for the player coming
    /// back from the dead: a fresh life starts the fight over at the plain
    /// recovery time.
    pub fn forget_down_counts(&mut self) {
        for target in &mut self.targets {
            target.downs = 0;
        }
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
        target.downs = target.downs.saturating_add(1);
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
            if now.wrapping_sub(target.fallen_at) < recovery_for(target.downs) {
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
            // The post is drawn standing on the floor under its marker, so
            // that is the box a shot has to reach: not the marker's height.
            let (foot, mins, maxs) = target.box_at();
            let Some(fraction) = hit_fraction(foot, mins, maxs, from, to) else {
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

        let (_, index, at) = best?;
        self.targets[index].health -= damage;
        let down = self.targets[index].health <= 0;
        if down {
            self.knock_down(index, at, now);
        }
        Some((at, down))
    }

    /// How many posts are on the floor right now. The client diffs this around
    /// a hit to keep a tally of how many it has put down.
    pub fn downed(&self) -> usize {
        self.targets.iter().filter(|target| !target.alive()).count()
    }

    /// Splashes damage over every post within `radius` of `at`, by distance
    /// only: like the player's own splash this ignores walls, so a rocket that
    /// bursts on the far side of one still catches whoever is hugging it.
    ///
    /// How hard it lands fades with distance, though, the way the player's own
    /// splash does: all of `damage` at the centre of the blast, nothing at the
    /// edge of the radius, so where a rocket bursts is worth aiming.
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
            let hurt = splash_damage(damage, distance, radius);
            if hurt <= 0 {
                continue;
            }
            self.targets[index].health -= hurt;
            if self.targets[index].health <= 0 {
                // A blast knocks a post down the same way a bullet does, so it
                // counts towards the longer wait the next fall buys.
                self.knock_down(index, at, now);
                down += 1;
            }
        }
        down
    }
}

/// Where a segment first enters the post's box, as a fraction of the segment,
/// or `None` when it misses. A slab test, with the box axis-aligned.
fn hit_fraction(
    origin: [f32; 3],
    mins: [f32; 3],
    maxs: [f32; 3],
    from: [f32; 3],
    to: [f32; 3],
) -> Option<f32> {
    let min = [
        origin[0] + mins[0],
        origin[1] + mins[1],
        origin[2] + mins[2],
    ];
    let max = [
        origin[0] + maxs[0],
        origin[1] + maxs[1],
        origin[2] + maxs[2],
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
    fn a_shot_hits_the_post_where_it_is_drawn() {
        // The marker sits a player's height off the floor and the post stands
        // on the floor under it, so its legs are as shootable as its head --
        // and the empty air above its head is not.
        let mut field = TargetField::new(&[[0.0, 0.0, 24.0]]);
        assert!(
            field
                .shoot([-100.0, 0.0, 10.0], [100.0, 0.0, 10.0], 10, 0)
                .is_some(),
            "the drawn body's legs are part of the post"
        );
        assert!(
            field
                .shoot([-100.0, 0.0, 80.0], [100.0, 0.0, 80.0], 10, 0)
                .is_none(),
            "nothing is there above its head"
        );
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
        // Halfway out, the blast lands for half: hurt but still standing.
        assert_eq!(field.blast([0.0, 60.0, 32.0], 120.0, 100, 0), 0);
        assert_eq!(field.alive(), 1);
        assert_eq!(field.targets()[0].health, 50, "the blast fades with distance");

        // At the post's feet the whole of it lands.
        assert_eq!(field.blast([0.0, 0.0, 32.0], 120.0, 100, 0), 1);
        assert_eq!(field.alive(), 0);

        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        assert_eq!(field.blast([0.0, 400.0, 32.0], 120.0, 100, 0), 0, "out of range");
        assert_eq!(field.alive(), 1);
    }

    #[test]
    fn a_blast_fades_from_the_centre_to_nothing_at_the_edge() {
        assert_eq!(splash_damage(100, 0.0, 120.0), 100, "everything at the centre");
        assert_eq!(splash_damage(100, 60.0, 120.0), 50, "half of it halfway out");
        assert_eq!(splash_damage(100, 120.0, 120.0), 0, "the edge is nothing");
        assert_eq!(splash_damage(100, 200.0, 120.0), 0, "and past it nothing");
        assert_eq!(splash_damage(100, 10.0, 0.0), 0, "a blast with no width");
    }

    #[test]
    fn a_post_a_blast_puts_down_counts_towards_its_next_fall() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        // A blast at its feet lands for everything, so one puts it down.
        let at_post = [0.0, 0.0, 32.0];
        assert_eq!(field.blast(at_post, 120.0, 100, 500), 1);
        assert_eq!(field.wait_at(500), TARGET_RECOVERY_MS);
        field.recover(500 + TARGET_RECOVERY_MS);

        // The second fall takes it down longer, whether a bullet or a blast
        // did it.
        assert_eq!(field.blast(at_post, 120.0, 100, 9_000), 1);
        assert_eq!(
            field.wait_at(9_000),
            TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS
        );
        assert_eq!(
            field.recover(9_000 + TARGET_RECOVERY_MS),
            0,
            "six seconds is not enough now"
        );
        assert_eq!(
            field.recover(9_000 + TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS),
            1
        );
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
    fn a_down_post_can_say_how_long_it_will_lie_there() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0], [400.0, 0.0, 0.0]]);
        assert_eq!(field.wait_at(500), 0, "nothing is down yet");

        let shot = ([0.0, 0.0, 40.0], [100.0, 0.0, 40.0]);
        field.shoot(shot.0, shot.1, 200, 500);
        assert_eq!(field.wait_at(500), TARGET_RECOVERY_MS);

        // A post put down for the second time sets the wait for the pair.
        field.recover(500 + TARGET_RECOVERY_MS);
        field.shoot(shot.0, shot.1, 200, 10_000);
        assert_eq!(
            field.wait_at(10_000),
            TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS
        );

        // Once they are all up there is no wait to speak of.
        field.recover(10_000 + TARGET_RECOVERY_MAX_MS);
        assert_eq!(field.wait_at(10_000), 0);
    }

    #[test]
    fn a_post_downed_now_says_its_own_wait_and_not_a_neighbours() {
        let mut field = TargetField::new(&[[0.0, 0.0, 0.0], [400.0, 0.0, 0.0]]);
        let near = ([0.0, 0.0, 40.0], [100.0, 0.0, 40.0]);
        let far = ([300.0, 0.0, 40.0], [500.0, 0.0, 40.0]);

        // The near post is put down a second time, so it is on the floor for
        // eight seconds.
        field.shoot(near.0, near.1, 200, 500);
        field.recover(500 + TARGET_RECOVERY_MS);
        field.shoot(near.0, near.1, 200, 7_000);
        assert_eq!(
            field.wait_at(7_000),
            TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS
        );

        // The far post's first fall is six seconds, whatever the near one is
        // serving.
        field.shoot(far.0, far.1, 200, 9_000);
        assert_eq!(field.wait_at(9_000), TARGET_RECOVERY_MS);
        assert_eq!(
            field.wait_at(7_000),
            TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS,
            "the near post still says its own wait"
        );
    }

    #[test]
    fn a_post_put_down_again_stays_down_longer_and_that_has_a_limit() {
        assert_eq!(recovery_for(0), TARGET_RECOVERY_MS);
        assert_eq!(recovery_for(1), TARGET_RECOVERY_MS);
        assert_eq!(recovery_for(2), TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS);
        assert_eq!(recovery_for(3), TARGET_RECOVERY_MS + 2 * TARGET_RECOVERY_STEP_MS);
        assert_eq!(recovery_for(99), TARGET_RECOVERY_MAX_MS, "it never sleeps forever");

        let mut field = TargetField::new(&[[0.0, 0.0, 0.0]]);
        let shot = ([0.0, 0.0, 40.0], [100.0, 0.0, 40.0]);
        field.shoot(shot.0, shot.1, 200, 500);
        // First time down is the plain six seconds.
        assert_eq!(field.recover(500 + TARGET_RECOVERY_MS - 1), 0);
        assert_eq!(field.recover(500 + TARGET_RECOVERY_MS), 1);

        // Put down a second time it lies there longer.
        field.shoot(shot.0, shot.1, 200, 10_000);
        assert_eq!(field.recover(10_000 + TARGET_RECOVERY_MS), 0, "six seconds is not enough now");
        assert_eq!(
            field.recover(10_000 + TARGET_RECOVERY_MS + TARGET_RECOVERY_STEP_MS),
            1
        );

        // A fresh life starts the count over.
        field.shoot(shot.0, shot.1, 200, 20_000);
        field.forget_down_counts();
        field.recover(20_000 + TARGET_RECOVERY_MS);
        field.shoot(shot.0, shot.1, 200, 30_000);
        assert_eq!(field.recover(30_000 + TARGET_RECOVERY_MS), 1, "back to six seconds");
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
