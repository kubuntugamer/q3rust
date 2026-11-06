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
use crate::bg_misc;
use crate::bsp::{Bsp, BspFace, DrawVert, LIGHTMAP_SIZE, SURFACE_PATCH};
use crate::items::{self, ItemMark};
use crate::model::Md3Model;
use crate::shader::{
    Parser, ShaderScript, ShaderStage, ShaderTextIndex, TexMod, TextureMap, GLS_DSTBLEND_ONE,
    GLS_SRCBLEND_ONE,
};
use crate::texture;
use crate::vk::{BlendMode, Texture, Vertex3D, Vulkan};

/// What a map's geometry costs, counted without a GPU. `audit_map` reads the
/// same lumps and runs the same tessellation `World::build` does, so a map that
/// audits cleanly is one the renderer can draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapAudit {
    pub rooms: usize,
    pub faces: usize,
    /// Triangles straight from the BSP's index list.
    pub triangles: usize,
    /// Triangles the patch tessellator produced.
    pub patch_triangles: usize,
    /// Non-triangle faces those room groups contain.
    pub patch_faces: usize,
    /// Surfaces a shader marks `SURF_NODRAW`: not drawn, by design.
    pub nodraw: usize,
    /// Patches whose control grid would not tessellate.
    pub patch_failed: usize,
    /// Faces that carry no triangle at all (fewer than three vertexes or
    /// indexes). q3map emits a few of these; they draw nothing by nature.
    pub empty: usize,
    /// Faces or triangles whose indexes or vertex range fall outside the data.
    pub out_of_range: usize,
    /// Entity blocks in the map.
    pub entities: usize,
    /// How many of those are `info_player_deathmatch` start points, which is
    /// what a match needs before it can spawn anyone.
    pub player_spawns: usize,
    /// Pickups the map places: items with a row in `bg_itemlist` and an origin.
    pub items: usize,
}

impl MapAudit {
    /// Every surface that produced no triangles, whether on purpose or not.
    pub fn skipped(&self) -> usize {
        self.nodraw + self.patch_failed + self.empty + self.out_of_range
    }
}

/// Counts one map's surfaces. This is the headless half of `World::build`: no
/// textures, no device, no window.
pub fn audit_map(bytes: &[u8]) -> Result<MapAudit, String> {
    let bsp = Bsp::load(bytes.to_vec()).map_err(|e| format!("{e:?}"))?;
    audit_bsp(&bsp)
}

/// The cheap map listing the game code wants before the collision geometry is
/// built: entity blocks, spawn points and the room count, read straight from a
/// loaded BSP. It goes through the same counter as [`audit_map`], so the two
/// can never disagree about a map.
pub fn map_edges(bsp: &Bsp) -> MapAudit {
    audit_bsp(bsp).unwrap_or_default()
}

/// The one surface counter both entry points use.
fn audit_bsp(bsp: &Bsp) -> Result<MapAudit, String> {
    let faces = bsp.faces().map_err(|e| format!("{e:?}"))?;
    let textures = bsp.textures().map_err(|e| format!("{e:?}"))?;
    let verts = bsp.draw_verts().map_err(|e| format!("{e:?}"))?;
    let indexes = bsp.draw_indexes().map_err(|e| format!("{e:?}"))?;
    let models = bsp.models().map_err(|e| format!("{e:?}"))?;
    let lightmaps = bsp.lightmaps();

    let mut audit = MapAudit {
        // One inline model per piece of geometry: the world plus each `*N`
        // submodel. A shipped map always has at least model 0.
        rooms: models.len(),
        faces: faces.len(),
        ..Default::default()
    };
    for face in &faces {
        // `World::build` drops these before looking at the geometry, so the
        // audit must not count them as a failure.
        if let Some(shader) = textures.get(face.shader) {
            if shader.surface_flags & SURF_NODRAW != 0 {
                audit.nodraw += 1;
                continue;
            }
        }
        let Some(face_verts) = verts.get(face.first_vert..face.first_vert + face.num_verts) else {
            audit.out_of_range += 1;
            continue;
        };

        if face.surface_type == SURFACE_PATCH {
            audit.patch_faces += 1;
            match tessellate_patch(face, face_verts, lightmaps) {
                Some(vertices) if !vertices.is_empty() => {
                    audit.patch_triangles += vertices.len() / 3;
                }
                _ => audit.patch_failed += 1,
            }
            continue;
        }

        if face.num_verts < 3 || face.num_indexes < 3 {
            audit.empty += 1;
            continue;
        }
        let Some(list) = indexes.get(face.first_index..face.first_index + face.num_indexes) else {
            audit.out_of_range += 1;
            continue;
        };

        // Same tolerant read as `World::build`: relative indexes are the norm,
        // but a few tools emit absolute ones.
        let relative = list
            .iter()
            .all(|&index| index >= 0 && (index as usize) < face.num_verts);
        for triangle in list.chunks_exact(3) {
            let resolves = triangle.iter().all(|&index| {
                let at = if relative {
                    index as usize
                } else {
                    index.wrapping_sub(face.first_vert as i32) as usize
                };
                face_verts.get(at).is_some()
            });
            if resolves {
                audit.triangles += 1;
            } else {
                audit.out_of_range += 1;
            }
        }
    }

    let entities = crate::entities::parse(bsp.entities());
    audit.entities = entities.len();
    audit.player_spawns = entities
        .iter()
        .filter(|entity| entity.classname.as_deref() == Some("info_player_deathmatch"))
        .count();
    audit.items = items::marks_in(&entities).len();
    Ok(audit)
}

#[cfg(test)]
mod map_edges_tests {
    use super::*;

    #[test]
    fn map_edges_comes_from_the_bsp_models() {
        use crate::bsp::tests::build_bsp;
        let bytes = build_bsp(b"{}");
        let bsp = crate::bsp::Bsp::load(bytes).unwrap();
        let audit = map_edges(&bsp);
        assert_eq!(audit.rooms, bsp.models().unwrap().len());
    }

    #[test]
    fn map_edges_mirrors_audit_map_on_a_test_bsp() {
        use crate::bsp::tests::build_bsp;
        let bytes = build_bsp(b"{}");
        let audit = audit_map(&bytes).unwrap();
        let edges = map_edges(&crate::bsp::Bsp::load(bytes).unwrap());
        assert_eq!(audit, edges);
    }

    #[test]
    fn a_map_with_entities_counts_them_the_same_way() {
        use crate::bsp::tests::build_bsp;
        let bytes = build_bsp(
            b"{ \"classname\" \"worldspawn\" }\n\
              { \"classname\" \"info_player_deathmatch\" \"origin\" \"64 128 16\" }",
        );
        let audit = map_edges(&crate::bsp::Bsp::load(bytes.clone()).unwrap());
        assert_eq!(audit, audit_map(&bytes).unwrap());
        assert_eq!(audit.entities, 2);
        assert_eq!(audit.player_spawns, 1);
    }
}

/// `DEFAULT_VIEWHEIGHT`: the eye sits this far above the player origin.
pub const DEFAULT_VIEW_HEIGHT: f32 = 26.0;
/// `SURF_NODRAW`: a shader the map never draws.
const SURF_NODRAW: u32 = 0x80;
/// `SURF_SKY`: the surface the skybox is drawn in place of.
const SURF_SKY: u32 = 0x4;
/// `r_znear`.
const NEAR_PLANE: f32 = 4.0;
/// Far plane, far enough for the largest stock map.
const FAR_PLANE: f32 = 4096.0;
/// `cg_fov`'s default: a horizontal field of view measured on a 4:3 screen.
pub const DEFAULT_FOV: f32 = 90.0;

/// One stage of a shader batch: the image it draws, how it blends, and how its
/// texture coordinates move. `lit` is true for the pass the baked lightmap
/// tints; detail and additive passes carry their own colour.
#[derive(Clone, Copy, Debug)]
pub struct Stage {
    pub texture: usize,
    pub blend: BlendMode,
    /// `[scale_s, scale_t, scroll_s, scroll_t]`. Scroll is per second.
    pub uv: [f32; 4],
    pub lit: bool,
}

/// One drawable run of triangles sharing a shader, drawn once per stage.
#[derive(Clone, Debug)]
pub struct Batch {
    pub first: usize,
    pub count: usize,
    pub stages: Vec<Stage>,
}

/// One surface of a pickup's model: the texture it names and its triangles in
/// model space, so an item only needs its origin moved to be drawn.
pub struct ItemSurface {
    /// Index into [`World::textures`].
    pub texture: usize,
    pub vertices: Vec<Vertex3D>,
    /// True when the surface has no art and its vertex colour is the item's
    /// own colour instead: the lit path takes the colour as the tint.
    pub lit: bool,
}

/// A pickup's own md3 model, as the paks hold it.
pub struct ItemModel {
    /// The `bg_itemlist` row this model is for.
    pub item_index: usize,
    pub surfaces: Vec<ItemSurface>,
}

/// The map's static geometry, uploaded once.
pub struct World {
    /// Triangle-list vertexes; every batch indexes a range of this.
    pub vertices: Vec<Vertex3D>,
    pub batches: Vec<Batch>,
    /// The pickups the map places, at the origins it gave them. The client
    /// keeps the live copy in an [`crate::items::ItemField`].
    pub items: Vec<ItemMark>,
    /// The model each kind of pickup on this map draws with.
    pub item_models: Vec<ItemModel>,
    /// The map's skybox, when its sky shader names one.
    pub sky: Option<Sky>,
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

        // The sky is whatever shader the map flags as sky: its script names
        // the six faces, so the name has to survive the face loop.
        let sky_name = faces.iter().find_map(|face| {
            let shader = textures.get(face.shader)?;
            (shader.surface_flags & SURF_SKY != 0).then(|| shader.name.clone())
        });

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
            items: Vec::new(),
            item_models: Vec::new(),
            sky: None,
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
        let mut cache: HashMap<String, usize> = HashMap::new();
        let checker = checker_texture(vk)?;
        let scripts = shader_scripts(assets);
        let index = shader_index(&scripts);

        for shader in shaders {
            let Some(vertices) = by_shader.remove(&shader) else {
                continue;
            };
            if vertices.is_empty() {
                continue;
            }
            let name = textures
                .get(shader)
                .map(|texture| texture.name.clone())
                .unwrap_or_default();

            // One draw per stage, so a shader's detail, blend and additive
            // passes land on the same triangles in the script's order.
            let mut stages = Vec::new();
            for spec in stage_list(&index, &name) {
                let texture = match cache.get(&spec.image) {
                    Some(index) => *index,
                    None => {
                        let loaded = match load_texture(vk, assets, &spec.image) {
                            Ok(texture) => texture,
                            Err(_) => {
                                world.missing.push(spec.image.clone());
                                vk.create_repeating_texture(64, 64, &checker.1)?
                            }
                        };
                        let at = world.textures.len();
                        world.textures.push((spec.image, loaded));
                        cache.insert(world.textures[at].0.clone(), at);
                        at
                    }
                };
                stages.push(Stage {
                    texture,
                    blend: spec.blend,
                    uv: spec.uv,
                    lit: spec.lit,
                });
            }

            let first = world.vertices.len();
            let count = vertices.len();
            world.vertices.extend(vertices);
            world.batches.push(Batch {
                first,
                count,
                stages,
            });
        }

        // The pickups, read from the same entity lump `audit_bsp` counts, and
        // the models they draw with.
        world.items = items::item_marks(bsp.entities());
        world.item_models = load_item_models(vk, assets, &world.items, &mut world.textures)?;

        world.sky = sky_name
            .as_deref()
            .and_then(|name| Sky::load(vk, assets, name, &index).ok().flatten());

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
        if let Some(sky) = self.sky {
            sky.shutdown(vk);
        }
    }
}

/// Loads the model for every kind of pickup this map places, once per kind.
///
/// The triangles go in model space, so drawing an item is only a matter of
/// moving the matrix: a map with a hundred pickups still shares thirty-odd
/// models. A model whose file or texture is missing is left out rather than
/// guessed at.
fn load_item_models(
    vk: &mut Vulkan,
    assets: &Assets,
    marks: &[ItemMark],
    textures: &mut Vec<(String, Texture)>,
) -> Result<Vec<ItemModel>, String> {
    let mut indexes: Vec<usize> = marks.iter().map(|mark| mark.item_index).collect();
    indexes.sort_unstable();
    indexes.dedup();

    let mut models = Vec::new();
    for item_index in indexes {
        let Some(path) = bg_misc::world_model(item_index) else {
            continue;
        };
        let Some(bytes) = assets.read(path) else {
            continue;
        };
        let Ok(model) = Md3Model::load(&bytes) else {
            continue;
        };

        // The item's own colour, used for any surface whose art is missing, so
        // a pickup is never invisible.
        let color = marks
            .iter()
            .find(|mark| mark.item_index == item_index)
            .map(|mark| mark.color)
            .unwrap_or([255, 255, 255, 255]);

        let mut surfaces = Vec::new();
        for surface in &model.surfaces {
            let Some(shader) = surface.shaders.first() else {
                continue;
            };
            let loaded = match textures.iter().position(|(name, _)| name == &shader.name) {
                Some(at) => Some(at),
                None => match load_texture(vk, assets, &shader.name) {
                    Ok(texture) => {
                        textures.push((shader.name.clone(), texture));
                        Some(textures.len() - 1)
                    }
                    Err(_) => None,
                },
            };
            let (texture, lit) = match loaded {
                Some(at) => (at, false),
                None => (flat_texture(vk, textures)?, true),
            };

            // Frame 0 is the pose a pickup is drawn in; the stock item models
            // are single-frame, so there is nothing to animate yet.
            let mesh = surface.lerp_vertices(0, 0, 0.0);
            let mut vertices = Vec::with_capacity(surface.triangles.len() * 3);
            for triangle in &surface.triangles {
                for &index in triangle {
                    let Some(vertex) = mesh.get(index as usize) else {
                        continue;
                    };
                    vertices.push(Vertex3D {
                        pos: vertex.position,
                        uv: vertex.tex_coords,
                        color: if lit { color } else { [255, 255, 255, 255] },
                    });
                }
            }
            if !vertices.is_empty() {
                surfaces.push(ItemSurface {
                    texture,
                    vertices,
                    lit,
                });
            }
        }

        if !surfaces.is_empty() {
            models.push(ItemModel {
                item_index,
                surfaces,
            });
        }
    }
    Ok(models)
}

/// A 1×1 white image, shared by every surface whose art is missing.
fn flat_texture(vk: &mut Vulkan, textures: &mut Vec<(String, Texture)>) -> Result<usize, String> {
    const NAME: &str = "items:flat";
    if let Some(at) = textures.iter().position(|(name, _)| name == NAME) {
        return Ok(at);
    }
    let texture = vk.create_texture(1, 1, &[255, 255, 255, 255])?;
    textures.push((NAME.to_string(), texture));
    Ok(textures.len() - 1)
}

/// The camera matrix with a model's world origin folded in, so one static set
/// of triangles can be drawn anywhere without copying it.
///
/// The matrix is column-major: the translation column picks up the model's
/// origin through the rotation part, exactly as multiplying the matrices would.
pub fn translate(mvp: &[f32; 16], origin: [f32; 3]) -> [f32; 16] {
    let mut moved = *mvp;
    for row in 0..4 {
        moved[12 + row] += mvp[row] * origin[0]
            + mvp[4 + row] * origin[1]
            + mvp[8 + row] * origin[2];
    }
    moved
}

/// How far the skybox sits from the camera. It has to stay inside
/// [`FAR_PLANE`], or the sky would be clipped away.
const SKY_RADIUS: f32 = 2048.0;

/// One face of the skybox: its normal and the two texture axes across it.
#[derive(Clone, Copy, Debug)]
struct SkyFace {
    normal: [f32; 3],
    s: [f32; 3],
    t: [f32; 3],
}

impl SkyFace {
    fn corner(&self, origin: [f32; 3], s: f32, t: f32) -> [f32; 3] {
        [
            origin[0] + (self.normal[0] + s * self.s[0] + t * self.t[0]) * SKY_RADIUS,
            origin[1] + (self.normal[1] + s * self.s[1] + t * self.t[1]) * SKY_RADIUS,
            origin[2] + (self.normal[2] + s * self.s[2] + t * self.t[2]) * SKY_RADIUS,
        ]
    }

    /// The two triangles of this face, rebuilt around `origin` so the box
    /// always surrounds the camera.
    fn vertices(&self, origin: [f32; 3]) -> [Vertex3D; 6] {
        let vertex = |s: f32, t: f32| Vertex3D {
            pos: self.corner(origin, s, t),
            uv: [s * 0.5 + 0.5, t * 0.5 + 0.5],
            color: [255, 255, 255, 255],
        };
        let (a, b, c, d) = (
            vertex(-1.0, -1.0),
            vertex(1.0, -1.0),
            vertex(1.0, 1.0),
            vertex(-1.0, 1.0),
        );
        [a, b, c, a, c, d]
    }
}

/// The six faces in `skyParms` order — `rt, bk, lf, ft, up, dn` — with the
/// texture axes Q3's sky vectors give them (forward +X, left +Y, up +Z).
const SKY_FACES: [SkyFace; 6] = [
    SkyFace {
        normal: [1.0, 0.0, 0.0],
        s: [0.0, -1.0, 0.0],
        t: [0.0, 0.0, -1.0],
    },
    SkyFace {
        normal: [0.0, 1.0, 0.0],
        s: [1.0, 0.0, 0.0],
        t: [0.0, 0.0, -1.0],
    },
    SkyFace {
        normal: [-1.0, 0.0, 0.0],
        s: [0.0, 1.0, 0.0],
        t: [0.0, 0.0, -1.0],
    },
    SkyFace {
        normal: [0.0, -1.0, 0.0],
        s: [-1.0, 0.0, 0.0],
        t: [0.0, 0.0, -1.0],
    },
    SkyFace {
        normal: [0.0, 0.0, 1.0],
        s: [1.0, 0.0, 0.0],
        t: [0.0, 1.0, 0.0],
    },
    SkyFace {
        normal: [0.0, 0.0, -1.0],
        s: [1.0, 0.0, 0.0],
        t: [0.0, -1.0, 0.0],
    },
];

/// The map's skybox: the six outer faces its sky shader names.
pub struct Sky {
    faces: Vec<(Texture, SkyFace)>,
}

impl Sky {
    /// Loads the faces `shader_name`'s `skyParms` names. A shader with no sky
    /// parms, or a face that is not in any pak, leaves the map without a
    /// skybox rather than stopping the client.
    fn load(
        vk: &mut Vulkan,
        assets: &Assets,
        shader_name: &str,
        index: &ShaderTextIndex,
    ) -> Result<Option<Sky>, String> {
        let Some(names) = sky_outerbox(index, shader_name) else {
            return Ok(None);
        };

        let mut faces = Vec::with_capacity(SKY_FACES.len());
        for (index, name) in names.iter().enumerate() {
            let Some((width, height, rgba)) = load_image(assets, name) else {
                return Ok(None);
            };
            let texture = vk.create_repeating_texture(width, height, &rgba)?;
            faces.push((texture, SKY_FACES[index]));
        }
        Ok(Some(Sky { faces }))
    }

    /// Draws the box around the camera. It writes no depth, so the map's
    /// geometry draws over it even when the sky is submitted first.
    pub fn draw(&self, vk: &mut Vulkan, camera: &Camera, mvp: &[f32; 16]) -> Result<(), String> {
        for (texture, face) in &self.faces {
            vk.draw_sky(texture, &face.vertices(camera.origin), mvp)?;
        }
        Ok(())
    }

    /// Releases the six textures; the renderer must outlive this call.
    pub fn shutdown(self, vk: &mut Vulkan) {
        for (texture, _) in self.faces {
            vk.destroy_texture(texture);
        }
    }
}

/// The `scripts/*.shader` files, as text, in pak order.
fn shader_scripts(assets: &Assets) -> Vec<String> {
    assets
        .files("scripts/", ".shader")
        .into_iter()
        .filter_map(|path| assets.read(&path))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect()
}

/// The shader text index the world and the skybox both look names up in.
fn shader_index(scripts: &[String]) -> ShaderTextIndex {
    let texts: Vec<&str> = scripts.iter().map(String::as_str).collect();
    ShaderTextIndex::build(&texts)
}

/// `FindShaderInShaderText` plus `ParseShader`: the named shader's definition,
/// or none when no script claims the name.
fn shader_script(index: &ShaderTextIndex, shader_name: &str) -> Option<ShaderScript> {
    let text = index.find(shader_name)?;
    let mut script = ShaderScript::default();
    script.name = shader_name.to_string();
    let mut parser = Parser::new(text);
    parser.parse_shader(&mut script).ok()?;
    Some(script)
}

/// The six outer face names `skyParms` names, if the shader has a sky.
fn sky_outerbox(index: &ShaderTextIndex, shader_name: &str) -> Option<[String; 6]> {
    shader_script(index, shader_name)?.sky?.outerbox
}

/// `MAX_STAGES`: how many stage passes one batch may draw.
const MAX_STAGES: usize = 4;

/// A stage before its texture is uploaded.
struct StageSpec {
    image: String,
    blend: BlendMode,
    uv: [f32; 4],
    lit: bool,
}

/// The stages a shader draws, in script order. A shader no script defines is
/// an implicit one: its own name is the image, drawn lit and opaque.
fn stage_list(index: &ShaderTextIndex, name: &str) -> Vec<StageSpec> {
    let mut stages = Vec::new();
    if let Some(script) = shader_script(index, name) {
        for stage in script.stages.iter().filter(|stage| stage.active) {
            let Some(image) = stage_image(stage) else {
                continue;
            };
            stages.push(StageSpec {
                image: image.to_string(),
                blend: blend_mode(stage.state_bits),
                uv: stage_transform(&stage.bundle.tex_mods),
                // Only the first pass carries the surface's lightmap.
                lit: stages.is_empty(),
            });
            if stages.len() == MAX_STAGES {
                break;
            }
        }
    }
    if stages.is_empty() {
        stages.push(StageSpec {
            image: name.to_string(),
            blend: BlendMode::Opaque,
            uv: [1.0, 1.0, 0.0, 0.0],
            lit: true,
        });
    }
    stages
}

/// The image a stage draws with. The lightmap is already in the vertex colour
/// and a video map has no texture to upload, so both leave the stage out.
fn stage_image(stage: &ShaderStage) -> Option<&str> {
    match stage.bundle.map.as_ref()? {
        TextureMap::File { name, .. } => Some(name.as_str()),
        TextureMap::Anim { frames, .. } => frames.first().map(String::as_str),
        TextureMap::WhiteImage | TextureMap::Lightmap | TextureMap::Video(_) => None,
    }
}

/// `state_bits` reduced to the blends the renderer has pipelines for. Anything
/// unusual draws as `blend`, which is what a translucent stage needs.
fn blend_mode(state_bits: u32) -> BlendMode {
    let source = state_bits & 0x0f;
    let destination = state_bits & 0xf0;
    if source == 0 && destination == 0 {
        BlendMode::Opaque
    } else if source == GLS_SRCBLEND_ONE && destination == GLS_DSTBLEND_ONE {
        BlendMode::Add
    } else {
        BlendMode::Blend
    }
}

/// The stage's static texture-coordinate transform: scale and per-second
/// scroll. Rotation, turbulent and stretch need the clock per vertex and are
/// not applied yet.
fn stage_transform(tex_mods: &[TexMod]) -> [f32; 4] {
    let mut scale = [1.0f32, 1.0];
    let mut scroll = [0.0f32, 0.0];
    for tex_mod in tex_mods {
        match tex_mod {
            TexMod::Scale { scale: factor } => {
                scale[0] *= factor[0];
                scale[1] *= factor[1];
            }
            TexMod::Scroll { scroll: step } => {
                scroll[0] += step[0];
                scroll[1] += step[1];
            }
            _ => {}
        }
    }
    [scale[0], scale[1], scroll[0], scroll[1]]
}

/// An image by exact path, falling back to the `.jpg` beside a `.tga` — the
/// same pair `R_LoadImage` tries.
fn load_image(assets: &Assets, path: &str) -> Option<(u32, u32, Vec<u8>)> {
    let mut candidates = vec![path.to_string()];
    if let Some(stem) = path.strip_suffix(".tga") {
        candidates.push(format!("{stem}.jpg"));
    }
    for candidate in candidates {
        let Some(bytes) = assets.read(&candidate) else {
            continue;
        };
        let decoded = if candidate.ends_with(".jpg") {
            texture::jpeg(&bytes)
        } else {
            texture::tga(&bytes)
        };
        if let Ok(image) = decoded {
            return Some((image.width, image.height, image.rgba));
        }
    }
    None
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

/// A shader's image: the name as written when it already carries an extension
/// (`map` lines in a script do), then the `.tga` and `.jpg` forms the same
/// fallback `R_LoadImage` makes.
fn load_texture(vk: &mut Vulkan, assets: &Assets, name: &str) -> Result<Texture, String> {
    if name.is_empty() {
        return Err("shader has no name".into());
    }
    let mut candidates = Vec::with_capacity(4);
    if name.contains('.') {
        candidates.push(name.to_string());
        // Some installations ship a shader's `.tga` as a `.jpg` beside it, and
        // the model files name the `.tga`: `R_LoadImage` falls back the same
        // way for the console's art.
        if name.to_ascii_lowercase().ends_with(".tga") {
            let mut jpeg = name.to_string();
            jpeg.replace_range(name.len() - 3.., "jpg");
            candidates.push(jpeg);
        }
    }
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
    fn an_empty_map_audits_to_nothing() {
        let bytes = crate::bsp::tests::build_bsp(b"");
        assert_eq!(audit_map(&bytes).unwrap(), MapAudit::default());
    }

    #[test]
    fn the_audit_counts_entities_and_player_spawns() {
        let bytes = crate::bsp::tests::build_bsp(
            b"{ \"classname\" \"worldspawn\" }\n\
              { \"classname\" \"info_player_deathmatch\" \"origin\" \"64 128 16\" }\n\
              { \"classname\" \"item_health\" \"origin\" \"8 8 8\" }",
        );
        let audit = audit_map(&bytes).unwrap();
        assert_eq!(audit.entities, 3);
        assert_eq!(audit.player_spawns, 1);
        // The health pack is a pickup; the other two blocks are not items.
        assert_eq!(audit.items, 1);
    }

    #[test]
    fn stages_come_out_of_the_script_in_order() {
        let script = "textures/test\n{\n {\n map textures/test1.tga\n }\n {\n map textures/test2.tga\n blendFunc add\n }\n}\n";
        let index = shader_index(&[script.to_string()]);
        let stages = stage_list(&index, "textures/test");
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].image, "textures/test1.tga");
        assert_eq!(stages[0].blend, BlendMode::Opaque);
        assert!(stages[0].lit);
        assert_eq!(stages[1].blend, BlendMode::Add);
        assert!(!stages[1].lit);
    }

    #[test]
    fn a_shader_with_no_script_draws_its_name_as_one_lit_stage() {
        let index = shader_index(&[String::new()]);
        let stages = stage_list(&index, "textures/base_wall/foo");
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].image, "textures/base_wall/foo");
        assert_eq!(stages[0].blend, BlendMode::Opaque);
        assert_eq!(stages[0].uv, [1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_scroll_texmod_becomes_a_per_second_offset() {
        let script = "textures/test\n{\n {\n map textures/t.tga\n tcMod scroll 0.25 -0.5\n tcMod scale 2 4\n }\n}\n";
        let index = shader_index(&[script.to_string()]);
        let stages = stage_list(&index, "textures/test");
        assert_eq!(stages[0].uv, [2.0, 4.0, 0.25, -0.5]);
    }

    #[test]
    fn a_lightmap_stage_is_not_uploaded_as_a_texture() {
        let script = "textures/test\n{\n {\n map textures/t.tga\n }\n {\n map $lightmap\n }\n}\n";
        let index = shader_index(&[script.to_string()]);
        let stages = stage_list(&index, "textures/test");
        assert_eq!(stages.len(), 1);
    }

    #[test]
    fn the_six_sky_faces_close_the_box_around_the_camera() {
        let origin = [120.0, -40.0, 64.0];
        let mut sum = [0.0f32; 3];
        for face in &SKY_FACES {
            sum[0] += face.normal[0];
            sum[1] += face.normal[1];
            sum[2] += face.normal[2];
            let vertices = face.vertices(origin);
            assert_eq!(vertices.len(), 6);
            for vertex in &vertices {
                let offset = [
                    vertex.pos[0] - origin[0],
                    vertex.pos[1] - origin[1],
                    vertex.pos[2] - origin[2],
                ];
                let length =
                    (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
                // A corner is normal + s*axis + t*axis, so between one and
                // sqrt(3) radii out.
                assert!(length >= SKY_RADIUS * 0.99, "{length}");
                assert!(length <= SKY_RADIUS * 1.75, "{length}");
                assert!((0.0..=1.0).contains(&vertex.uv[0]));
                assert!((0.0..=1.0).contains(&vertex.uv[1]));
            }
        }
        // Opposite faces cancel, so the box really closes.
        assert_eq!(sum, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_moving_camera_carries_the_sky_with_it() {
        let face = SKY_FACES[0];
        let before = face.vertices([0.0; 3]);
        let after = face.vertices([100.0, 0.0, 0.0]);
        for (before, after) in before.iter().zip(after.iter()) {
            assert!((after.pos[0] - before.pos[0] - 100.0).abs() < 1.0e-3);
            assert!((after.pos[1] - before.pos[1]).abs() < 1.0e-3);
            assert!((after.pos[2] - before.pos[2]).abs() < 1.0e-3);
        }
    }

    #[test]
    fn a_sky_shader_yields_its_six_outer_face_names() {
        let script = "textures/skies/test\n{\n skyParms env/test 512 -\n}\n";
        let names = sky_outerbox(&shader_index(&[script.to_string()]), "textures/skies/test").unwrap();
        assert_eq!(names[0], "env/test_rt.tga");
        assert_eq!(names[3], "env/test_ft.tga");
        assert_eq!(names[5], "env/test_dn.tga");
    }

    #[test]
    fn a_shader_without_sky_parms_has_no_box() {
        let script = "textures/stone\n{ { map textures/stone.tga } }\n";
        assert!(sky_outerbox(&shader_index(&[script.to_string()]), "textures/stone").is_none());
        // A name that is not in the scripts at all is equally absent.
        assert!(sky_outerbox(&shader_index(&[script.to_string()]), "textures/other").is_none());
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
