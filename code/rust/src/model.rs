//! MD3 model container, ported from the model path in
//! `code/renderer/tr_model.c` and `code/qcommon/qfiles.h`.
//!
//! This is the first stage of the renderer's model path: the file is
//! parsed into owned frames, tags and surface metadata, which is what
//! [`crate::refexport::RefExport::lerp_tag`] and
//! [`crate::refexport::RefExport::model_bounds`] need. The geometry chunks
//! (triangles, texture coordinates and compressed vertex normals) are
//! located and bounds-checked but not decoded yet; that arrives with the
//! rendering stage.
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

/// `md3Surface_t` metadata. The geometry chunks behind `ofs_triangles`,
/// `ofs_st` and `ofs_xyz_normals` are validated but not decoded yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Md3Surface {
    pub name: String,
    pub flags: i32,
    pub num_frames: usize,
    pub num_verts: usize,
    pub num_triangles: usize,
    pub shaders: Vec<Md3Shader>,
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
        let ofs_shaders = read_u32(data, pos + 92) as usize;
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

        surfaces.push(Md3Surface {
            name: read_cstring(&data[pos + 4..pos + 4 + MD3_NAME_LENGTH]),
            flags: i32::from_le_bytes(data[pos + 68..pos + 72].try_into().unwrap()),
            num_frames,
            num_verts,
            num_triangles,
            shaders,
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

fn read_f32(data: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
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

    fn put_name(data: &mut [u8], at: usize, width: usize, name: &str) {
        data[at..at + width].fill(0);
        let bytes = name.as_bytes();
        let len = bytes.len().min(width);
        data[at..at + len].copy_from_slice(&bytes[..len]);
    }

    /// Builds a small but structurally complete MD3: two frames, one tag
    /// per frame, and one surface with one shader. Frame `f` puts its tag
    /// origin at `(f * 10, 0, 0)` and scales the axis rows by `1 + f`, so
    /// interpolation and normalization are both observable.
    fn build_md3(tag_name: &str) -> Vec<u8> {
        let num_frames = 2;
        let num_tags = 1;
        let num_shaders = 1;

        let ofs_frames = HEADER_SIZE;
        let ofs_tags = ofs_frames + num_frames * FRAME_SIZE;
        let ofs_surfaces = ofs_tags + num_frames * num_tags * TAG_SIZE;
        let surface_ofs_end = SURFACE_SIZE + num_shaders * SHADER_SIZE;
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

        // one surface with one shader
        let surf = ofs_surfaces;
        put_i32(&mut data, surf, 0x33504449 as i32); // surface ident, IDP3
        put_name(&mut data, surf + 4, 64, "body");
        put_i32(&mut data, surf + 72, num_frames as i32);
        put_i32(&mut data, surf + 76, num_shaders as i32);
        put_i32(&mut data, surf + 80, 100); // numVerts
        put_i32(&mut data, surf + 84, 90); // numTriangles
        put_i32(&mut data, surf + 92, SURFACE_SIZE as i32); // ofsShaders
        put_i32(&mut data, surf + 104, surface_ofs_end as i32); // ofsEnd
        put_name(&mut data, surf + SURFACE_SIZE, 64, "models/test/body");
        put_i32(&mut data, surf + SURFACE_SIZE + 64, 3);

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
        assert_eq!(surface.num_verts, 100);
        assert_eq!(surface.num_triangles, 90);
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
}
