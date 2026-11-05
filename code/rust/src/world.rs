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
use crate::bsp::{Bsp, LIGHTMAP_SIZE};
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
            // Patches are tessellated separately in the C renderer; they are
            // the curved surfaces, and drawing their control mesh would be
            // wrong, so they are skipped until the tessellator lands.
            if face.surface_type == 2 {
                skipped += 1;
                continue;
            }
            if face.num_verts < 3 || face.num_indexes < 3 {
                skipped += 1;
                continue;
            }
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
fn vertex3d(vertex: &crate::bsp::DrawVert, face: &crate::bsp::BspFace, lightmaps: &[u8]) -> Vertex3D {
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
}
