//! The Q3 BSP header and directory, from `bspfile.h`, and the tokenizer the
//! game's entity lump is read with.
//!
//! The game needs the entity lump at `GAME_INIT` and the submodels for every
//! `*N` inline model's bounds; collision walks the planes, nodes, leafs and
//! brushes. The render lumps belong to parts of the engine that are not here.
//! Reading the directory is also the cheapest check that a file really is a
//! version 46 BSP rather than something that happens to be named `.bsp`.
//!
//! `next_token` is `COM_Parse` from `q_shared.c`: quoted strings come back
//! without their quotes, `//` and `/* */` comments are skipped, and a word ends
//! at the first byte at or below a space.

use std::fmt;

/// `"IBSP"` read as a little-endian u32, which is how the file stores it.
pub const IBSP_MAGIC: u32 = 0x5053_4249;
/// `BSP_VERSION` from `qfiles.h`.
pub const BSP_VERSION: u32 = 46;
/// `HEADER_LUMPS`: the directory has one entry per lump.
pub const HEADER_LUMPS: usize = 17;
/// `LUMP_ENTITIES`: the key/value spawn text.
pub const LUMP_ENTITIES: usize = 0;
/// `LUMP_SHADERS`: the surface flags and contents of every shader.
pub const LUMP_SHADERS: usize = 1;
/// `LUMP_PLANES`: the clipping planes.
pub const LUMP_PLANES: usize = 2;
/// `LUMP_NODES`: the BSP tree.
pub const LUMP_NODES: usize = 3;
/// `LUMP_LEAFS`: the leafs, which hold the brush list of each volume.
pub const LUMP_LEAFS: usize = 4;
/// `LUMP_LEAFBRUSHES`: the brush indices each leaf holds.
pub const LUMP_LEAFBRUSHES: usize = 6;
/// `LUMP_MODELS`: the submodels, one per `*N` inline model.
pub const LUMP_MODELS: usize = 7;
/// `LUMP_BRUSHES`: the world's brushes.
pub const LUMP_BRUSHES: usize = 8;
/// `LUMP_BRUSHSIDES`: the planes bounding each brush.
pub const LUMP_BRUSHSIDES: usize = 9;
/// `LUMP_DRAWVERTS`: the shared vertexes the surfaces draw.
pub const LUMP_DRAWVERTS: usize = 10;
/// `LUMP_DRAWINDEXES`: triangle indexes into the vertexes of one surface.
pub const LUMP_DRAWINDEXES: usize = 11;
/// `LUMP_FACES`: one record per drawable surface.
pub const LUMP_FACES: usize = 12;
/// `LUMP_LIGHTMAPS`: 128x128 RGB pages the faces sample.
pub const LUMP_LIGHTMAPS: usize = 13;
/// Width and height of one lightmap page, in texels.
pub const LIGHTMAP_SIZE: usize = 128;
/// `sizeof( dmodel_t )`: mins, maxs, radius, headnode, firstface, numfaces.
const DMODEL_SIZE: usize = 40;
/// `sizeof( dshader_t )`: name[64], surfaceFlags, contentFlags.
const DSHADER_SIZE: usize = 72;
/// `sizeof( drawVert_t )`: xyz, st, lightmap, normal, colour.
const DRAWVERT_SIZE: usize = 44;
/// `sizeof( dface_t )`: shader, fog, type, vertex and index ranges, lightmap.
const DFACE_SIZE: usize = 104;

#[derive(Debug, PartialEq, Eq)]
pub enum BspError {
    /// Shorter than the header and its directory.
    TooShort,
    BadMagic(u32),
    BadVersion(u32),
    /// A lump that runs past the end of the file, or has a negative offset.
    LumpOutOfRange(usize),
    /// A lump whose length is not a whole number of records, which
    /// `CMod_LoadSubmodels` refuses as a "funny lump size".
    BadLumpSize(usize),
}

impl fmt::Display for BspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BspError::TooShort => write!(f, "the file is shorter than a BSP header"),
            BspError::BadMagic(magic) => write!(f, "bad BSP magic 0x{magic:08x}"),
            BspError::BadVersion(version) => write!(f, "BSP version {version}, not {BSP_VERSION}"),
            BspError::LumpOutOfRange(index) => {
                write!(f, "lump {index} runs past the end of the file")
            }
            BspError::BadLumpSize(index) => {
                write!(f, "lump {index} is not a whole number of records")
            }
        }
    }
}

impl std::error::Error for BspError {}

/// One inline model's bounds, from a `dmodel_t`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BspModel {
    pub mins: [f32; 3],
    pub maxs: [f32; 3],
}

/// One entry of `LUMP_SHADERS`: the shader name and the flags q3map baked
/// into the surface, so the renderer can skip what is never drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BspTexture {
    pub name: String,
    pub surface_flags: u32,
}

/// One `dface_t`: a triangle range of the shared vertex array, the shader it
/// draws with, and where its lightmap sits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BspFace {
    pub shader: usize,
    pub surface_type: i32,
    pub first_vert: usize,
    pub num_verts: usize,
    pub first_index: usize,
    pub num_indexes: usize,
    /// `lightmapNum`, or -1 when the face is fullbright.
    pub lightmap: i32,
    pub lightmap_x: i32,
    pub lightmap_y: i32,
    /// Size of the control grid a patch stores in its vertex range; zero on a
    /// triangle surface.
    pub patch_width: i32,
    pub patch_height: i32,
}

/// `SURFACE_PATCH`: a curved surface whose vertex range is a control grid,
/// not a triangle list.
pub const SURFACE_PATCH: i32 = 2;

/// One `drawVert_t`. `st` is the texture coordinate the map was built with
/// and `lightmap` is the texel offset into this face's lightmap rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrawVert {
    pub xyz: [f32; 3],
    pub st: [f32; 2],
    pub lightmap: [f32; 2],
    pub normal: [f32; 3],
}

/// A loaded BSP: the file bytes and the directory that says where each lump
/// sits.
pub struct Bsp {
    bytes: Vec<u8>,
    lumps: [(usize, usize); HEADER_LUMPS],
    /// Where the surfaces and lightmaps were found. The retail maps leave
    /// lump 12 empty and start the surfaces at 13, while the header documents
    /// 12/13, so both layouts are detected from the directory.
    faces_lump: usize,
    lightmaps_lump: usize,
}

impl Bsp {
    /// Reads the header and checks every lump against the file, as `CM_LoadMap`
    /// does before it starts copying lumps out.
    pub fn load(bytes: Vec<u8>) -> Result<Self, BspError> {
        let directory_end = 8 + HEADER_LUMPS * 8;
        if bytes.len() < directory_end {
            return Err(BspError::TooShort);
        }

        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if magic != IBSP_MAGIC {
            return Err(BspError::BadMagic(magic));
        }
        if version != BSP_VERSION {
            return Err(BspError::BadVersion(version));
        }

        let mut lumps = [(0usize, 0usize); HEADER_LUMPS];
        for (index, slot) in lumps.iter_mut().enumerate() {
            let entry = 8 + index * 8;
            let offset = i32::from_le_bytes(bytes[entry..entry + 4].try_into().unwrap()) as i64;
            let length = i32::from_le_bytes(bytes[entry + 4..entry + 8].try_into().unwrap()) as i64;
            let end = offset + length;
            if offset < 0 || length < 0 || end > bytes.len() as i64 {
                return Err(BspError::LumpOutOfRange(index));
            }
            *slot = (offset as usize, length as usize);
        }

        // The retail maps leave lump 12 empty and start the surfaces at 13;
        // the documented layout starts them at 12. Pick whichever one holds
        // whole face records, and the lightmap pages beside it.
        let length = |index: usize| lumps.get(index).map(|&(_, length)| length).unwrap_or(0);
        let faces_lump = [LUMP_FACES, LUMP_FACES + 1]
            .into_iter()
            .find(|&index| length(index) > 0 && length(index) % DFACE_SIZE == 0)
            .unwrap_or(LUMP_FACES);
        let lightmaps_lump = [LUMP_LIGHTMAPS, LUMP_LIGHTMAPS + 1]
            .into_iter()
            .find(|&index| {
                index != faces_lump
                    && length(index) > 0
                    && length(index) % (LIGHTMAP_SIZE * LIGHTMAP_SIZE * 3) == 0
            })
            .unwrap_or(LUMP_LIGHTMAPS);

        Ok(Bsp {
            bytes,
            lumps,
            faces_lump,
            lightmaps_lump,
        })
    }

    /// One lump's bytes. The index is a `LUMP_*` constant.
    pub fn lump(&self, index: usize) -> &[u8] {
        match self.lumps.get(index) {
            Some(&(offset, length)) => &self.bytes[offset..offset + length],
            None => &[],
        }
    }

    /// `CM_EntityString`: the spawn text the game parses through
    /// `G_GET_ENTITY_TOKEN`.
    pub fn entities(&self) -> &[u8] {
        self.lump(LUMP_ENTITIES)
    }

    /// The shader names and surface flags, in face order.
    pub fn textures(&self) -> Result<Vec<BspTexture>, BspError> {
        let lump = self.lump(LUMP_SHADERS);
        if lump.len() % DSHADER_SIZE != 0 {
            return Err(BspError::BadLumpSize(LUMP_SHADERS));
        }
        Ok(lump
            .chunks_exact(DSHADER_SIZE)
            .map(|entry| {
                let end = entry[..64]
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(64);
                BspTexture {
                    name: String::from_utf8_lossy(&entry[..end]).into_owned(),
                    surface_flags: u32::from_le_bytes(entry[64..68].try_into().unwrap()),
                }
            })
            .collect())
    }

    /// The drawable surfaces.
    pub fn faces(&self) -> Result<Vec<BspFace>, BspError> {
        let lump = self.lump(self.faces_lump);
        if lump.len() % DFACE_SIZE != 0 {
            return Err(BspError::BadLumpSize(self.faces_lump));
        }
        Ok(lump
            .chunks_exact(DFACE_SIZE)
            .map(|entry| {
                let field = |index: usize| {
                    i32::from_le_bytes(entry[index * 4..index * 4 + 4].try_into().unwrap())
                };
                BspFace {
                    shader: field(0).max(0) as usize,
                    surface_type: field(2),
                    first_vert: field(3).max(0) as usize,
                    num_verts: field(4).max(0) as usize,
                    first_index: field(5).max(0) as usize,
                    num_indexes: field(6).max(0) as usize,
                    lightmap: field(7),
                    lightmap_x: field(8),
                    lightmap_y: field(9),
                    // Past lightmapWidth/Height, lightmapOrigin and the three
                    // lightmapVecs: `patchWidth` then `patchHeight`.
                    patch_width: field(24).max(0),
                    patch_height: field(25).max(0),
                }
            })
            .collect())
    }

    /// The shared vertex array the faces index into.
    pub fn draw_verts(&self) -> Result<Vec<DrawVert>, BspError> {
        let lump = self.lump(LUMP_DRAWVERTS);
        if lump.len() % DRAWVERT_SIZE != 0 {
            return Err(BspError::BadLumpSize(LUMP_DRAWVERTS));
        }
        Ok(lump
            .chunks_exact(DRAWVERT_SIZE)
            .map(|entry| {
                let float = |at: usize| {
                    f32::from_le_bytes(entry[at..at + 4].try_into().unwrap())
                };
                DrawVert {
                    xyz: [float(0), float(4), float(8)],
                    st: [float(12), float(16)],
                    lightmap: [float(20), float(24)],
                    normal: [float(28), float(32), float(36)],
                }
            })
            .collect())
    }

    /// The triangle indexes, three per triangle.
    pub fn draw_indexes(&self) -> Result<Vec<i32>, BspError> {
        let lump = self.lump(LUMP_DRAWINDEXES);
        if lump.len() % 4 != 0 {
            return Err(BspError::BadLumpSize(LUMP_DRAWINDEXES));
        }
        Ok(lump
            .chunks_exact(4)
            .map(|entry| i32::from_le_bytes(entry.try_into().unwrap()))
            .collect())
    }

    /// The 128x128 RGB lightmap pages, indexed by `BspFace::lightmap`.
    pub fn lightmaps(&self) -> &[u8] {
        self.lump(self.lightmaps_lump)
    }

    /// The submodels, indexed by the `*N` a map gives a brush entity.
    ///
    /// `CMod_LoadSubmodels` spreads every bound outward by one unit, so a
    /// `*0` box is one unit larger on each side than the file says; the
    /// collision bounds the game sees are those spread ones.
    pub fn models(&self) -> Result<Vec<BspModel>, BspError> {
        let lump = self.lump(LUMP_MODELS);
        if lump.len() % DMODEL_SIZE != 0 {
            return Err(BspError::BadLumpSize(LUMP_MODELS));
        }
        Ok(lump
            .chunks_exact(DMODEL_SIZE)
            .map(|entry| {
                let read = |at: usize| {
                    f32::from_le_bytes(entry[at..at + 4].try_into().unwrap())
                };
                BspModel {
                    mins: [read(0) - 1.0, read(4) - 1.0, read(8) - 1.0],
                    maxs: [read(12) + 1.0, read(16) + 1.0, read(20) + 1.0],
                }
            })
            .collect())
    }
}

/// `COM_ParseExt` with line breaks allowed: the next token in `text`, advancing
/// `at` past it. `None` once there is nothing left to parse.
pub fn next_token(text: &[u8], at: &mut usize) -> Option<String> {
    let mut position = *at;

    // `SkipWhitespace`: spaces, then either comment form, repeatedly.
    loop {
        while position < text.len() && text[position] <= b' ' {
            position += 1;
        }
        match text.get(position..position + 2) {
            Some(b"//") => {
                position += 2;
                while position < text.len() && text[position] != b'\n' {
                    position += 1;
                }
            }
            Some(b"/*") => {
                position += 2;
                while position < text.len()
                    && !(text[position] == b'*' && text.get(position + 1) == Some(&b'/'))
                {
                    position += 1;
                }
                if position < text.len() {
                    position += 2;
                }
            }
            _ => break,
        }
    }

    if position >= text.len() {
        *at = position;
        return None;
    }

    // A quoted string ends at the closing quote and may contain spaces.
    if text[position] == b'"' {
        position += 1;
        let mut token = Vec::new();
        while position < text.len() {
            let byte = text[position];
            position += 1;
            if byte == b'"' {
                break;
            }
            token.push(byte);
        }
        *at = position;
        return Some(String::from_utf8_lossy(&token).into_owned());
    }

    // A word ends at the first byte at or below a space, exactly as the C loop
    // `while ( c > ' ' )` does.
    let start = position;
    while position < text.len() && text[position] > b' ' {
        position += 1;
    }
    *at = position;
    Some(String::from_utf8_lossy(&text[start..position]).into_owned())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A BSP with only an entity lump, laid out like the real thing: header,
    /// 17 directory entries, then the lump.
    pub fn build_bsp(entities: &[u8]) -> Vec<u8> {
        build_bsp_with(&[(LUMP_ENTITIES, entities)])
    }

    /// A BSP carrying the given lumps, each placed after the directory.
    pub fn build_bsp_with(lumps: &[(usize, &[u8])]) -> Vec<u8> {
        let directory_end = 8 + HEADER_LUMPS * 8;
        let mut entries = [(0i32, 0i32); HEADER_LUMPS];
        let mut body = Vec::new();
        for (index, data) in lumps {
            entries[*index] = ((directory_end + body.len()) as i32, data.len() as i32);
            body.extend_from_slice(data);
        }

        let mut bytes = Vec::with_capacity(directory_end + body.len());
        bytes.extend_from_slice(&IBSP_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&BSP_VERSION.to_le_bytes());
        for (offset, length) in entries {
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
        }
        bytes.extend_from_slice(&body);
        bytes
    }

    #[test]
    fn the_entity_lump_is_read_from_the_directory() {
        let bsp = Bsp::load(build_bsp(b"{ \"classname\" \"worldspawn\" }")).unwrap();
        assert_eq!(bsp.entities(), b"{ \"classname\" \"worldspawn\" }");
        assert!(bsp.lump(1).is_empty(), "an empty lump has no bytes");
    }

    #[test]
    fn a_file_that_is_not_a_bsp_is_refused() {
        let mut bytes = build_bsp(b"{}");
        bytes[0..4].copy_from_slice(b"JUNK");
        assert!(matches!(Bsp::load(bytes), Err(BspError::BadMagic(_))));
        assert!(matches!(Bsp::load(vec![0u8; 10]), Err(BspError::TooShort)));

        let mut bytes = build_bsp(b"{}");
        bytes[4..8].copy_from_slice(&47u32.to_le_bytes());
        assert!(matches!(Bsp::load(bytes), Err(BspError::BadVersion(47))));
    }

    #[test]
    fn a_lump_past_the_end_of_the_file_is_refused() {
        let mut bytes = build_bsp(b"{}");
        // Stretch the entity lump by a hundred bytes.
        let length = i32::from_le_bytes(bytes[12..16].try_into().unwrap()) + 100;
        bytes[12..16].copy_from_slice(&length.to_le_bytes());
        assert!(matches!(Bsp::load(bytes), Err(BspError::LumpOutOfRange(0))));
    }

    #[test]
    fn a_patch_face_carries_its_control_grid_size() {
        let mut face = [0u8; DFACE_SIZE];
        face[8..12].copy_from_slice(&SURFACE_PATCH.to_le_bytes());
        face[96..100].copy_from_slice(&5i32.to_le_bytes());
        face[100..104].copy_from_slice(&3i32.to_le_bytes());

        let bsp = Bsp::load(build_bsp_with(&[(LUMP_FACES, &face)])).unwrap();
        let faces = bsp.faces().unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].surface_type, SURFACE_PATCH);
        assert_eq!((faces[0].patch_width, faces[0].patch_height), (5, 3));
    }

    #[test]
    fn submodel_bounds_are_spread_by_a_unit() {
        let mut model = [0u8; DMODEL_SIZE];
        let corners = [
            (0, -32.0f32),
            (4, -32.0),
            (8, -16.0),
            (12, 32.0),
            (16, 32.0),
            (20, 48.0),
        ];
        for (at, value) in corners {
            model[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        let bsp = Bsp::load(build_bsp_with(&[(LUMP_MODELS, &model)])).unwrap();
        let models = bsp.models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].mins, [-33.0, -33.0, -17.0]);
        assert_eq!(models[0].maxs, [33.0, 33.0, 49.0]);
    }

    #[test]
    fn a_model_lump_that_is_not_whole_records_is_refused() {
        let bsp = Bsp::load(build_bsp_with(&[(LUMP_MODELS, &[0u8; 12])])).unwrap();
        assert_eq!(bsp.models(), Err(BspError::BadLumpSize(LUMP_MODELS)));
    }

    #[test]
    fn a_map_with_no_submodels_has_none() {
        let bsp = Bsp::load(build_bsp(b"{}")).unwrap();
        assert_eq!(bsp.models().unwrap(), Vec::new());
    }

    #[test]
    fn tokens_strip_quotes_and_skip_comments() {
        let text = b"{ // spawn the world\n \"classname\" \"world* spawn\" /* block */ }";
        let mut at = 0;
        let mut tokens = Vec::new();
        while let Some(token) = next_token(text, &mut at) {
            tokens.push(token);
        }
        assert_eq!(tokens, ["{", "classname", "world* spawn", "}"]);
    }

    #[test]
    fn an_empty_lump_has_no_tokens() {
        let mut at = 0;
        assert_eq!(next_token(b"   \n", &mut at), None);
        assert_eq!(next_token(b"", &mut at), None);
    }
}
