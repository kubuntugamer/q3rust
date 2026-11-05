//! The world's collision, from `cm_test.c` and `cm_trace.c`: a box swept
//! through the map's brushes, and the contents at a point.
//!
//! This is `CM_BoxTrace` with the world model, and `CM_PointContents` for it.
//! Both are the C algorithms rather than a re-invention, because the game
//! leans on their exact answers: `SV_Trace` stops a player a
//! `SURFACE_CLIP_EPSILON` short of a plane so the position stays valid for
//! network snapping, and `PM_SlideMove` spends that epsilon rather than
//! colliding again.
//!
//! The map is read out of the BSP's own lumps: planes, nodes, leafs, the leaf
//! brush lists, brushes, brush sides and shaders. Plane `type` and `signbits`,
//! the brush bounds from the first six axial sides, the contents of a brush
//! from its shader, and the surface flags of a side all come from the loaders
//! in `cm_load.c`; the brushes a trace tests are the ones whose leaf it
//! reaches, deduplicated with the same per-trace check count.
//!
//! What is not here: patches (the curved surfaces `cm_patch.c` turns into
//! collision facets) and capsule collision. A trace passes through a patch, and
//! `G_TRACECAPSULE` is answered by sweeping the bounding box instead of the
//! oriented capsule. Both are named where they are missed.

use crate::bsp::{
    Bsp, BspError, LUMP_BRUSHES, LUMP_BRUSHSIDES, LUMP_LEAFBRUSHES, LUMP_LEAFS, LUMP_NODES,
    LUMP_PLANES, LUMP_SHADERS,
};

/// `CONTENTS_SOLID` from `q_shared.h`: the contents of every world brush.
pub const CONTENTS_SOLID: i32 = 1;

/// `SURFACE_CLIP_EPSILON`: a trace stops this far in front of the surface, so
/// the resting position is still on the open side after snapping.
const SURFACE_CLIP_EPSILON: f32 = 0.125;
/// `BoxOnPlaneSide` treats a box within this distance of a plane as touching.
const PLANE_SIDE_EPSILON: f32 = 0.0001;

// `sizeof` of each BSP record this module reads.
const DPLANE_SIZE: usize = 16;
const DNODE_SIZE: usize = 36;
const DLEAF_SIZE: usize = 48;
const DBRUSH_SIZE: usize = 12;
const DBRUSHSIDE_SIZE: usize = 8;
const DSHADER_SIZE: usize = 72;

/// The six axial planes `CM_BoundBrush` expects first in every brush, in the
/// order `-x, +x, -y, +y, -z, +z`.
const AXIAL_SIDES: usize = 6;

/// A `cplane_t`: the normal, the distance along it, and the two values the
/// collision code uses to avoid a dot product.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plane {
    pub normal: [f32; 3],
    pub dist: f32,
    /// 0, 1 or 2 for a plane whose normal is +x, +y or +z, else 3.
    pub kind: u8,
    /// `signx + (signy << 1) + (signz << 2)` for a negative normal component.
    pub signbits: u8,
}

impl Default for Plane {
    fn default() -> Self {
        Plane {
            normal: [0.0; 3],
            dist: 0.0,
            kind: 3,
            signbits: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    plane: i32,
    children: [i32; 2],
}

#[derive(Clone, Copy, Debug)]
struct Leaf {
    first_brush: i32,
    num_brushes: i32,
}

#[derive(Clone, Copy, Debug)]
struct BrushSide {
    plane: i32,
    surface_flags: i32,
}

#[derive(Clone, Debug)]
struct Brush {
    contents: i32,
    first_side: i32,
    num_sides: i32,
    bounds: [[f32; 3]; 2],
}

/// A trace result: `trace_t` from `q_shared.h`. The caller writes this into the
/// module's memory field by field, so the layout matters to
/// [`crate::game_imports`], not here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trace {
    pub allsolid: bool,
    pub startsolid: bool,
    pub fraction: f32,
    pub endpos: [f32; 3],
    pub plane: Plane,
    pub surface_flags: i32,
    pub contents: i32,
    pub entity_num: i32,
}

impl Default for Trace {
    fn default() -> Self {
        Trace {
            allsolid: false,
            startsolid: false,
            fraction: 1.0,
            endpos: [0.0; 3],
            plane: Plane::default(),
            surface_flags: 0,
            contents: 0,
            entity_num: 0,
        }
    }
}

/// The world's collision model, built once per map.
pub struct Collision {
    planes: Vec<Plane>,
    nodes: Vec<Node>,
    leafs: Vec<Leaf>,
    leaf_brushes: Vec<i32>,
    brushes: Vec<Brush>,
    brush_sides: Vec<BrushSide>,
}

impl Collision {
    /// Reads the collision lumps. A lump whose length is not a whole number of
    /// records is refused, as `CMod_LoadSubmodels` refuses a "funny lump size";
    /// an index that points nowhere is treated as an empty brush rather than a
    /// panic, because a damaged map should not take the engine down.
    pub fn load(bsp: &Bsp) -> Result<Self, BspError> {
        let shaders: Vec<(i32, i32)> = records(bsp, LUMP_SHADERS, DSHADER_SIZE)?
            .iter()
            .map(|entry| (read_i32(entry, 64), read_i32(entry, 68)))
            .collect();
        // `CM_LoadBrushes` clamps an out-of-range shader to the first one.
        let shader = |number: i32| -> (i32, i32) {
            let index = if number < 0 {
                0
            } else {
                number as usize
            };
            shaders.get(index).copied().unwrap_or((0, 0))
        };

        let planes = records(bsp, LUMP_PLANES, DPLANE_SIZE)?
            .iter()
            .map(|entry| {
                let normal = [
                    read_f32(entry, 0),
                    read_f32(entry, 4),
                    read_f32(entry, 8),
                ];
                let mut kind = 3u8;
                if normal[0] == 1.0 {
                    kind = 0;
                }
                if normal[1] == 1.0 {
                    kind = 1;
                }
                if normal[2] == 1.0 {
                    kind = 2;
                }
                let mut signbits = 0u8;
                if normal[0] < 0.0 {
                    signbits |= 1;
                }
                if normal[1] < 0.0 {
                    signbits |= 2;
                }
                if normal[2] < 0.0 {
                    signbits |= 4;
                }
                Plane {
                    normal,
                    dist: read_f32(entry, 12),
                    kind,
                    signbits,
                }
            })
            .collect::<Vec<_>>();

        let brush_sides = records(bsp, LUMP_BRUSHSIDES, DBRUSHSIDE_SIZE)?
            .iter()
            .map(|entry| BrushSide {
                plane: read_i32(entry, 0),
                surface_flags: shader(read_i32(entry, 4)).0,
            })
            .collect::<Vec<_>>();

        let plane_at = |number: i32| -> Option<Plane> {
            if number < 0 {
                None
            } else {
                planes.get(number as usize).copied()
            }
        };

        let brushes = records(bsp, LUMP_BRUSHES, DBRUSH_SIZE)?
            .iter()
            .map(|entry| {
                let first_side = read_i32(entry, 0);
                let num_sides = read_i32(entry, 4);
                let contents = shader(read_i32(entry, 8)).1;
                // `CM_BoundBrush`: the first six sides are the axial planes, so
                // each pair's distance gives one side of the box.
                let mut bounds = [[0.0f32; 3]; 2];
                if num_sides as usize >= AXIAL_SIDES {
                    for axis in 0..3 {
                        let low = brush_sides
                            .get((first_side + axis as i32 * 2) as usize)
                            .and_then(|side| plane_at(side.plane));
                        let high = brush_sides
                            .get((first_side + axis as i32 * 2 + 1) as usize)
                            .and_then(|side| plane_at(side.plane));
                        if let (Some(low), Some(high)) = (low, high) {
                            bounds[0][axis] = -low.dist;
                            bounds[1][axis] = high.dist;
                        }
                    }
                }
                Brush {
                    contents,
                    first_side,
                    num_sides,
                    bounds,
                }
            })
            .collect::<Vec<_>>();

        let nodes = records(bsp, LUMP_NODES, DNODE_SIZE)?
            .iter()
            .map(|entry| Node {
                plane: read_i32(entry, 0),
                children: [read_i32(entry, 4), read_i32(entry, 8)],
            })
            .collect::<Vec<_>>();

        let leafs = records(bsp, LUMP_LEAFS, DLEAF_SIZE)?
            .iter()
            .map(|entry| Leaf {
                first_brush: read_i32(entry, 40),
                num_brushes: read_i32(entry, 44),
            })
            .collect::<Vec<_>>();

        let leaf_brushes = records(bsp, LUMP_LEAFBRUSHES, 4)?
            .iter()
            .map(|entry| read_i32(entry, 0))
            .collect::<Vec<_>>();

        Ok(Collision {
            planes,
            nodes,
            leafs,
            leaf_brushes,
            brushes,
            brush_sides,
        })
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn brush_count(&self) -> usize {
        self.brushes.len()
    }

    /// `CM_BoxTrace` for the world: `mins` and `maxs` are the zero-vector for a
    /// point or a ray, and `brush_mask` is the contents a brush must share to
    /// block the trace.
    pub fn box_trace(
        &self,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        brush_mask: i32,
    ) -> Trace {
        let mut work = Work::new(start, end, mins, maxs, brush_mask, self.brushes.len());
        if self.nodes.is_empty() {
            work.trace.endpos = end;
            return work.trace;
        }

        if start == end {
            // `CM_PositionTest`: the box does not move, so every leaf it
            // touches is asked whether the box is already inside a brush.
            self.position_test(&mut work);
        } else {
            let (from, to) = (work.start, work.end);
            self.trace_through_tree(&mut work, 0, 0.0, 1.0, from, to);
        }

        let fraction = work.trace.fraction;
        if fraction == 1.0 {
            work.trace.endpos = end;
        } else {
            for axis in 0..3 {
                work.trace.endpos[axis] = start[axis] + fraction * (end[axis] - start[axis]);
            }
        }
        work.trace
    }

    /// `CM_PointContents` for the world: the contents of every brush the
    /// point's leaf holds it inside of, ored together.
    pub fn point_contents(&self, p: [f32; 3]) -> i32 {
        if self.nodes.is_empty() {
            return 0;
        }

        let mut number = 0i32;
        // A damaged tree could point at itself; stop instead of spinning.
        for _ in 0..self.nodes.len() + 1 {
            if number < 0 {
                break;
            }
            let Some(node) = self.nodes.get(number as usize) else {
                return 0;
            };
            let Some(plane) = self.plane_of(node.plane) else {
                return 0;
            };
            let d = if plane.kind < 3 {
                p[plane.kind as usize] - plane.dist
            } else {
                dot(plane.normal, p) - plane.dist
            };
            number = if d < 0.0 {
                node.children[1]
            } else {
                node.children[0]
            };
        }
        if number >= 0 {
            return 0;
        }

        let Some(leaf) = self.leafs.get((-1 - number) as usize) else {
            return 0;
        };
        let mut contents = 0;
        for k in 0..leaf.num_brushes.max(0) {
            let Some(&index) = self.leaf_brushes.get((leaf.first_brush + k) as usize) else {
                continue;
            };
            if index < 0 {
                continue;
            }
            let Some(brush) = self.brushes.get(index as usize) else {
                continue;
            };
            let mut inside = true;
            for i in 0..brush.num_sides.max(0) {
                let Some(side) = self.brush_sides.get((brush.first_side + i) as usize) else {
                    continue;
                };
                let Some(plane) = self.plane_of(side.plane) else {
                    continue;
                };
                if dot(p, plane.normal) > plane.dist {
                    inside = false;
                    break;
                }
            }
            if inside {
                contents |= brush.contents;
            }
        }
        contents
    }

    fn plane_of(&self, number: i32) -> Option<Plane> {
        if number < 0 {
            None
        } else {
            self.planes.get(number as usize).copied()
        }
    }

    /// `CM_TraceThroughTree`: walk the node tree, keeping only the parts of the
    /// sweep the trace box can touch, and trace the leafs it reaches.
    fn trace_through_tree(
        &self,
        work: &mut Work,
        number: i32,
        p1f: f32,
        p2f: f32,
        p1: [f32; 3],
        p2: [f32; 3],
    ) {
        if work.trace.fraction <= p1f {
            return; // already hit something nearer
        }
        if number < 0 {
            self.trace_through_leaf(work, (-1 - number) as usize);
            return;
        }
        let Some(node) = self.nodes.get(number as usize) else {
            return;
        };
        let Some(plane) = self.plane_of(node.plane) else {
            return;
        };

        let (t1, t2, offset) = if plane.kind < 3 {
            let axis = plane.kind as usize;
            (
                p1[axis] - plane.dist,
                p2[axis] - plane.dist,
                work.extents[axis],
            )
        } else {
            (
                dot(plane.normal, p1) - plane.dist,
                dot(plane.normal, p2) - plane.dist,
                if work.is_point { 0.0 } else { 2048.0 },
            )
        };

        if t1 >= offset + 1.0 && t2 >= offset + 1.0 {
            self.trace_through_tree(work, node.children[0], p1f, p2f, p1, p2);
            return;
        }
        if t1 < -offset - 1.0 && t2 < -offset - 1.0 {
            self.trace_through_tree(work, node.children[1], p1f, p2f, p1, p2);
            return;
        }

        // The crossing points, each moved one epsilon off the plane.
        let (side, frac, frac2) = if t1 < t2 {
            let idist = 1.0 / (t1 - t2);
            (
                1,
                (t1 - offset + SURFACE_CLIP_EPSILON) * idist,
                (t1 + offset + SURFACE_CLIP_EPSILON) * idist,
            )
        } else if t1 > t2 {
            let idist = 1.0 / (t1 - t2);
            (
                0,
                (t1 + offset + SURFACE_CLIP_EPSILON) * idist,
                (t1 - offset - SURFACE_CLIP_EPSILON) * idist,
            )
        } else {
            (0, 1.0, 0.0)
        };
        let frac = frac.clamp(0.0, 1.0);
        let frac2 = frac2.clamp(0.0, 1.0);

        let midf = p1f + (p2f - p1f) * frac;
        let mid = lerp(p1, p2, frac);
        self.trace_through_tree(work, node.children[side], p1f, midf, p1, mid);

        let midf = p1f + (p2f - p1f) * frac2;
        let mid = lerp(p1, p2, frac2);
        self.trace_through_tree(work, node.children[side ^ 1], midf, p2f, mid, p2);
    }

    fn trace_through_leaf(&self, work: &mut Work, leaf: usize) {
        let Some(leaf) = self.leafs.get(leaf) else {
            return;
        };
        for k in 0..leaf.num_brushes.max(0) {
            let Some(&index) = self.leaf_brushes.get((leaf.first_brush + k) as usize) else {
                continue;
            };
            if index < 0 || !work.check(index as usize) {
                continue;
            }
            let Some(brush) = self.brushes.get(index as usize) else {
                continue;
            };
            if brush.contents & work.contents == 0 {
                continue;
            }
            self.trace_through_brush(work, brush);
            if work.trace.fraction == 0.0 {
                return;
            }
        }
    }

    /// `CM_TraceThroughBrush`: the latest entry across a plane and the earliest
    /// exit, which together say whether and where the box enters the brush.
    fn trace_through_brush(&self, work: &mut Work, brush: &Brush) {
        if brush.num_sides <= 0 {
            return;
        }

        let mut enter_fraction = -1.0f32;
        let mut leave_fraction = 1.0f32;
        let mut clip: Option<Plane> = None;
        let mut lead_flags = 0;
        let mut getout = false;
        let mut startout = false;

        for i in 0..brush.num_sides as usize {
            let Some(side) = self.brush_sides.get((brush.first_side + i as i32) as usize) else {
                continue;
            };
            let Some(plane) = self.plane_of(side.plane) else {
                continue;
            };

            let dist =
                plane.dist - dot(work.offsets[plane.signbits as usize], plane.normal);
            let d1 = dot(work.start, plane.normal) - dist;
            let d2 = dot(work.end, plane.normal) - dist;

            if d2 > 0.0 {
                getout = true; // endpoint is not in solid
            }
            if d1 > 0.0 {
                startout = true;
            }

            // Completely in front of this face: no intersection with the brush.
            if d1 > 0.0 && (d2 >= SURFACE_CLIP_EPSILON || d2 >= d1) {
                return;
            }
            // The sweep does not cross the plane, so it says nothing.
            if d1 <= 0.0 && d2 <= 0.0 {
                continue;
            }

            if d1 > d2 {
                let mut f = (d1 - SURFACE_CLIP_EPSILON) / (d1 - d2);
                if f < 0.0 {
                    f = 0.0;
                }
                if f > enter_fraction {
                    enter_fraction = f;
                    clip = Some(plane);
                    lead_flags = side.surface_flags;
                }
            } else {
                let mut f = (d1 + SURFACE_CLIP_EPSILON) / (d1 - d2);
                if f > 1.0 {
                    f = 1.0;
                }
                if f < leave_fraction {
                    leave_fraction = f;
                }
            }
        }

        if !startout {
            // The original point was inside the brush.
            work.trace.startsolid = true;
            if !getout {
                work.trace.allsolid = true;
                work.trace.fraction = 0.0;
                work.trace.contents = brush.contents;
            }
            return;
        }

        if enter_fraction < leave_fraction
            && enter_fraction > -1.0
            && enter_fraction < work.trace.fraction
        {
            if let Some(plane) = clip {
                work.trace.fraction = enter_fraction.max(0.0);
                work.trace.plane = plane;
                work.trace.surface_flags = lead_flags;
                work.trace.contents = brush.contents;
            }
        }
    }

    fn position_test(&self, work: &mut Work) {
        let mut bounds = [[0.0f32; 3]; 2];
        for axis in 0..3 {
            bounds[0][axis] = work.start[axis] + work.size[0][axis] - 1.0;
            bounds[1][axis] = work.start[axis] + work.size[1][axis] + 1.0;
        }
        let mut leafs = Vec::new();
        self.collect_leafs(bounds, &mut leafs);
        for leaf in leafs {
            self.test_in_leaf(work, leaf);
            if work.trace.allsolid {
                break;
            }
        }
    }

    fn collect_leafs(&self, bounds: [[f32; 3]; 2], out: &mut Vec<usize>) {
        let mut stack = vec![0i32];
        // Every node can be pushed twice; bound the walk by that.
        let mut budget = self.nodes.len() * 2 + 2;
        while let Some(number) = stack.pop() {
            if budget == 0 {
                return;
            }
            budget -= 1;
            if number < 0 {
                out.push((-1 - number) as usize);
                continue;
            }
            let Some(node) = self.nodes.get(number as usize) else {
                continue;
            };
            let Some(plane) = self.plane_of(node.plane) else {
                continue;
            };
            match box_on_plane_side(bounds, &plane) {
                1 => stack.push(node.children[0]),
                2 => stack.push(node.children[1]),
                _ => {
                    stack.push(node.children[0]);
                    stack.push(node.children[1]);
                }
            }
        }
    }

    fn test_in_leaf(&self, work: &mut Work, leaf: usize) {
        let Some(leaf) = self.leafs.get(leaf) else {
            return;
        };
        for k in 0..leaf.num_brushes.max(0) {
            let Some(&index) = self.leaf_brushes.get((leaf.first_brush + k) as usize) else {
                continue;
            };
            if index < 0 || !work.check(index as usize) {
                continue;
            }
            let Some(brush) = self.brushes.get(index as usize) else {
                continue;
            };
            if brush.contents & work.contents == 0 {
                continue;
            }
            self.test_box_in_brush(work, brush);
            if work.trace.allsolid {
                return;
            }
        }
    }

    /// `CM_TestBoxInBrush`: the first six planes are the axial hull the box's
    /// own bounds already satisfied, so only the rest are tested.
    fn test_box_in_brush(&self, work: &mut Work, brush: &Brush) {
        if brush.num_sides <= 0 {
            return;
        }
        for axis in 0..3 {
            if work.bounds[0][axis] > brush.bounds[1][axis]
                || work.bounds[1][axis] < brush.bounds[0][axis]
            {
                return;
            }
        }
        for i in AXIAL_SIDES..brush.num_sides as usize {
            let Some(side) = self.brush_sides.get((brush.first_side + i as i32) as usize) else {
                continue;
            };
            let Some(plane) = self.plane_of(side.plane) else {
                continue;
            };
            let dist =
                plane.dist - dot(work.offsets[plane.signbits as usize], plane.normal);
            if dot(work.start, plane.normal) - dist > 0.0 {
                return;
            }
        }
        work.trace.startsolid = true;
        work.trace.allsolid = true;
        work.trace.fraction = 0.0;
        work.trace.contents = brush.contents;
    }
}

/// The sweep the trace is working on: `traceWork_t`.
struct Work {
    start: [f32; 3],
    end: [f32; 3],
    size: [[f32; 3]; 2],
    /// `offsets[signbits]` is the box corner that points the way the plane's
    /// normal does.
    offsets: [[f32; 3]; 8],
    extents: [f32; 3],
    bounds: [[f32; 3]; 2],
    contents: i32,
    is_point: bool,
    trace: Trace,
    /// Which brushes this trace has already tested, the per-trace check count.
    checked: Vec<bool>,
    check_count: u32,
}

impl Work {
    fn new(
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        contents: i32,
        brushes: usize,
    ) -> Self {
        // `CM_Trace` makes the box symmetric about its centre, which keeps the
        // plane offsets in rotated brush models simple.
        let mut offset = [0.0f32; 3];
        for axis in 0..3 {
            offset[axis] = (mins[axis] + maxs[axis]) * 0.5;
        }
        let mut size = [[0.0f32; 3]; 2];
        for axis in 0..3 {
            size[0][axis] = mins[axis] - offset[axis];
            size[1][axis] = maxs[axis] - offset[axis];
        }
        let start = [
            start[0] + offset[0],
            start[1] + offset[1],
            start[2] + offset[2],
        ];
        let end = [end[0] + offset[0], end[1] + offset[1], end[2] + offset[2]];

        let mut offsets = [[0.0f32; 3]; 8];
        for (bits, entry) in offsets.iter_mut().enumerate() {
            for axis in 0..3 {
                entry[axis] = if bits & (1 << axis) != 0 {
                    size[1][axis]
                } else {
                    size[0][axis]
                };
            }
        }

        let is_point = size[0] == [0.0; 3];
        let extents = if is_point { [0.0; 3] } else { size[1] };

        let mut bounds = [[0.0f32; 3]; 2];
        for axis in 0..3 {
            if start[axis] < end[axis] {
                bounds[0][axis] = start[axis] + size[0][axis];
                bounds[1][axis] = end[axis] + size[1][axis];
            } else {
                bounds[0][axis] = end[axis] + size[0][axis];
                bounds[1][axis] = start[axis] + size[1][axis];
            }
        }

        Work {
            start,
            end,
            size,
            offsets,
            extents,
            bounds,
            contents,
            is_point,
            trace: Trace::default(),
            checked: vec![false; brushes],
            check_count: 0,
        }
    }

    /// Marks a brush as tested for this sweep. Returns false when it already
    /// was, the way `checkcount` skips a brush another leaf already held.
    fn check(&mut self, index: usize) -> bool {
        match self.checked.get_mut(index) {
            Some(slot) if !*slot => {
                *slot = true;
                self.check_count += 1;
                true
            }
            _ => false,
        }
    }
}

/// `BoxOnPlaneSide`: 1 when the box is entirely in front of the plane, 2 when
/// entirely behind it, 3 when it straddles.
fn box_on_plane_side(bounds: [[f32; 3]; 2], plane: &Plane) -> i32 {
    let mut low = 0.0f32;
    let mut high = 0.0f32;
    for axis in 0..3 {
        let normal = plane.normal[axis];
        if normal >= 0.0 {
            low += normal * bounds[0][axis];
            high += normal * bounds[1][axis];
        } else {
            low += normal * bounds[1][axis];
            high += normal * bounds[0][axis];
        }
    }

    let mut side = 0;
    if high - plane.dist >= -PLANE_SIDE_EPSILON {
        side = 1;
    }
    if low - plane.dist < PLANE_SIDE_EPSILON {
        side |= 2;
    }
    side
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn lerp(from: [f32; 3], to: [f32; 3], fraction: f32) -> [f32; 3] {
    [
        from[0] + fraction * (to[0] - from[0]),
        from[1] + fraction * (to[1] - from[1]),
        from[2] + fraction * (to[2] - from[2]),
    ]
}

fn read_i32(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// One lump as fixed-size records, refusing a length that is not a whole
/// number of them.
fn records<'a>(bsp: &'a Bsp, lump: usize, size: usize) -> Result<Vec<&'a [u8]>, BspError> {
    let bytes = bsp.lump(lump);
    if bytes.len() % size != 0 {
        return Err(BspError::BadLumpSize(lump));
    }
    Ok(bytes.chunks_exact(size).collect())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn dplane(normal: [f32; 3], dist: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity(DPLANE_SIZE);
        for value in normal {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&dist.to_le_bytes());
        out
    }

    /// `dnode_t`: the plane, two children, and the node's own culling box,
    /// which collision does not read but the record still carries.
    fn dnode(plane: i32, children: [i32; 2]) -> Vec<u8> {
        let mut out = Vec::with_capacity(DNODE_SIZE);
        out.extend_from_slice(&plane.to_le_bytes());
        out.extend_from_slice(&children[0].to_le_bytes());
        out.extend_from_slice(&children[1].to_le_bytes());
        out.resize(DNODE_SIZE, 0);
        out
    }

    fn dleaf(first_brush: i32, num_brushes: i32) -> Vec<u8> {
        let mut out = vec![0u8; DLEAF_SIZE];
        out[40..44].copy_from_slice(&first_brush.to_le_bytes());
        out[44..48].copy_from_slice(&num_brushes.to_le_bytes());
        out
    }

    fn dbrush(first_side: i32, num_sides: i32, shader: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(DBRUSH_SIZE);
        out.extend_from_slice(&first_side.to_le_bytes());
        out.extend_from_slice(&num_sides.to_le_bytes());
        out.extend_from_slice(&shader.to_le_bytes());
        out
    }

    fn dbrushside(plane: i32, shader: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(DBRUSHSIDE_SIZE);
        out.extend_from_slice(&plane.to_le_bytes());
        out.extend_from_slice(&shader.to_le_bytes());
        out
    }

    fn dshader(surface_flags: i32, contents: i32) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out.extend_from_slice(&surface_flags.to_le_bytes());
        out.extend_from_slice(&contents.to_le_bytes());
        out
    }

    /// A map made of one solid cube from -64 to 64 on every axis, with a node
    /// whose two children both reach the leaf that holds it.
    pub fn solid_cube() -> Collision {
        let planes: Vec<u8> = [
            ([-1.0, 0.0, 0.0], 64.0),
            ([1.0, 0.0, 0.0], 64.0),
            ([0.0, -1.0, 0.0], 64.0),
            ([0.0, 1.0, 0.0], 64.0),
            ([0.0, 0.0, -1.0], 64.0),
            ([0.0, 0.0, 1.0], 64.0),
        ]
        .iter()
        .flat_map(|(normal, dist)| dplane(*normal, *dist))
        .collect();
        let sides: Vec<u8> = (0..AXIAL_SIDES as i32)
            .flat_map(|plane| dbrushside(plane, 0))
            .collect();
        let brushes = dbrush(0, AXIAL_SIDES as i32, 0);
        let shaders = dshader(0x4, CONTENTS_SOLID);
        let nodes = dnode(0, [-1, -2]);
        let leafs: Vec<u8> = [dleaf(0, 1), dleaf(0, 1)].concat();
        let leaf_brushes = 0i32.to_le_bytes().to_vec();

        let bsp = Bsp::load(crate::bsp::tests::build_bsp_with(&[
            (LUMP_PLANES, &planes),
            (LUMP_BRUSHSIDES, &sides),
            (LUMP_BRUSHES, &brushes),
            (LUMP_SHADERS, &shaders),
            (LUMP_NODES, &nodes),
            (LUMP_LEAFS, &leafs),
            (LUMP_LEAFBRUSHES, &leaf_brushes),
        ]))
        .unwrap();
        Collision::load(&bsp).unwrap()
    }

    /// A map of flat boxes: each `(mins, maxs)` pair becomes one solid brush
    /// with the six axial sides `CM_BoundBrush` expects. The node splits the
    /// world but both of its children reach leafs holding every brush, so a
    /// trace finds them wherever it starts.
    pub fn arena(boxes: &[([f32; 3], [f32; 3])]) -> Collision {
        let mut planes = Vec::new();
        let mut sides = Vec::new();
        let mut brushes = Vec::new();
        for (number, (mins, maxs)) in boxes.iter().enumerate() {
            let first = (number * AXIAL_SIDES) as i32;
            let axial = [
                ([-1.0, 0.0, 0.0], -mins[0]),
                ([1.0, 0.0, 0.0], maxs[0]),
                ([0.0, -1.0, 0.0], -mins[1]),
                ([0.0, 1.0, 0.0], maxs[1]),
                ([0.0, 0.0, -1.0], -mins[2]),
                ([0.0, 0.0, 1.0], maxs[2]),
            ];
            for (side, (normal, dist)) in axial.into_iter().enumerate() {
                planes.extend_from_slice(&dplane(normal, dist));
                sides.extend_from_slice(&dbrushside(first + side as i32, 0));
            }
            brushes.extend_from_slice(&dbrush(first, AXIAL_SIDES as i32, 0));
        }
        let shaders = dshader(0, CONTENTS_SOLID);
        let nodes = dnode(0, [-1, -2]);
        let count = boxes.len() as i32;
        let leafs: Vec<u8> = [dleaf(0, count), dleaf(0, count)].concat();
        let leaf_brushes: Vec<u8> = (0..count).flat_map(i32::to_le_bytes).collect();

        let bsp = Bsp::load(crate::bsp::tests::build_bsp_with(&[
            (LUMP_PLANES, &planes),
            (LUMP_BRUSHSIDES, &sides),
            (LUMP_BRUSHES, &brushes),
            (LUMP_SHADERS, &shaders),
            (LUMP_NODES, &nodes),
            (LUMP_LEAFS, &leafs),
            (LUMP_LEAFBRUSHES, &leaf_brushes),
        ]))
        .unwrap();
        Collision::load(&bsp).unwrap()
    }

    /// A map with no nodes at all, as a map with no world brushes loads.
    pub fn empty() -> Collision {
        let bsp = Bsp::load(crate::bsp::tests::build_bsp(b"{}")).unwrap();
        Collision::load(&bsp).unwrap()
    }

    fn trace(collision: &Collision, start: [f32; 3], end: [f32; 3]) -> Trace {
        collision.box_trace(start, end, [0.0; 3], [0.0; 3], crate::collision::CONTENTS_SOLID)
    }

    #[test]
    fn the_world_brushes_load() {
        let world = solid_cube();
        assert_eq!(world.node_count(), 1);
        assert_eq!(world.brush_count(), 1);
    }

    #[test]
    fn a_point_inside_the_box_is_solid_and_outside_is_empty() {
        let world = solid_cube();
        assert_eq!(world.point_contents([0.0, 0.0, 0.0]), CONTENTS_SOLID);
        assert_eq!(world.point_contents([63.0, 0.0, 0.0]), CONTENTS_SOLID);
        assert_eq!(world.point_contents([65.0, 0.0, 0.0]), 0);
        assert_eq!(world.point_contents([0.0, 0.0, -100.0]), 0);
    }

    #[test]
    fn a_downward_trace_stops_one_epsilon_short_of_the_floor() {
        let world = solid_cube();
        let hit = trace(&world, [0.0, 0.0, 100.0], [0.0, 0.0, -100.0]);

        // d1 = 36, d2 = -164, so f = (36 - 0.125) / 200.
        let fraction = 35.875 / 200.0;
        assert!((hit.fraction - fraction).abs() < 1e-6, "{}", hit.fraction);
        assert!((hit.endpos[2] - 64.125).abs() < 1e-4);
        assert_eq!(hit.plane.normal, [0.0, 0.0, 1.0]);
        assert_eq!(hit.plane.dist, 64.0);
        assert_eq!(hit.plane.kind, 2);
        assert_eq!(hit.contents, CONTENTS_SOLID);
        assert!(!hit.startsolid);
        assert!(!hit.allsolid);
    }

    #[test]
    fn a_trace_that_misses_keeps_its_endpoint() {
        let world = solid_cube();
        let hit = trace(&world, [200.0, 0.0, 100.0], [200.0, 0.0, -100.0]);
        assert_eq!(hit.fraction, 1.0);
        assert_eq!(hit.endpos, [200.0, 0.0, -100.0]);
        assert_eq!(hit.contents, 0);
        assert!(!hit.startsolid);
    }

    #[test]
    fn a_box_stops_with_its_corner_on_the_floor() {
        let world = solid_cube();
        let hit = world.box_trace(
            [0.0, 0.0, 100.0],
            [0.0, 0.0, -100.0],
            [-16.0; 3],
            [16.0; 3],
            CONTENTS_SOLID,
        );
        // The offset corner is 16 below the centre, so the plane sits at 80.
        let fraction = 19.875 / 200.0;
        assert!((hit.fraction - fraction).abs() < 1e-6, "{}", hit.fraction);
        assert!((hit.endpos[2] - 80.125).abs() < 1e-4, "{:?}", hit.endpos);
    }

    #[test]
    fn starting_inside_solid_reports_startsolid() {
        let world = solid_cube();
        let hit = trace(&world, [0.0, 0.0, 0.0], [0.0, 0.0, 10.0]);
        assert!(hit.startsolid);
        assert!(hit.allsolid);
        assert_eq!(hit.fraction, 0.0);
        assert_eq!(hit.contents, CONTENTS_SOLID);
    }

    #[test]
    fn a_move_that_leaves_solid_is_allowed_out() {
        let world = solid_cube();
        let hit = trace(&world, [0.0, 0.0, 0.0], [0.0, 0.0, 200.0]);
        assert!(hit.startsolid, "the start is still reported as in solid");
        assert!(!hit.allsolid, "the end is out, so it is not all solid");
        assert_eq!(hit.fraction, 1.0);
        assert_eq!(hit.endpos, [0.0, 0.0, 200.0]);
    }

    #[test]
    fn a_mask_that_does_not_share_the_brush_contents_ignores_it() {
        let world = solid_cube();
        let hit = world.box_trace(
            [0.0, 0.0, 100.0],
            [0.0, 0.0, -100.0],
            [0.0; 3],
            [0.0; 3],
            0x4000_0000,
        );
        assert_eq!(hit.fraction, 1.0);
    }

    #[test]
    fn an_empty_map_has_no_contents_and_traces_straight_through() {
        let world = empty();
        assert_eq!(world.point_contents([0.0; 3]), 0);
        let hit = trace(&world, [0.0; 3], [100.0, 0.0, 0.0]);
        assert_eq!(hit.fraction, 1.0);
        assert_eq!(hit.endpos, [100.0, 0.0, 0.0]);
    }

    #[test]
    fn a_trace_that_ends_where_it_starts_tests_the_position() {
        let world = solid_cube();
        let hit = trace(&world, [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
        assert!(hit.allsolid);
        assert_eq!(hit.fraction, 0.0);

        let out = trace(&world, [200.0, 0.0, 0.0], [200.0, 0.0, 0.0]);
        assert!(!out.startsolid);
        assert_eq!(out.fraction, 1.0);
    }
}
