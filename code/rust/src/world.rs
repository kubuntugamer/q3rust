//! The world: the BSP's surfaces turned into a triangle batch per shader, lit
//! by the lightmaps q3map baked, plus the camera the client looks through.
//!
//! `tr_bsp.c` loads the vertexes and faces into `drawSurf_t`s and `tr_surface.c`
//! submits them; the subdivision into shader batches here is the same idea
//! with the surface list collapsed into one array per texture, which is what
//! the renderer can draw without a per-surface state change.
//!
//! Lightmaps are sampled on the CPU and folded into the vertex colour. That is
//! the same data `RB_CalcDiffuseColor` multiplies in, and it keeps the frame
//! to one texture binding per batch; the cost is that the light is evaluated
//! per vertex instead of per texel, which is invisible at the scale the stock
//! lightmaps are built at.

use std::collections::HashMap;

use crate::assets::Assets;
use crate::bsp::{Bsp, BspFace, DrawVert, LIGHTMAP_SIZE, SURFACE_PATCH};
use crate::texture;
use crate::vk::{Texture, Vertex3D, Vulkan};

/// `DEFAULT_VIEWHEIGHT`: the eye sits this far above the player origin.
pub const DEFAULT_VIEW_HEIGHT: f32 = 26.0;
/// `SURF_NODRAW`: a shader the map never draws.
const SURF_NODRAW: u32 = 0x80;
/// `r_znear`.
const NEAR_PLANE: f32 = 4.0;
/// Far plane, far enough for the largest stock map.
const FAR_PLANE: f32 = 4096.0;
/// `cg_fov`'s default: a horizontal field of view measured on a 4:3 screen.
pub const DEFAULT_FOV: f32 = 90.0;

/// One drawable run of triangles sharing a texture.
#[derive(Clone, Copy, Debug)]
pub struct Batch {
    pub first: usize,
    pub count: usize,
    pub texture: usize,
}

/// The map's static geometry, uploaded once.
pub struct World {
    /// Triangle-list vertexes; every batch indexes a range of this.
    pub vertices: Vec<Vertex3D>,
    pub batches: Vec<Batch>,
    textures: Vec<(String, Texture)>,
    /// Surfaces in the file.
    pub faces: usize,
    /// Surfaces that produced triangles.
    pub drawn: usize,
    /// Surfaces skipped (patches, nodraw, bad ranges).
    pub skipped: usize,
    /// Shader names that had no image in the paks.
    pub missing: Vec<String>,
}

impl World {
    /// Reads the map's render lumps and uploads a texture per shader.
    pub fn build(vk: &mut Vulkan, assets: &Assets, bytes: &[u8]) -> Result<Self, String> {
        let bsp = Bsp::load(bytes.to_vec()).map_err(|e| e.to_string())?;
        let textures = bsp.textures().map_err(|e| e.to_string())?;
        let faces = bsp.faces().map_err(|e| e.to_string())?;
        let verts = bsp.draw_verts().map_err(|e| e.to_string())?;
        let indexes = bsp.draw_indexes().map_err(|e| e.to_string())?;
        let lightmaps = bsp.lightmaps();

        let mut by_shader: HashMap<usize, Vec<Vertex3D>> = HashMap::new();
        let mut drawn = 0usize;
        let mut skipped = 0usize;

        for face in &faces {
            if let Some(shader) = textures.get(face.shader) {
                if shader.surface_flags & SURF_NODRAW != 0 {
                    skipped += 1;
                    continue;
                }
            }

            let Some(face_verts) = verts.get(face.first_vert..face.first_vert + face.num_verts)
            else {
                skipped += 1;
                continue;
            };

            // A patch stores a control grid in its vertex range and has no
            // index range of its own; `R_SubdividePatchToGrid` turns it into
            // the vertexes a triangle list can use.
            if face.surface_type == SURFACE_PATCH {
                match tessellate_patch(face, face_verts, lightmaps) {
                    Some(vertexes) if !vertexes.is_empty() => {
                        drawn += vertexes.len() / 3;
                        by_shader
                            .entry(face.shader)
                            .or_default()
                            .extend_from_slice(&vertexes);
                    }
                    _ => skipped += 1,
                }
                continue;
            }

            if face.num_verts < 3 || face.num_indexes < 3 {
                skipped += 1;
                continue;
            }
            let Some(face_indexes) = indexes.get(face.first_index..face.first_index + face.num_indexes)
            else {
                skipped += 1;
                continue;
            };

            // q3map writes indexes relative to the face's first vertex, but a
            // tolerant read costs nothing and a few tools emit absolute ones.
            let relative = face_indexes
                .iter()
                .all(|&index| index >= 0 && (index as usize) < face.num_verts);

            let batch = by_shader.entry(face.shader).or_default();
            for triangle in face_indexes.chunks_exact(3) {
                let mut vertexes = [Vertex3D::default(); 3];
                let mut complete = true;
                for (corner, &index) in triangle.iter().enumerate() {
                    let at = if relative {
                        index as usize
                    } else {
                        index.wrapping_sub(face.first_vert as i32) as usize
                    };
                    let Some(vertex) = face_verts.get(at) else {
                        complete = false;
                        break;
                    };
                    vertexes[corner] = vertex3d(vertex, face, lightmaps);
                }
                if complete {
                    batch.extend_from_slice(&vertexes);
                    drawn += 1;
                } else {
                    skipped += 1;
                }
            }
        }

        let mut world = World {
            vertices: Vec::new(),
            batches: Vec::new(),
            textures: Vec::new(),
            faces: faces.len(),
            drawn,
            skipped,
            missing: Vec::new(),
        };

        // Sorted so a map always produces the same batch order and the same
        // texture upload order.
        let mut shaders: Vec<usize> = by_shader.keys().copied().collect();
        shaders.sort_unstable();
        let mut cache: HashMap<usize, usize> = HashMap::new();
        let checker = checker_texture(vk)?;

        for shader in shaders {
            let Some(vertices) = by_shader.remove(&shader) else {
                continue;
            };
            if vertices.is_empty() {
                continue;
            }
            let index = match cache.get(&shader) {
                Some(index) => *index,
                None => {
                    let name = textures
                        .get(shader)
                        .map(|texture| texture.name.clone())
                        .unwrap_or_default();
                    let texture = match load_texture(vk, assets, &name) {
                        Ok(texture) => texture,
                        Err(_) => {
                            world.missing.push(name.clone());
                            vk.create_repeating_texture(64, 64, &checker.1)?
                        }
                    };
                    let index = world.textures.len();
                    world.textures.push((name, texture));
                    cache.insert(shader, index);
                    index
                }
            };
            let first = world.vertices.len();
            let count = vertices.len();
            world.vertices.extend(vertices);
            world.batches.push(Batch {
                first,
                count,
                texture: index,
            });
        }

        Ok(world)
    }

    /// The images the map draws with, for the console's loader line.
    pub fn texture_names(&self) -> impl Iterator<Item = &str> {
        self.textures.iter().map(|(name, _)| name.as_str())
    }

    pub fn textures(&self) -> &[(String, Texture)] {
        &self.textures
    }

    /// Releases the textures; the renderer must outlive this call.
    pub fn shutdown(self, vk: &mut Vulkan) {
        for (_, texture) in self.textures {
            vk.destroy_texture(texture);
        }
    }
}

/// One vertex of the world, with its lightmap folded into the colour.
fn vertex3d(vertex: &DrawVert, face: &BspFace, lightmaps: &[u8]) -> Vertex3D {
    Vertex3D {
        pos: vertex.xyz,
        uv: vertex.st,
        color: lightmap_color(face, vertex, lightmaps),
    }
}

/// Samples the face's lightmap at this vertex, as `R_LoadVertexes` leaves it
/// for `RB_CalcDiffuseColor`. Lightmaps are overbright by one bit, which is
/// what `r_overBrightBits` compensates for.
fn lightmap_color(face: &crate::bsp::BspFace, vertex: &crate::bsp::DrawVert, lightmaps: &[u8]) -> [u8; 4] {
    if face.lightmap < 0 {
        return [255, 255, 255, 255];
    }
    let page = face.lightmap as usize;
    let x = (face.lightmap_x + vertex.lightmap[0].round() as i32).clamp(0, LIGHTMAP_SIZE as i32 - 1) as usize;
    let y = (face.lightmap_y + vertex.lightmap[1].round() as i32).clamp(0, LIGHTMAP_SIZE as i32 - 1) as usize;
    let at = page * LIGHTMAP_SIZE * LIGHTMAP_SIZE * 3 + (y * LIGHTMAP_SIZE + x) * 3;
    match lightmaps.get(at..at + 3) {
        Some(rgb) => [
            (rgb[0] as u16 * 2).min(255) as u8,
            (rgb[1] as u16 * 2).min(255) as u8,
            (rgb[2] as u16 * 2).min(255) as u8,
            255,
        ],
        None => [255, 255, 255, 255],
    }
}

/// A shader's image: `textures/<name>.tga`, then the `.jpg` beside it, the
/// same fallback `R_LoadImage` makes.
fn load_texture(vk: &mut Vulkan, assets: &Assets, name: &str) -> Result<Texture, String> {
    if name.is_empty() {
        return Err("shader has no name".into());
    }
    let mut candidates = Vec::with_capacity(2);
    for extension in ["tga", "jpg"] {
        candidates.push(format!("{name}.{extension}"));
    }
    let mut last = format!("{name}: not in any pak");
    for candidate in candidates {
        let Some(bytes) = assets.read(&candidate) else {
            continue;
        };
        let decoded = if candidate.ends_with("jpg") {
            texture::jpeg(&bytes)
        } else {
            texture::tga(&bytes)
        };
        match decoded {
            Ok(image) => {
                return vk.create_repeating_texture(image.width, image.height, &image.rgba)
            }
            Err(e) => last = format!("{candidate}: {e}"),
        }
    }
    Err(last)
}

/// The magenta checker a shader without an image falls back to, so a missing
/// texture is visible instead of black.
fn checker_texture(_vk: &mut Vulkan) -> Result<(usize, Vec<u8>), String> {
    const SIZE: usize = 64;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let odd = ((x / 16) + (y / 16)) % 2 == 0;
            let at = (y * SIZE + x) * 4;
            rgba[at] = if odd { 200 } else { 40 };
            rgba[at + 1] = if odd { 40 } else { 200 };
            rgba[at + 2] = if odd { 200 } else { 40 };
            rgba[at + 3] = 255;
        }
    }
    Ok((SIZE, rgba))
}

/// Where the eye is and which way it looks.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub origin: [f32; 3],
    /// Degrees, growing to the left as `angle_vectors` does.
    pub yaw: f32,
    /// Degrees, positive looking down as Q3's `viewangles[PITCH]` is.
    pub pitch: f32,
}

/// `fov_y`: the vertical field of view that belongs to a horizontal `cg_fov`
/// on a 4:3 screen. The horizontal field then grows with the aspect ratio, so
/// a 16:9 or 21:9 display sees more of the map instead of a stretched one.
pub fn fov_y(fov_x_degrees: f32) -> f32 {
    let tan_half = (fov_x_degrees.to_radians() / 2.0).tan();
    2.0 * (tan_half * 3.0 / 4.0).atan().to_degrees()
}

/// The horizontal field of view actually shown at this aspect ratio.
pub fn fov_x(fov_x_degrees: f32, aspect: f32) -> f32 {
    let tan_half = (fov_y(fov_x_degrees).to_radians() / 2.0).tan();
    2.0 * (tan_half * aspect).atan().to_degrees()
}

/// A right-handed projection with Vulkan's 0..1 depth and +Y down.
pub fn projection(aspect: f32, fov_x_degrees: f32) -> [f32; 16] {
    let half = fov_y(fov_x_degrees).to_radians() / 2.0;
    let f = 1.0 / half.tan();
    let mut matrix = [0.0f32; 16];
    matrix[0] = f / aspect;
    // Vulkan's clip-space Y grows downward, so the GL orientation is flipped.
    matrix[5] = -f;
    matrix[10] = FAR_PLANE / (NEAR_PLANE - FAR_PLANE);
    matrix[11] = -1.0;
    matrix[14] = NEAR_PLANE * FAR_PLANE / (NEAR_PLANE - FAR_PLANE);
    matrix
}

/// The view matrix for a camera: rows are right, up and back.
pub fn view(camera: &Camera) -> [f32; 16] {
    let (sin_yaw, cos_yaw) = camera.yaw.to_radians().sin_cos();
    let (sin_pitch, cos_pitch) = camera.pitch.to_radians().sin_cos();

    let forward = [
        cos_pitch * cos_yaw,
        cos_pitch * sin_yaw,
        -sin_pitch,
    ];
    let right = [sin_yaw, -cos_yaw, 0.0];
    let up = [
        right[1] * forward[2] - right[2] * forward[1],
        right[2] * forward[0] - right[0] * forward[2],
        right[0] * forward[1] - right[1] * forward[0],
    ];
    let eye = camera.origin;

    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];

    // Column-major, the layout a GLSL mat4 is read in.
    [
        right[0],
        up[0],
        -forward[0],
        0.0,
        right[1],
        up[1],
        -forward[1],
        0.0,
        right[2],
        up[2],
        -forward[2],
        0.0,
        -dot(right, eye),
        -dot(up, eye),
        dot(forward, eye),
        1.0,
    ]
}

/// `a * b`, both column-major.
pub fn multiply(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for column in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k * 4 + row] * b[column * 4 + k];
            }
            out[column * 4 + row] = sum;
        }
    }
    out
}

/// The matrix the world is drawn with.
pub fn view_projection(camera: &Camera, aspect: f32, fov_x_degrees: f32) -> [f32; 16] {
    multiply(&projection(aspect, fov_x_degrees), &view(camera))
}

/// `MAX_GRID_SIZE` from `tr_curve.c`: the largest grid a patch subdivides to.
const MAX_GRID_SIZE: usize = 32;
/// `r_subdivisions`, which the stock renderer leaves at its default of 4.
const SUBDIVISIONS: f32 = 4.0;

/// `LerpDrawVert`: the midpoint of two control points, in every field the
/// renderer reads. Normals are left at zero because the world shader takes its
/// light from the baked lightmap instead.
fn lerp(a: &DrawVert, b: &DrawVert) -> DrawVert {
    let half = |x: f32, y: f32| 0.5 * (x + y);
    DrawVert {
        xyz: [
            half(a.xyz[0], b.xyz[0]),
            half(a.xyz[1], b.xyz[1]),
            half(a.xyz[2], b.xyz[2]),
        ],
        st: [half(a.st[0], b.st[0]), half(a.st[1], b.st[1])],
        lightmap: [
            half(a.lightmap[0], b.lightmap[0]),
            half(a.lightmap[1], b.lightmap[1]),
        ],
        normal: [0.0; 3],
    }
}

/// `Transpose`, which lets the subdivider run its row test in both directions
/// without a second copy of the loop.
fn transpose(grid: &mut [[DrawVert; MAX_GRID_SIZE]; MAX_GRID_SIZE], width: usize, height: usize) {
    if width > height {
        for i in 0..height {
            for j in i + 1..width {
                if j < height {
                    let temp = grid[j][i];
                    grid[j][i] = grid[i][j];
                    grid[i][j] = temp;
                } else {
                    grid[j][i] = grid[i][j];
                }
            }
        }
    } else {
        for i in 0..width {
            for j in i + 1..height {
                if j < width {
                    let temp = grid[i][j];
                    grid[i][j] = grid[j][i];
                    grid[j][i] = temp;
                } else {
                    grid[i][j] = grid[j][i];
                }
            }
        }
    }
}

/// `PutPointsOnCurve`: the stored vertexes are spline controls, so every other
/// one has to be pulled onto the curve before the surface is drawn.
fn put_points_on_curve(
    grid: &mut [[DrawVert; MAX_GRID_SIZE]; MAX_GRID_SIZE],
    width: usize,
    height: usize,
) {
    for i in 0..width {
        let mut j = 1;
        while j + 1 < height {
            let prev = lerp(&grid[j][i], &grid[j + 1][i]);
            let next = lerp(&grid[j][i], &grid[j - 1][i]);
            grid[j][i] = lerp(&prev, &next);
            j += 2;
        }
    }

    for j in 0..height {
        let mut i = 1;
        while i + 1 < width {
            let prev = lerp(&grid[j][i], &grid[j][i + 1]);
            let next = lerp(&grid[j][i], &grid[j][i - 1]);
            grid[j][i] = lerp(&prev, &next);
            i += 2;
        }
    }
}

/// `R_SubdividePatchToGrid`'s insertion pass: a span that bends by more than
/// `r_subdivisions` gains two vertexes, until the grid reaches the renderer's
/// limit. The C code also records a LOD error table, culls columns that lie on
/// the line and flips the grid for longer triangle strips; none of that moves
/// the surface, so only the inserted vertexes are kept here.
fn subdivide(
    grid: &mut [[DrawVert; MAX_GRID_SIZE]; MAX_GRID_SIZE],
    width: usize,
    height: usize,
) -> (usize, usize) {
    let mut width = width;
    let mut height = height;

    for _direction in 0..2 {
        let mut j = 0;
        while j + 2 < width {
            let mut deviation = 0.0f32;
            for i in 0..height {
                // The point the curve passes through above this span.
                let mid = lerp(
                    &lerp(&grid[i][j], &grid[i][j + 1]),
                    &lerp(&grid[i][j + 1], &grid[i][j + 2]),
                );
                let start = grid[i][j].xyz;
                let end = grid[i][j + 2].xyz;
                let axis = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
                let length = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
                if length < f32::EPSILON {
                    continue;
                }
                let axis = [axis[0] / length, axis[1] / length, axis[2] / length];
                let delta = [
                    mid.xyz[0] - start[0],
                    mid.xyz[1] - start[1],
                    mid.xyz[2] - start[2],
                ];
                // Distance from the curve point to the straight chord.
                let along = delta[0] * axis[0] + delta[1] * axis[1] + delta[2] * axis[2];
                let off = [
                    delta[0] - axis[0] * along,
                    delta[1] - axis[1] * along,
                    delta[2] - axis[2] * along,
                ];
                let distance = (off[0] * off[0] + off[1] * off[1] + off[2] * off[2]).sqrt();
                if distance > deviation {
                    deviation = distance;
                }
            }

            // A span already on the line needs no vertex, and a flat enough one
            // is left as it is; both move on to the next span.
            if deviation < 0.1 || width + 2 > MAX_GRID_SIZE || deviation <= SUBDIVISIONS {
                j += 2;
                continue;
            }

            // Two new columns, either side of the peak the chord misses.
            width += 2;
            for i in 0..height {
                let prev = lerp(&grid[i][j], &grid[i][j + 1]);
                let next = lerp(&grid[i][j + 1], &grid[i][j + 2]);
                let mid = lerp(&prev, &next);
                let mut k = width - 1;
                while k > j + 3 {
                    grid[i][k] = grid[i][k - 2];
                    k -= 1;
                }
                grid[i][j + 1] = prev;
                grid[i][j + 2] = mid;
                grid[i][j + 3] = next;
            }
            // The C loop backs up and rechecks this span, which may still
            // bend too much: `j` stays put rather than advancing.
        }

        transpose(grid, width, height);
        std::mem::swap(&mut width, &mut height);
    }

    put_points_on_curve(grid, width, height);
    (width, height)
}

/// Turns one patch face into triangles: its vertex range is a
/// `patchWidth` x `patchHeight` control grid, which subdivides into a grid of
/// vertexes, and each grid cell becomes two triangles.
fn tessellate_patch(face: &BspFace, control: &[DrawVert], lightmaps: &[u8]) -> Option<Vec<Vertex3D>> {
    let patch_width = face.patch_width as usize;
    let patch_height = face.patch_height as usize;
    // A patch needs at least a 3x3 control grid, and no map exceeds the
    // renderer's own grid limit.
    if patch_width < 3 || patch_height < 3 {
        return None;
    }
    if patch_width > MAX_GRID_SIZE || patch_height > MAX_GRID_SIZE {
        return None;
    }
    if control.len() < patch_width * patch_height {
        return None;
    }

    let mut grid = [[DrawVert::default(); MAX_GRID_SIZE]; MAX_GRID_SIZE];
    for j in 0..patch_height {
        for i in 0..patch_width {
            grid[j][i] = control[j * patch_width + i];
        }
    }

    let (width, height) = subdivide(&mut grid, patch_width, patch_height);
    if width < 2 || height < 2 {
        return None;
    }

    let mut vertexes = Vec::with_capacity((width - 1) * (height - 1) * 6);
    for j in 0..height - 1 {
        for i in 0..width - 1 {
            let a = vertex3d(&grid[j][i], face, lightmaps);
            let b = vertex3d(&grid[j][i + 1], face, lightmaps);
            let c = vertex3d(&grid[j + 1][i + 1], face, lightmaps);
            let d = vertex3d(&grid[j + 1][i], face, lightmaps);
            vertexes.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    Some(vertexes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_four_by_three_screen_keeps_the_configured_field() {
        let horizontal = fov_x(90.0, 4.0 / 3.0);
        assert!((horizontal - 90.0).abs() < 0.01, "{horizontal}");
    }

    #[test]
    fn a_wide_screen_sees_more_not_stretched() {
        let wide = fov_x(90.0, 16.0 / 9.0);
        assert!(wide > 100.0, "{wide}");
        // The vertical field is the one the 4:3 value implies, unchanged.
        assert!((fov_y(90.0) - 73.74).abs() < 0.05, "{}", fov_y(90.0));
    }

    #[test]
    fn identity_multiply_leaves_a_matrix_alone() {
        let mut identity = [0.0f32; 16];
        identity[0] = 1.0;
        identity[5] = 1.0;
        identity[10] = 1.0;
        identity[15] = 1.0;
        let camera = Camera {
            origin: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        };
        let matrix = view(&camera);
        assert_eq!(multiply(&identity, &matrix), matrix);
        assert_eq!(multiply(&matrix, &identity), matrix);
    }

    #[test]
    fn looking_down_the_x_axis_maps_it_to_depth() {
        // yaw 0 faces +X, so a point straight ahead must land in front of the
        // camera after the projection.
        let camera = Camera {
            origin: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        };
        let mvp = view_projection(&camera, 4.0 / 3.0, 90.0);
        let point = [100.0, 0.0, 0.0];
        let mut clip = [0.0f32; 4];
        for row in 0..4 {
            clip[row] = (0..4).map(|k| mvp[k * 4 + row] * [point[0], point[1], point[2], 1.0][k]).sum();
        }
        assert!(clip[3] > 0.0, "{clip:?}");
        let depth = clip[2] / clip[3];
        assert!((0.0..=1.0).contains(&depth), "{depth}");
    }

    /// A patch face, built the way `dface_t` stores one.
    fn patch_face(width: i32, height: i32) -> BspFace {
        BspFace {
            shader: 0,
            surface_type: SURFACE_PATCH,
            first_vert: 0,
            num_verts: (width * height) as usize,
            first_index: 0,
            num_indexes: 0,
            lightmap: -1,
            lightmap_x: 0,
            lightmap_y: 0,
            patch_width: width,
            patch_height: height,
        }
    }

    /// A control grid on a 64-unit lattice. `lift` raises the interior points,
    /// which is what bends the surface.
    fn control_grid(width: usize, height: usize, lift: f32) -> Vec<DrawVert> {
        let mut verts = Vec::with_capacity(width * height);
        for j in 0..height {
            for i in 0..width {
                let interior = i > 0 && i + 1 < width && j > 0 && j + 1 < height;
                verts.push(DrawVert {
                    xyz: [
                        i as f32 * 64.0,
                        j as f32 * 64.0,
                        if interior { lift } else { 0.0 },
                    ],
                    st: [i as f32, j as f32],
                    lightmap: [i as f32, j as f32],
                    normal: [0.0, 0.0, 1.0],
                });
            }
        }
        verts
    }

    #[test]
    fn a_flat_patch_keeps_its_plane_without_subdividing() {
        let control = control_grid(3, 3, 0.0);
        let triangles = tessellate_patch(&patch_face(3, 3), &control, &[]).unwrap();
        // A 3x3 grid with no bend is four quads, so eight triangles.
        assert_eq!(triangles.len(), 8 * 3);
        assert!(triangles.iter().all(|vertex| vertex.pos[2] == 0.0));
    }

    #[test]
    fn a_curved_patch_subdivides_and_lifts_the_surface() {
        let control = control_grid(3, 3, 64.0);
        let triangles = tessellate_patch(&patch_face(3, 3), &control, &[]).unwrap();
        // The bend is past the subdivision tolerance, so the grid gained rows
        // and columns instead of staying 3x3.
        assert!(triangles.len() > 8 * 3, "{} vertexes", triangles.len());
        let top = triangles
            .iter()
            .map(|vertex| vertex.pos[2])
            .fold(f32::MIN, f32::max);
        assert!(top > 0.0 && top <= 64.0, "{top}");
        // The surface stays inside the control grid's footprint.
        assert!(triangles
            .iter()
            .all(|vertex| (0.0..=128.0).contains(&vertex.pos[0])));
    }

    #[test]
    fn a_patch_whose_vertex_range_is_too_small_is_refused() {
        let control = control_grid(3, 3, 0.0);
        // Nine control points cannot describe a 5x5 grid.
        assert!(tessellate_patch(&patch_face(5, 5), &control, &[]).is_none());
        // Below 3x3 there is no curve to draw.
        assert!(tessellate_patch(&patch_face(2, 3), &control, &[]).is_none());
    }
}
