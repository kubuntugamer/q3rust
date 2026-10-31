//! MD3 model container, ported from the model path in
//! `code/renderer/tr_model.c` and `code/qcommon/qfiles.h`.
//!
//! This is the first stage of the renderer's model path: the file is
//! parsed into owned frames, tags and surface metadata, which is what
//! [`crate::refexport::RefExport::lerp_tag`] and
//! [`crate::refexport::RefExport::model_bounds`] need. Surface geometry —
//! triangles, texture coordinates and the compressed vertex positions and
//! normals — is decoded the way `LerpMeshVertexes` and `RB_SurfaceMesh`
//! do it, so a backend can build vertex buffers straight from this
//! module.
//!
//! All multi-byte fields are read little-endian, as `LittleLong`/
//! `LittleFloat` do in the C loader.

use crate::ref_types::{Orientation, Vec3};

/// `MD3_IDENT`: `'I' 'D' 'P' '3'` read as a little-endian integer.
pub const MD3_IDENT: u32 = 0x3350_4449;
/// `MD3_VERSION` in `qfiles.h`.
pub const MD3_VERSION: i32 = 15;

// limits from qfiles.h
pub const MD3_MAX_LODS: usize = 3;
pub const MD3_MAX_TRIANGLES: usize = 8192;
pub const MD3_MAX_VERTS: usize = 4096;
pub const MD3_MAX_SHADERS: usize = 256;
pub const MD3_MAX_FRAMES: usize = 1024;
pub const MD3_MAX_SURFACES: usize = 32;
pub const MD3_MAX_TAGS: usize = 16;

/// `MAX_QPATH`, the width of tag and surface names in the file.
pub const MD3_NAME_LENGTH: usize = 64;
/// Width of `md3Frame_t::name`.
pub const MD3_FRAME_NAME_LENGTH: usize = 16;

const HEADER_SIZE: usize = 108;
const FRAME_SIZE: usize = 56;
const TAG_SIZE: usize = 112;
const SURFACE_SIZE: usize = 108;
const SHADER_SIZE: usize = 68;
const TRIANGLE_SIZE: usize = 12;
const ST_SIZE: usize = 8;
const XYZ_SIZE: usize = 8;

/// `MD3_XYZ_SCALE`: compressed vertex positions are stored in 1/64ths.
const MD3_XYZ_SCALE: f32 = 1.0 / 64.0;

/// `FUNCTABLE_SIZE` from `tr_local.h`.
const FUNCTABLE_SIZE: usize = 1024;
const FUNCTABLE_MASK: usize = FUNCTABLE_SIZE - 1;

/// Why an MD3 file could not be loaded, standing in for the warnings and
/// `ERR_DROP` calls in `R_LoadMD3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelError {
    /// Shorter than the header.
    TooShort,
    /// The ident is not `IDP3`.
    BadIdent,
    /// The version is not [`MD3_VERSION`].
    BadVersion(i32),
    /// `numFrames` is zero, which `R_LoadMD3` refuses.
    NoFrames,
    /// An offset or a chunk runs past the end of the file.
    BadOffset(&'static str),
    /// A count exceeds the `MD3_MAX_*` limit.
    LimitExceeded(&'static str),
    /// Surfaces did not chain to the end of the file.
    BadSurfaceChain,
    /// A triangle index points past the surface's vertices. Stricter than
    /// `R_LoadMD3`, which leaves this for the draw path to trip over.
    BadIndex(&'static str),
}

/// `md3Frame_t`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Md3Frame {
    pub bounds_min: Vec3,
    pub bounds_max: Vec3,
    pub local_origin: Vec3,
    pub radius: f32,
    pub name: String,
}

/// `md3Tag_t`: an attachment point in one frame of one model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Md3Tag {
    pub name: String,
    pub origin: Vec3,
    pub axis: [Vec3; 3],
}

/// `md3Shader_t` as stored in a surface: a name plus the index the engine
/// fills in when it registers the model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Md3Shader {
    pub name: String,
    pub shader_index: i32,
}

/// `md3Surface_t` plus its geometry, decoded at load: triangles and
/// texture coordinates are flat, while the compressed vertex positions
/// and normals stay as the file stored them so frames can be lerped on
/// demand without keeping a float copy of every frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Md3Surface {
    pub name: String,
    pub flags: i32,
    pub num_frames: usize,
    pub num_verts: usize,
    pub num_triangles: usize,
    pub shaders: Vec<Md3Shader>,
    /// `numTriangles` index triples, each index below `num_verts`.
    pub triangles: Vec<[u32; 3]>,
    /// One `[s, t]` pair per vertex.
    pub tex_coords: Vec<[f32; 2]>,
    /// The raw `md3XyzNormal_t` chunk: `num_frames * num_verts * 8`
    /// bytes of three little-endian `i16` positions plus a packed normal.
    xyz_normals: Vec<u8>,
}

/// One vertex as `LerpMeshVertexes` and `RB_SurfaceMesh` hand it to the
/// tessellator: the lerped position, the decoded normal, and the
/// texcoords the draw path copies alongside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshVertex {
    pub position: Vec3,
    pub normal: Vec3,
    pub tex_coords: [f32; 2],
}

/// A loaded MD3 model: header fields plus the decoded frames, tags and
/// surface metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Md3Model {
    pub name: String,
    pub flags: i32,
    pub frames: Vec<Md3Frame>,
    /// `numFrames * numTags` tags, in file order: the tags of frame 0
    /// first, then frame 1 and so on.
    pub tags: Vec<Md3Tag>,
    pub surfaces: Vec<Md3Surface>,
}

impl Md3Model {
    /// Parses an MD3 file from memory, validating the header, the limits
    /// and every chunk this stage reads.
    pub fn load(data: &[u8]) -> Result<Md3Model, ModelError> {
        if data.len() < HEADER_SIZE {
            return Err(ModelError::TooShort);
        }

        let ident = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if ident != MD3_IDENT {
            return Err(ModelError::BadIdent);
        }
        let version = i32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != MD3_VERSION {
            return Err(ModelError::BadVersion(version));
        }

        // md3Header_t: ident(4) version(4) name[64], then the integers.
        let num_frames = read_u32(data, 76) as usize;
        let num_tags = read_u32(data, 80) as usize;
        let num_surfaces = read_u32(data, 84) as usize;
        let ofs_frames = read_u32(data, 92) as usize;
        let ofs_tags = read_u32(data, 96) as usize;
        let ofs_surfaces = read_u32(data, 100) as usize;
        let ofs_end = read_u32(data, 104) as usize;

        if num_frames < 1 {
            return Err(ModelError::NoFrames);
        }
        if num_frames > MD3_MAX_FRAMES {
            return Err(ModelError::LimitExceeded("MD3_MAX_FRAMES"));
        }
        if num_tags > MD3_MAX_TAGS {
            return Err(ModelError::LimitExceeded("MD3_MAX_TAGS"));
        }
        if num_surfaces > MD3_MAX_SURFACES {
            return Err(ModelError::LimitExceeded("MD3_MAX_SURFACES"));
        }

        // Everything this stage reads must live inside the file, and inside
        // the span the header says the model occupies.
        let span = ofs_end.min(data.len());
        if ofs_end > data.len() {
            return Err(ModelError::BadOffset("ofsEnd"));
        }

        let frames = read_frames(data, ofs_frames, num_frames, span)?;
        let tags = read_tags(data, ofs_tags, num_frames, num_tags, span)?;
        let surfaces = read_surfaces(data, ofs_surfaces, num_surfaces, span)?;

        Ok(Md3Model {
            name: read_cstring(&data[8..72]),
            flags: i32::from_le_bytes(data[72..76].try_into().unwrap()),
            frames,
            tags,
            surfaces,
        })
    }

    /// Tags stored for one frame, in file order.
    pub fn frame_tags(&self, frame: usize) -> &[Md3Tag] {
        let per_frame = self.tags_per_frame();
        if per_frame == 0 {
            return &[];
        }
        let frame = frame.min(self.frames.len() - 1) * per_frame;
        let end = (frame + per_frame).min(self.tags.len());
        &self.tags[frame..end]
    }

    /// Number of tags each frame carries.
    pub fn tags_per_frame(&self) -> usize {
        if self.frames.is_empty() {
            0
        } else {
            self.tags.len() / self.frames.len()
        }
    }

    /// `R_GetTag`: finds the tag named `name` in `frame`, matching the name
    /// case-sensitively as `strcmp` does. A frame past the end of the model
    /// is clamped to the last frame, which `R_GetTag` does so a model
    /// change mid-animation cannot error out.
    pub fn tag(&self, frame: usize, name: &str) -> Option<&Md3Tag> {
        let frame = frame.min(self.frames.len() - 1);
        self.frame_tags(frame).iter().find(|tag| tag.name == name)
    }

    /// `R_LerpTag`: interpolates a tag between two frames and normalizes
    /// each axis row. Returns `None` when the tag does not exist, the case
    /// where the C code clears the orientation and returns `qfalse`.
    pub fn lerp_tag(
        &self,
        start_frame: i32,
        end_frame: i32,
        frac: f32,
        name: &str,
    ) -> Option<Orientation> {
        let start = self.tag(start_frame.max(0) as usize, name)?;
        let end = self.tag(end_frame.max(0) as usize, name)?;

        let front = frac;
        let back = 1.0 - frac;

        let mut orientation = Orientation::default();
        for i in 0..3 {
            orientation.origin[i] = start.origin[i] * back + end.origin[i] * front;
            for row in 0..3 {
                orientation.axis[row][i] = start.axis[row][i] * back + end.axis[row][i] * front;
            }
        }
        for row in 0..3 {
            normalize(&mut orientation.axis[row]);
        }
        Some(orientation)
    }

    /// `R_ModelBounds`: the bounds recorded in the first frame.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        let frame = &self.frames[0];
        (frame.bounds_min, frame.bounds_max)
    }
}

impl Md3Surface {
    /// The raw compressed vertex chunk: `num_frames * num_verts * 8`
    /// bytes of three little-endian `i16` positions plus a packed normal,
    /// kept exactly as the C loader keeps them in the hunk.
    pub fn compressed_vertices(&self) -> &[u8] {
        &self.xyz_normals
    }

    /// `LerpMeshVertexes`: the vertices of `frame` pulled towards
    /// `old_frame` by `backlerp`, paired with the texcoords
    /// `RB_SurfaceMesh` copies next to them. As in C, equal frames force
    /// `backlerp` to zero, frames past the end clamp to the last one, and
    /// the decoded normals are only renormalized when a real lerp
    /// happened (`VectorArrayNormalize` runs solely in that branch).
    pub fn lerp_vertices(&self, frame: usize, old_frame: usize, backlerp: f32) -> Vec<MeshVertex> {
        let stride = self.num_frames * self.num_verts * XYZ_SIZE;
        if self.num_verts == 0 || self.num_frames == 0 || self.xyz_normals.len() < stride {
            return Vec::new();
        }

        let frame = frame.min(self.num_frames - 1);
        let old_frame = old_frame.min(self.num_frames - 1);
        let backlerp = if frame == old_frame { 0.0 } else { backlerp };
        let front = 1.0 - backlerp;

        let mut vertices = Vec::with_capacity(self.num_verts);
        for vert in 0..self.num_verts {
            let new_at = (frame * self.num_verts + vert) * XYZ_SIZE;
            let old_at = (old_frame * self.num_verts + vert) * XYZ_SIZE;

            let mut position = [0.0f32; 3];
            for axis in 0..3 {
                let new = read_i16(&self.xyz_normals, new_at + axis * 2) as f32;
                let old = read_i16(&self.xyz_normals, old_at + axis * 2) as f32;
                position[axis] = (old * backlerp + new * front) * MD3_XYZ_SCALE;
            }

            let new_normal = decode_normal(read_i16(&self.xyz_normals, new_at + 6));
            let normal = if backlerp == 0.0 {
                new_normal
            } else {
                let old_normal = decode_normal(read_i16(&self.xyz_normals, old_at + 6));
                let mut lerped = [0.0f32; 3];
                for axis in 0..3 {
                    lerped[axis] = old_normal[axis] * backlerp + new_normal[axis] * front;
                }
                normalize(&mut lerped);
                lerped
            };

            vertices.push(MeshVertex {
                position,
                normal,
                tex_coords: self.tex_coords[vert],
            });
        }
        vertices
    }
}

fn read_frames(
    data: &[u8],
    ofs: usize,
    count: usize,
    span: usize,
) -> Result<Vec<Md3Frame>, ModelError> {
    let size = count
        .checked_mul(FRAME_SIZE)
        .ok_or(ModelError::BadOffset("ofsFrames"))?;
    let end = ofs
        .checked_add(size)
        .ok_or(ModelError::BadOffset("ofsFrames"))?;
    if end > span {
        return Err(ModelError::BadOffset("ofsFrames"));
    }

    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let at = ofs + i * FRAME_SIZE;
        frames.push(Md3Frame {
            bounds_min: read_vec3(data, at),
            bounds_max: read_vec3(data, at + 12),
            local_origin: read_vec3(data, at + 24),
            radius: read_f32(data, at + 36),
            name: read_cstring(&data[at + 40..at + FRAME_SIZE]),
        });
    }
    Ok(frames)
}

fn read_tags(
    data: &[u8],
    ofs: usize,
    num_frames: usize,
    num_tags: usize,
    span: usize,
) -> Result<Vec<Md3Tag>, ModelError> {
    let size = num_frames
        .checked_mul(num_tags)
        .and_then(|n| n.checked_mul(TAG_SIZE))
        .ok_or(ModelError::BadOffset("ofsTags"))?;
    let end = ofs
        .checked_add(size)
        .ok_or(ModelError::BadOffset("ofsTags"))?;
    if end > span {
        return Err(ModelError::BadOffset("ofsTags"));
    }

    let mut tags = Vec::with_capacity(num_frames * num_tags);
    for i in 0..(num_frames * num_tags) {
        let at = ofs + i * TAG_SIZE;
        tags.push(Md3Tag {
            name: read_cstring(&data[at..at + MD3_NAME_LENGTH]),
            origin: read_vec3(data, at + 64),
            axis: [
                read_vec3(data, at + 76),
                read_vec3(data, at + 88),
                read_vec3(data, at + 100),
            ],
        });
    }
    Ok(tags)
}

fn read_surfaces(
    data: &[u8],
    ofs: usize,
    count: usize,
    span: usize,
) -> Result<Vec<Md3Surface>, ModelError> {
    let mut surfaces = Vec::with_capacity(count);
    let mut pos = ofs;

    for _ in 0..count {
        if pos.checked_add(SURFACE_SIZE).map_or(true, |end| end > span) {
            return Err(ModelError::BadOffset("ofsSurfaces"));
        }

        // md3Surface_t: ident(4) name[64], flags at 68, then the rest.
        let num_frames = read_u32(data, pos + 72) as usize;
        let num_shaders = read_u32(data, pos + 76) as usize;
        let num_verts = read_u32(data, pos + 80) as usize;
        let num_triangles = read_u32(data, pos + 84) as usize;
        let ofs_triangles = read_u32(data, pos + 88) as usize;
        let ofs_shaders = read_u32(data, pos + 92) as usize;
        let ofs_st = read_u32(data, pos + 96) as usize;
        let ofs_xyz_normals = read_u32(data, pos + 100) as usize;
        let ofs_end = read_u32(data, pos + 104) as usize;

        if num_shaders > MD3_MAX_SHADERS {
            return Err(ModelError::LimitExceeded("MD3_MAX_SHADERS"));
        }
        if num_verts > MD3_MAX_VERTS {
            return Err(ModelError::LimitExceeded("MD3_MAX_VERTS"));
        }
        if num_triangles > MD3_MAX_TRIANGLES {
            return Err(ModelError::LimitExceeded("MD3_MAX_TRIANGLES"));
        }
        if ofs_end == 0 {
            return Err(ModelError::BadSurfaceChain);
        }

        let shader_start = pos
            .checked_add(ofs_shaders)
            .ok_or(ModelError::BadOffset("ofsShaders"))?;
        let shader_end = shader_start
            .checked_add(
                num_shaders
                    .checked_mul(SHADER_SIZE)
                    .ok_or(ModelError::BadOffset("ofsShaders"))?,
            )
            .ok_or(ModelError::BadOffset("ofsShaders"))?;
        if shader_end > span {
            return Err(ModelError::BadOffset("ofsShaders"));
        }

        let mut shaders = Vec::with_capacity(num_shaders);
        for i in 0..num_shaders {
            let at = shader_start + i * SHADER_SIZE;
            shaders.push(Md3Shader {
                name: read_cstring(&data[at..at + MD3_NAME_LENGTH]),
                shader_index: i32::from_le_bytes(
                    data[at + MD3_NAME_LENGTH..at + SHADER_SIZE]
                        .try_into()
                        .unwrap(),
                ),
            });
        }

        // The three geometry chunks must sit inside the model's span.
        let chunk_start =
            |ofs: usize, bytes: usize, what: &'static str| -> Result<usize, ModelError> {
                let start = pos.checked_add(ofs).ok_or(ModelError::BadOffset(what))?;
                let end = start
                    .checked_add(bytes)
                    .ok_or(ModelError::BadOffset(what))?;
                if end > span {
                    return Err(ModelError::BadOffset(what));
                }
                Ok(start)
            };

        let triangles_start = chunk_start(
            ofs_triangles,
            num_triangles
                .checked_mul(TRIANGLE_SIZE)
                .ok_or(ModelError::BadOffset("ofsTriangles"))?,
            "ofsTriangles",
        )?;
        let st_start = chunk_start(
            ofs_st,
            num_verts
                .checked_mul(ST_SIZE)
                .ok_or(ModelError::BadOffset("ofsSt"))?,
            "ofsSt",
        )?;
        let xyz_bytes = num_frames
            .checked_mul(num_verts)
            .and_then(|n| n.checked_mul(XYZ_SIZE))
            .ok_or(ModelError::BadOffset("ofsXyzNormals"))?;
        let xyz_start = chunk_start(ofs_xyz_normals, xyz_bytes, "ofsXyzNormals")?;

        let mut triangles = Vec::with_capacity(num_triangles);
        for i in 0..num_triangles {
            let at = triangles_start + i * TRIANGLE_SIZE;
            let index = [
                read_u32(data, at),
                read_u32(data, at + 4),
                read_u32(data, at + 8),
            ];
            // R_LoadMD3 never checks this: a wild index would read past
            // the vertex array at draw time instead of failing here.
            if index.iter().any(|&idx| idx as usize >= num_verts) {
                return Err(ModelError::BadIndex("triangles"));
            }
            triangles.push(index);
        }

        let mut tex_coords = Vec::with_capacity(num_verts);
        for i in 0..num_verts {
            let at = st_start + i * ST_SIZE;
            tex_coords.push([read_f32(data, at), read_f32(data, at + 4)]);
        }

        let xyz_normals = data[xyz_start..xyz_start + xyz_bytes].to_vec();

        surfaces.push(Md3Surface {
            name: read_cstring(&data[pos + 4..pos + 4 + MD3_NAME_LENGTH]),
            flags: i32::from_le_bytes(data[pos + 68..pos + 72].try_into().unwrap()),
            num_frames,
            num_verts,
            num_triangles,
            shaders,
            triangles,
            tex_coords,
            xyz_normals,
        });

        let next = pos
            .checked_add(ofs_end)
            .ok_or(ModelError::BadSurfaceChain)?;
        if next > span {
            return Err(ModelError::BadSurfaceChain);
        }
        pos = next;
    }

    Ok(surfaces)
}

/// `VectorNormalize` from `q_shared.h`: a zero vector is left alone.
fn normalize(v: &mut Vec3) {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 0.0 {
        let inv = 1.0 / len;
        v[0] *= inv;
        v[1] *= inv;
        v[2] *= inv;
    }
}

/// `tr.sinTable[i] = sin( DEG2RAD( i * 360.0f / (FUNCTABLE_SIZE - 1) ) )`.
/// The lat and lng indexes are at most `255 * (FUNCTABLE_SIZE/256) =
/// 1020`, so the unmasked C lookups stay in range; masking makes that
/// invariant explicit.
fn sin_table(index: usize) -> f32 {
    let degrees = (index & FUNCTABLE_MASK) as f32 * 360.0 / (FUNCTABLE_SIZE as f32 - 1.0);
    degrees.to_radians().sin()
}

/// The packed-normal decode from `LerpMeshVertexes`: the high byte is
/// `lat`, the low byte `lng`, giving `x = cos(lat) * sin(lng)`,
/// `y = sin(lat) * sin(lng)`, `z = cos(lng)` through the engine's table.
fn decode_normal(packed: i16) -> Vec3 {
    let bits = packed as u16;
    let lat = (bits >> 8) as usize * (FUNCTABLE_SIZE / 256);
    let lng = (bits & 0xff) as usize * (FUNCTABLE_SIZE / 256);
    [
        sin_table((lat + FUNCTABLE_SIZE / 4) & FUNCTABLE_MASK) * sin_table(lng),
        sin_table(lat) * sin_table(lng),
        sin_table((lng + FUNCTABLE_SIZE / 4) & FUNCTABLE_MASK),
    ]
}

fn read_f32(data: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn read_i16(data: &[u8], at: usize) -> i16 {
    i16::from_le_bytes(data[at..at + 2].try_into().unwrap())
}

fn read_vec3(data: &[u8], at: usize) -> Vec3 {
    [
        read_f32(data, at),
        read_f32(data, at + 4),
        read_f32(data, at + 8),
    ]
}

/// Reads a fixed-width, NUL-terminated string, as `Q_strncpyz` would give.
fn read_cstring(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(data: &mut [u8], at: usize, v: u32) {
        data[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_i32(data: &mut [u8], at: usize, v: i32) {
        data[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_f32(data: &mut [u8], at: usize, v: f32) {
        data[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_i16(data: &mut [u8], at: usize, v: i16) {
        data[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put_name(data: &mut [u8], at: usize, width: usize, name: &str) {
        data[at..at + width].fill(0);
        let bytes = name.as_bytes();
        let len = bytes.len().min(width);
        data[at..at + len].copy_from_slice(&bytes[..len]);
    }

    /// Builds a small but structurally complete MD3: two frames, one tag
    /// per frame, and one surface with one shader, one triangle and three
    /// vertices whose positions and packed normals differ per frame.
    /// Frame `f` puts its tag
    /// origin at `(f * 10, 0, 0)` and scales the axis rows by `1 + f`, so
    /// interpolation and normalization are both observable.
    fn build_md3(tag_name: &str) -> Vec<u8> {
        let num_frames = 2;
        let num_tags = 1;
        let num_shaders = 1;
        let num_verts = 3;
        let num_triangles = 1;

        let ofs_frames = HEADER_SIZE;
        let ofs_tags = ofs_frames + num_frames * FRAME_SIZE;
        let ofs_surfaces = ofs_tags + num_frames * num_tags * TAG_SIZE;
        // The surface carries real geometry, in the chunk order qfiles.h
        // documents: shader, triangles, texcoords, compressed vertices.
        let ofs_shaders = SURFACE_SIZE;
        let ofs_triangles = ofs_shaders + num_shaders * SHADER_SIZE;
        let ofs_st = ofs_triangles + num_triangles * TRIANGLE_SIZE;
        let ofs_xyz = ofs_st + num_verts * ST_SIZE;
        let surface_ofs_end = ofs_xyz + num_frames * num_verts * XYZ_SIZE;
        let ofs_eof = ofs_surfaces + surface_ofs_end;

        let mut data = vec![0u8; ofs_eof];

        // header
        put_u32(&mut data, 0, MD3_IDENT);
        put_i32(&mut data, 4, MD3_VERSION);
        put_name(&mut data, 8, 64, "models/test");
        put_i32(&mut data, 72, 7); // flags
        put_i32(&mut data, 76, num_frames as i32);
        put_i32(&mut data, 80, num_tags as i32);
        put_i32(&mut data, 84, 1); // numSurfaces
        put_i32(&mut data, 88, 0); // numSkins
        put_i32(&mut data, 92, ofs_frames as i32);
        put_i32(&mut data, 96, ofs_tags as i32);
        put_i32(&mut data, 100, ofs_surfaces as i32);
        put_i32(&mut data, 104, ofs_eof as i32);

        // frames
        for frame in 0..num_frames {
            let at = ofs_frames + frame * FRAME_SIZE;
            let f = frame as f32;
            for axis in 0..3 {
                put_f32(&mut data, at + axis * 4, -10.0 - f + axis as f32);
                put_f32(&mut data, at + 12 + axis * 4, 10.0 + f + axis as f32);
                put_f32(&mut data, at + 24 + axis * 4, f * 3.0);
            }
            put_f32(&mut data, at + 36, 20.0 + f);
            put_name(
                &mut data,
                at + 40,
                MD3_FRAME_NAME_LENGTH,
                &format!("frame{frame}"),
            );
        }

        // tags: one per frame
        for frame in 0..num_frames {
            let at = ofs_tags + frame * TAG_SIZE;
            put_name(&mut data, at, MD3_NAME_LENGTH, tag_name);
            put_f32(&mut data, at + 64, frame as f32 * 10.0);
            // axis rows scaled by (1 + frame), identity otherwise
            let scale = 1.0 + frame as f32;
            for row in 0..3 {
                for col in 0..3 {
                    let value = if row == col { scale } else { 0.0 };
                    put_f32(&mut data, at + 76 + row * 12 + col * 4, value);
                }
            }
        }

        // one surface with one shader and one triangle over three vertices
        let surf = ofs_surfaces;
        put_i32(&mut data, surf, 0x33504449 as i32); // surface ident, IDP3
        put_name(&mut data, surf + 4, 64, "body");
        put_i32(&mut data, surf + 72, num_frames as i32);
        put_i32(&mut data, surf + 76, num_shaders as i32);
        put_i32(&mut data, surf + 80, num_verts as i32);
        put_i32(&mut data, surf + 84, num_triangles as i32);
        put_i32(&mut data, surf + 88, ofs_triangles as i32);
        put_i32(&mut data, surf + 92, ofs_shaders as i32);
        put_i32(&mut data, surf + 96, ofs_st as i32);
        put_i32(&mut data, surf + 100, ofs_xyz as i32);
        put_i32(&mut data, surf + 104, surface_ofs_end as i32);

        put_name(&mut data, surf + ofs_shaders, 64, "models/test/body");
        put_i32(&mut data, surf + ofs_shaders + 64, 3);

        put_i32(&mut data, surf + ofs_triangles, 0);
        put_i32(&mut data, surf + ofs_triangles + 4, 1);
        put_i32(&mut data, surf + ofs_triangles + 8, 2);

        for vert in 0..num_verts {
            let at = surf + ofs_st + vert * ST_SIZE;
            put_f32(&mut data, at, vert as f32 * 0.25);
            put_f32(&mut data, at + 4, 1.0 - vert as f32 * 0.25);
        }

        // Frame `f` stores vertex `v` at x = 64*(v+1)*(f+1), y = x/2,
        // z = 0; frame 0's packed normals point up (+Z), frame 1's
        // (lat = lng = 64) point along +Y.
        for frame in 0..num_frames {
            for vert in 0..num_verts {
                let at = surf + ofs_xyz + (frame * num_verts + vert) * XYZ_SIZE;
                let scale = 64.0 * (frame + 1) as f32;
                put_i16(&mut data, at, ((vert + 1) as f32 * scale) as i16);
                put_i16(&mut data, at + 2, ((vert + 1) as f32 * scale * 0.5) as i16);
                put_i16(&mut data, at + 4, 0);
                let packed: i16 = if frame == 0 { 0 } else { (64 << 8) | 64 };
                put_i16(&mut data, at + 6, packed);
            }
        }

        data
    }

    #[test]
    fn loads_header_frames_tags_and_surfaces() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();

        assert_eq!(model.name, "models/test");
        assert_eq!(model.flags, 7);
        assert_eq!(model.frames.len(), 2);
        assert_eq!(model.tags.len(), 2);
        assert_eq!(model.tags_per_frame(), 1);

        assert_eq!(model.frames[0].name, "frame0");
        assert_eq!(model.frames[1].name, "frame1");
        assert_eq!(model.frames[0].radius, 20.0);
        assert_eq!(model.frames[1].radius, 21.0);
        assert_eq!(model.frames[0].local_origin, [0.0, 0.0, 0.0]);
        assert_eq!(model.frames[1].local_origin, [3.0, 3.0, 3.0]);

        assert_eq!(model.surfaces.len(), 1);
        let surface = &model.surfaces[0];
        assert_eq!(surface.name, "body");
        assert_eq!(surface.num_verts, 3);
        assert_eq!(surface.num_triangles, 1);
        assert_eq!(surface.shaders.len(), 1);
        assert_eq!(surface.shaders[0].name, "models/test/body");
        assert_eq!(surface.shaders[0].shader_index, 3);
    }

    #[test]
    fn bounds_come_from_the_first_frame() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();
        let (mins, maxs) = model.bounds();
        assert_eq!(mins, [-10.0, -9.0, -8.0]);
        assert_eq!(maxs, [10.0, 11.0, 12.0]);
    }

    #[test]
    fn tags_are_looked_up_per_frame_and_clamped() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();

        assert_eq!(model.tag(0, "tag_head").unwrap().origin, [0.0, 0.0, 0.0]);
        assert_eq!(model.tag(1, "tag_head").unwrap().origin, [10.0, 0.0, 0.0]);
        // Past the end clamps to the last frame, as R_GetTag does.
        assert_eq!(model.tag(99, "tag_head").unwrap().origin, [10.0, 0.0, 0.0]);

        // R_GetTag compares with strcmp, so the name must match exactly.
        assert!(model.tag(0, "TAG_HEAD").is_none());
        assert!(model.tag(0, "tag_body").is_none());
    }

    #[test]
    fn lerp_tag_interpolates_the_origin() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();

        let start = model.lerp_tag(0, 1, 0.0, "tag_head").unwrap();
        assert_eq!(start.origin, [0.0, 0.0, 0.0]);

        let middle = model.lerp_tag(0, 1, 0.5, "tag_head").unwrap();
        assert_eq!(middle.origin, [5.0, 0.0, 0.0]);

        let end = model.lerp_tag(0, 1, 1.0, "tag_head").unwrap();
        assert_eq!(end.origin, [10.0, 0.0, 0.0]);
    }

    #[test]
    fn lerp_tag_normalizes_each_axis_row() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();

        // Frame 0 holds the identity, frame 1 the identity scaled by two,
        // so any interpolation is a multiple of the identity until the
        // normalization in R_LerpTag runs.
        for frac in [0.0, 0.25, 0.5, 1.0] {
            let orientation = model.lerp_tag(0, 1, frac, "tag_head").unwrap();
            for row in 0..3 {
                let length = orientation.axis[row]
                    .iter()
                    .map(|v| v * v)
                    .sum::<f32>()
                    .sqrt();
                assert!((length - 1.0).abs() < 1e-6, "row {row} at {frac}");
            }
            assert_eq!(
                orientation.axis,
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
            );
        }
    }

    #[test]
    fn lerp_tag_of_a_missing_tag_is_none() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();
        // The C code clears the orientation and returns qfalse here.
        assert_eq!(model.lerp_tag(0, 1, 0.5, "tag_weapon"), None);
    }

    #[test]
    fn bad_ident_is_rejected() {
        let mut data = build_md3("tag_head");
        data[0..4].copy_from_slice(b"NOPE");
        assert_eq!(Md3Model::load(&data), Err(ModelError::BadIdent));
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut data = build_md3("tag_head");
        put_i32(&mut data, 4, 14);
        assert_eq!(Md3Model::load(&data), Err(ModelError::BadVersion(14)));
    }

    #[test]
    fn a_model_without_frames_is_rejected() {
        let mut data = build_md3("tag_head");
        put_i32(&mut data, 76, 0);
        assert_eq!(Md3Model::load(&data), Err(ModelError::NoFrames));
    }

    #[test]
    fn truncated_file_is_rejected() {
        let data = build_md3("tag_head");
        assert_eq!(
            Md3Model::load(&data[..HEADER_SIZE - 1]),
            Err(ModelError::TooShort)
        );

        // A tag offset that runs past the end of the file.
        let mut data = data;
        put_i32(&mut data, 96, 0x00ff_ffffu32 as i32);
        assert_eq!(Md3Model::load(&data), Err(ModelError::BadOffset("ofsTags")));
    }

    #[test]
    fn a_tag_count_past_the_limit_is_rejected() {
        let mut data = build_md3("tag_head");
        put_i32(&mut data, 80, (MD3_MAX_TAGS + 1) as i32);
        assert_eq!(
            Md3Model::load(&data),
            Err(ModelError::LimitExceeded("MD3_MAX_TAGS"))
        );

        let mut data = build_md3("tag_head");
        put_i32(&mut data, 84, (MD3_MAX_SURFACES + 1) as i32);
        assert_eq!(
            Md3Model::load(&data),
            Err(ModelError::LimitExceeded("MD3_MAX_SURFACES"))
        );
    }

    #[test]
    fn a_surface_that_does_not_chain_is_rejected() {
        let mut data = build_md3("tag_head");
        let ofs_surfaces = 444; // 108 header + 2*56 frames + 2*112 tags
                                // A zero ofsEnd would loop forever in the C walk; it is refused.
        put_i32(&mut data, ofs_surfaces + 104, 0);
        assert_eq!(Md3Model::load(&data), Err(ModelError::BadSurfaceChain));
    }

    #[test]
    fn geometry_decodes_triangles_tex_coords_and_positions() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();
        let surface = &model.surfaces[0];

        assert_eq!(surface.triangles, vec![[0, 1, 2]]);
        assert_eq!(
            surface.tex_coords,
            vec![[0.0, 1.0], [0.25, 0.75], [0.5, 0.5]]
        );
        assert_eq!(surface.compressed_vertices().len(), 2 * 3 * XYZ_SIZE);

        let frame0 = surface.lerp_vertices(0, 0, 0.0);
        assert_eq!(frame0.len(), 3);
        // The raw shorts are 64*(vert+1)*(frame+1) on x and half that on
        // y; MD3_XYZ_SCALE brings frame 0 back to 1, 2, 3 on x.
        assert_eq!(frame0[0].position, [1.0, 0.5, 0.0]);
        assert_eq!(frame0[2].position, [3.0, 1.5, 0.0]);
        assert_eq!(frame0[1].tex_coords, [0.25, 0.75]);

        // The packed normal 0 (lat = lng = 0) decodes to +Z.
        assert!(frame0[0].normal[0].abs() < 1e-4);
        assert!(frame0[0].normal[1].abs() < 1e-4);
        assert!((frame0[0].normal[2] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn lerp_vertices_interpolates_positions_and_normals() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();
        let surface = &model.surfaces[0];

        let lerped = surface.lerp_vertices(1, 0, 0.5);
        // The positions are dyadic, so the halfway point is exact.
        assert_eq!(lerped[0].position, [1.5, 0.75, 0.0]);
        assert_eq!(lerped[1].position, [3.0, 1.5, 0.0]);
        assert_eq!(lerped[2].position, [4.5, 2.25, 0.0]);

        // +Z lerped halfway with +Y, then renormalized exactly as
        // VectorArrayNormalize does in the lerping branch.
        assert!(lerped[0].normal[0].abs() < 0.01);
        assert!(
            (lerped[0].normal[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "{}",
            lerped[0].normal[1]
        );
        assert!(
            (lerped[0].normal[2] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "{}",
            lerped[0].normal[2]
        );
    }

    #[test]
    fn matching_frames_force_a_zero_backlerp() {
        let model = Md3Model::load(&build_md3("tag_head")).unwrap();
        let surface = &model.surfaces[0];

        // RB_SurfaceMesh zeroes backlerp when oldframe == frame, so a
        // stale backlerp value cannot move the vertices.
        let forced = surface.lerp_vertices(1, 1, 0.75);
        assert_eq!(forced, surface.lerp_vertices(1, 1, 0.0));
        assert_eq!(forced[0].position, [2.0, 1.0, 0.0]);
        // Frames past the end clamp to the last one and clamp together.
        assert_eq!(surface.lerp_vertices(9, 9, 0.5), forced);
    }

    #[test]
    fn packed_normals_decode_through_the_engine_sin_table() {
        // The table is sin(i * 360 / (1023) degrees), so the decode is
        // only exact up to the table's own rounding; 0.01 covers that.
        let up = decode_normal(0);
        assert!(up[0].abs() < 1e-4 && up[1].abs() < 1e-4);
        assert!((up[2] - 1.0).abs() < 1e-4);

        // A quarter turn of lat with a quarter turn of lng lands on +Y.
        let side = decode_normal((64 << 8) | 64);
        assert!((side[1] - 1.0).abs() < 0.01);
        assert!(side[0].abs() < 0.01 && side[2].abs() < 0.01);

        // Every lat/lng pair decodes to a unit vector.
        for lat in (0..=255u32).step_by(17) {
            for lng in (0..=255u32).step_by(17) {
                let packed = (((lat << 8) | lng) & 0xffff) as i16;
                let normal = decode_normal(packed);
                let len = normal.iter().map(|c| c * c).sum::<f32>().sqrt();
                assert!((len - 1.0).abs() < 0.01, "lat={lat} lng={lng} len={len}");
            }
        }
    }

    #[test]
    fn an_out_of_range_geometry_offset_is_rejected() {
        let mut data = build_md3("tag_head");
        let surf = 444; // 108 header + 2*56 frames + 2*112 tags
        put_i32(&mut data, surf + 96, 0x00ff_ffffu32 as i32); // ofsSt
        assert_eq!(Md3Model::load(&data), Err(ModelError::BadOffset("ofsSt")));
    }

    #[test]
    fn a_triangle_index_past_the_vertices_is_rejected() {
        let mut data = build_md3("tag_head");
        let surf = 444;
        let ofs_triangles =
            i32::from_le_bytes(data[surf + 88..surf + 92].try_into().unwrap()) as usize;
        // The second index points at a fourth vertex that does not exist;
        // R_LoadMD3 would only find out at draw time.
        put_i32(&mut data, surf + ofs_triangles + 4, 7);
        assert_eq!(
            Md3Model::load(&data),
            Err(ModelError::BadIndex("triangles"))
        );
    }
}
