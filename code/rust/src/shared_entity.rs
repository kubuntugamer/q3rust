//! The entity memory the module and the engine share: `entityState_t` followed
//! by `entityShared_t`. That pair is `sharedEntity_t` in `g_public.h`, and it is
//! the start of the game's own `gentity_t` — which is why the game writes
//! `ent->s` and `ent->r` directly and the engine can read them by offset.
//!
//! The game allocates `level.gentities` and reports the array, its stride and
//! its length through `G_LOCATE_GAME_DATA`; after that the engine addresses
//! entities by offset into that array. Every constant here comes from the field
//! order the headers fix with "DO NOT MODIFY ANYTHING ABOVE THIS, THE SERVER
//! EXPECTS THE FIELDS IN THAT ORDER". Both structs hold only four-byte members,
//! so each offset is the sum of the fields before it. `trajectory_t` is the one
//! record that is easy to get wrong: it starts with `trType`, so it is 36 bytes
//! rather than the 32 its two vectors alone suggest. That makes
//! `entityState_t` 208 bytes. `entityShared_t` is 308, because the header gives
//! it its own copy of `entityState_t` before `linked`: every `r.` field is one
//! `entityState_t` further in than the shared entity's start. The game's own
//! `gentity_t` fields begin at 516.
//!
//! The two derived values are [`solid_value`] and [`absolute_box`], which are
//! the parts of `SV_LinkEntity` a box can be computed from without a world.

/// `sizeof( entityState_t )`: where `entityShared_t` starts. Two 36-byte
/// trajectories and the rest of the four-byte fields.
pub const ENTITY_STATE_SIZE: usize = 208;
/// `sizeof( entityShared_t )`: no trajectories in this one.
pub const ENTITY_SHARED_SIZE: usize = 100;
/// `sizeof( entityShared_t )`: it repeats the 208-byte state before `linked`.
pub const ENTITY_SHARED_T_SIZE: usize = ENTITY_STATE_SIZE + ENTITY_SHARED_SIZE;
/// `sizeof( sharedEntity_t )`: where the game's own `gentity_t` fields start.
pub const SHARED_ENTITY_SIZE: usize = ENTITY_STATE_SIZE + ENTITY_SHARED_T_SIZE;

// entityState_t, in `q_shared.h` order. `pos` and `apos` are `trajectory_t`:
// `trType`, `trTime`, `trDuration`, then `trBase` and `trDelta` as `vec3_t`,
// which is 36 bytes each. `pos` is at 12, so its base is at 24.
pub const S_NUMBER: usize = 0;
pub const S_ETYPE: usize = 4;
pub const S_EFLAGS: usize = 8;
pub const S_POS: usize = 12;
/// `s.pos.trBase`: the trajectory's start point, which `G_SetOrigin` writes.
pub const S_POS_TRBASE: usize = S_POS + 12;
pub const S_APOS: usize = 48;
pub const S_TIME: usize = 84;
pub const S_TIME2: usize = 88;
pub const S_ORIGIN: usize = 92;
pub const S_ORIGIN2: usize = 104;
pub const S_ANGLES: usize = 116;
pub const S_ANGLES2: usize = 128;
pub const S_OTHER_ENTITY_NUM: usize = 140;
pub const S_OTHER_ENTITY_NUM2: usize = 144;
pub const S_GROUND_ENTITY_NUM: usize = 148;
pub const S_CONSTANT_LIGHT: usize = 152;
pub const S_LOOP_SOUND: usize = 156;
pub const S_MODELINDEX: usize = 160;
pub const S_MODELINDEX2: usize = 164;
pub const S_CLIENT_NUM: usize = 168;
pub const S_FRAME: usize = 172;
pub const S_SOLID: usize = 176;
pub const S_EVENT: usize = 180;
pub const S_EVENT_PARM: usize = 184;
pub const S_POWERUPS: usize = 188;
pub const S_WEAPON: usize = 192;
pub const S_LEGS_ANIM: usize = 196;
pub const S_TORSO_ANIM: usize = 200;
pub const S_GENERIC1: usize = 204;

// entityShared_t, which begins at ENTITY_STATE_SIZE. Its own first member is
// another `entityState_t`, so the fields below start one more ENTITY_STATE_SIZE
// in: `r.linked` is at 208 + 208.
pub const R_LINKED: usize = ENTITY_STATE_SIZE + ENTITY_STATE_SIZE;
pub const R_LINKCOUNT: usize = R_LINKED + 4;
pub const R_SVFLAGS: usize = R_LINKED + 8;
pub const R_SINGLE_CLIENT: usize = R_LINKED + 12;
pub const R_BMODEL: usize = R_LINKED + 16;
pub const R_MINS: usize = R_LINKED + 20;
pub const R_MAXS: usize = R_LINKED + 32;
pub const R_CONTENTS: usize = R_LINKED + 44;
pub const R_ABSMIN: usize = R_LINKED + 48;
pub const R_ABSMAX: usize = R_LINKED + 60;
pub const R_CURRENT_ORIGIN: usize = R_LINKED + 72;
pub const R_CURRENT_ANGLES: usize = R_LINKED + 84;
pub const R_OWNERNUM: usize = R_LINKED + 96;

/// `SOLID_BMODEL` from `q_shared.h`: the `s.solid` value that tells a client
/// the entity's shape comes from a brush model instead of a packed box.
pub const SOLID_BMODEL: i32 = 0x00ff_ffff;
/// `CONTENTS_SOLID`: an eye is never valid in a solid.
pub const CONTENTS_SOLID: i32 = 1;
/// `CONTENTS_BODY`: a player or a corpse.
pub const CONTENTS_BODY: i32 = 0x0200_0000;

/// The `s.solid` value `SV_LinkEntity` encodes from an entity's bounds: the
/// brush-model marker, a box packed as `(top << 16) | (bottom << 8) | width`,
/// or zero for an entity that does not collide. Each part is clamped to one
/// byte, and the x width is assumed symmetrical about the origin.
pub fn solid_value(bmodel: bool, contents: i32, mins: [f32; 3], maxs: [f32; 3]) -> i32 {
    if bmodel {
        // A solid box will never create this value.
        return SOLID_BMODEL;
    }
    if contents & (CONTENTS_SOLID | CONTENTS_BODY) == 0 {
        return 0;
    }
    let width = (maxs[0] as i32).clamp(1, 255);
    let bottom = (-mins[2] as i32).clamp(1, 255);
    let top = ((maxs[2] + 32.0) as i32).clamp(1, 255);
    (top << 16) | (bottom << 8) | width
}

/// `RadiusFromBounds` from `q_shared.c`: the distance from the origin to the
/// far corner of a box, using the longer reach on each axis.
pub fn radius_from_bounds(mins: [f32; 3], maxs: [f32; 3]) -> f32 {
    let mut total = 0.0f32;
    for axis in 0..3 {
        let largest = mins[axis].abs().max(maxs[axis].abs());
        total += largest * largest;
    }
    total.sqrt()
}

/// The absolute box `SV_LinkEntity` derives from an entity's current origin,
/// angles and bounds: the bounds moved to the origin, or a cube of the bounding
/// radius when a brush model is rotated, then spread by one unit on every side
/// because movement stops an epsilon short of an edge.
pub fn absolute_box(
    bmodel: bool,
    angles: [f32; 3],
    origin: [f32; 3],
    mins: [f32; 3],
    maxs: [f32; 3],
) -> ([f32; 3], [f32; 3]) {
    let rotated = bmodel && angles.iter().any(|angle| *angle != 0.0);
    let mut low = [0.0f32; 3];
    let mut high = [0.0f32; 3];
    if rotated {
        let radius = radius_from_bounds(mins, maxs);
        for axis in 0..3 {
            low[axis] = origin[axis] - radius;
            high[axis] = origin[axis] + radius;
        }
    } else {
        for axis in 0..3 {
            low[axis] = origin[axis] + mins[axis];
            high[axis] = origin[axis] + maxs[axis];
        }
    }
    for axis in 0..3 {
        low[axis] -= 1.0;
        high[axis] += 1.0;
    }
    (low, high)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offsets_follow_the_header_field_order() {
        // The members of both structs are four bytes wide, so nothing pads.
        // A trajectory is 36 bytes because of `trType`, so `apos` is not at 44.
        assert_eq!(S_POS_TRBASE, 24);
        assert_eq!(S_APOS, 48);
        assert_eq!(S_MODELINDEX, 160);
        assert_eq!(S_SOLID, 176);
        assert_eq!(ENTITY_STATE_SIZE, 208);
        // `r` repeats `entityState_t` before its own fields.
        assert_eq!(R_LINKED, 416);
        assert_eq!(R_BMODEL, 432);
        assert_eq!(R_MINS, 436);
        assert_eq!(R_CONTENTS, 460);
        assert_eq!(R_CURRENT_ORIGIN, 488);
        assert_eq!(R_OWNERNUM, 512);
        assert_eq!(ENTITY_SHARED_SIZE, 100);
        assert_eq!(ENTITY_SHARED_T_SIZE, 308);
        assert_eq!(SHARED_ENTITY_SIZE, 516);
    }

    #[test]
    fn a_brush_model_reports_the_bmodel_solid_marker() {
        assert_eq!(
            solid_value(true, -1, [-16.0; 3], [16.0; 3]),
            SOLID_BMODEL
        );
        assert_eq!(solid_value(false, 0, [-16.0; 3], [16.0; 3]), 0);
    }

    #[test]
    fn a_solid_box_packs_its_bytes() {
        let solid = solid_value(
            false,
            CONTENTS_SOLID,
            [-16.0, -16.0, -24.0],
            [16.0, 16.0, 40.0],
        );
        assert_eq!(solid & 0xff, 16, "the x width");
        assert_eq!((solid >> 8) & 0xff, 24, "the z reach below the origin");
        assert_eq!((solid >> 16) & 0xff, 72, "the z reach above, plus 32");
        assert_eq!(solid, (72 << 16) | (24 << 8) | 16);

        // A body collides like a solid; a trigger-like zero does not.
        assert_ne!(solid_value(false, CONTENTS_BODY, [-1.0; 3], [1.0; 3]), 0);
        assert_eq!(
            solid_value(false, 0x4000_0000, [-1.0; 3], [1.0; 3]),
            0,
            "CONTENTS_TRIGGER alone is not a collision box"
        );
    }

    #[test]
    fn a_huge_or_tiny_box_is_clamped_to_a_byte() {
        let huge = solid_value(false, CONTENTS_SOLID, [-1000.0; 3], [1000.0; 3]);
        assert_eq!(huge & 0xff, 255);
        assert_eq!((huge >> 8) & 0xff, 255);
        assert_eq!((huge >> 16) & 0xff, 255);

        // A zero box still stores the 32 units a standing body gets above it.
        let tiny = solid_value(false, CONTENTS_SOLID, [0.0; 3], [0.0; 3]);
        assert_eq!(tiny & 0xff, 1);
        assert_eq!((tiny >> 8) & 0xff, 1);
        assert_eq!((tiny >> 16) & 0xff, 32);
    }

    #[test]
    fn the_absolute_box_is_spread_by_one_unit() {
        let (low, high) = absolute_box(
            true,
            [0.0; 3],
            [10.0, 20.0, 30.0],
            [-16.0, -16.0, -24.0],
            [16.0, 16.0, 40.0],
        );
        assert_eq!(low, [-7.0, 3.0, 5.0]);
        assert_eq!(high, [27.0, 37.0, 71.0]);
    }

    #[test]
    fn a_rotated_brush_model_expands_to_its_radius() {
        let (low, high) = absolute_box(
            true,
            [0.0, 45.0, 0.0],
            [0.0; 3],
            [-16.0, -16.0, -24.0],
            [16.0, 16.0, 40.0],
        );
        // radius = sqrt(16^2 + 16^2 + 40^2), plus the unit spread
        let radius = (16.0f32 * 16.0 + 16.0 * 16.0 + 40.0 * 40.0).sqrt();
        assert!((low[0] - (-radius - 1.0)).abs() < 1e-4);
        assert!((high[2] - (radius + 1.0)).abs() < 1e-4);
        assert_eq!(low, [-radius - 1.0, -radius - 1.0, -radius - 1.0]);
    }

    #[test]
    fn an_unrotated_brush_model_keeps_its_own_bounds() {
        let (low, high) = absolute_box(
            false,
            [0.0, 45.0, 0.0],
            [0.0; 3],
            [-1.0, -2.0, -3.0],
            [1.0, 2.0, 3.0],
        );
        assert_eq!(low, [-2.0, -3.0, -4.0]);
        assert_eq!(high, [2.0, 3.0, 4.0]);
    }
}
