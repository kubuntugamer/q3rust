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
/// `sizeof( dmodel_t )`: mins, maxs, radius, headnode, firstface, numfaces.
const DMODEL_SIZE: usize = 40;

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

/// A loaded BSP: the file bytes and the directory that says where each lump
/// sits.
pub struct Bsp {
    bytes: Vec<u8>,
    lumps: [(usize, usize); HEADER_LUMPS],
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

        Ok(Bsp { bytes, lumps })
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
