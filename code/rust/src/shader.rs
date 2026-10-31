//! Shader script parsing, ported from `code/renderer/tr_shader.c`.
//!
//! The tokenizer — [`Parser::next`] is `COM_ParseExt` — drives both
//! halves of the port. [`Parser::parse_stage`] is `ParseStage` with
//! everything under it (`ParseVector`, `ParseWaveForm`, `ParseTexMod`
//! and the name-to-value tables), and [`Parser::parse_shader`] is the
//! `ParseShader` driver with the shader-level directives
//! (`deformVertexes`, `skyParms`, `sort`, `surfaceparm`, `q3map_sun`,
//! ...). [`ShaderTextIndex`] is the last piece: `COM_Compress`,
//! `generateHashValue`, and `ScanAndLoadShaderFiles`'s combined-text
//! table as queried by `FindShaderInShaderText`, so a shader name finds
//! its definition and hands it straight to [`Parser::parse_shader`].
//! What happens *after* parsing — the `FinishShader` optimisation pass
//! — is not ported yet.
//!
//! Two deliberate differences from the C:
//!
//! * Image lookup is deferred: `map`, `clampmap`, `animMap` and
//!   `videoMap` record the name instead of calling `R_FindImageFile`, so
//!   a missing image can no longer fail a parse the way it does in C.
//!   `$lightmap`/`$whiteimage` are recorded the same way instead of
//!   being resolved against `tr.lightmaps`.
//! * Warnings are collected in [`Parser::warnings`] instead of being
//!   printed through `ri.Printf`, and the `ri.Error(ERR_DROP)` for too
//!   many `tcMod`s becomes a [`ShaderError`] the caller sees.
//!
//! Everything else — token rules, warning text, defaulting, the order
//! the state bits are folded in — follows the C exactly, including its
//! quirks; those are called out where they occur.

use crate::ref_types::Vec3;

/// `MAX_SHADER_STAGES` from `tr_local.h`.
pub const MAX_SHADER_STAGES: usize = 8;
/// `TR_MAX_TEXMODS`: `tcMod`s per stage.
pub const TR_MAX_TEXMODS: usize = 4;
/// `MAX_IMAGE_ANIMATIONS` from `tr_local.h`.
pub const MAX_IMAGE_ANIMATIONS: usize = 8;
/// `MAX_TOKEN_CHARS` from `q_shared.h`.
const MAX_TOKEN_CHARS: usize = 1024;

// Render state bits from tr_local.h; a stage's `stateBits` is the
// combination the backend applies when the stage runs.
pub const GLS_SRCBLEND_ZERO: u32 = 0x0000_0001;
pub const GLS_SRCBLEND_ONE: u32 = 0x0000_0002;
pub const GLS_SRCBLEND_DST_COLOR: u32 = 0x0000_0003;
pub const GLS_SRCBLEND_ONE_MINUS_DST_COLOR: u32 = 0x0000_0004;
pub const GLS_SRCBLEND_SRC_ALPHA: u32 = 0x0000_0005;
pub const GLS_SRCBLEND_ONE_MINUS_SRC_ALPHA: u32 = 0x0000_0006;
pub const GLS_SRCBLEND_DST_ALPHA: u32 = 0x0000_0007;
pub const GLS_SRCBLEND_ONE_MINUS_DST_ALPHA: u32 = 0x0000_0008;
pub const GLS_SRCBLEND_ALPHA_SATURATE: u32 = 0x0000_0009;
pub const GLS_DSTBLEND_ZERO: u32 = 0x0000_0010;
pub const GLS_DSTBLEND_ONE: u32 = 0x0000_0020;
pub const GLS_DSTBLEND_SRC_COLOR: u32 = 0x0000_0030;
pub const GLS_DSTBLEND_ONE_MINUS_SRC_COLOR: u32 = 0x0000_0040;
pub const GLS_DSTBLEND_SRC_ALPHA: u32 = 0x0000_0050;
pub const GLS_DSTBLEND_ONE_MINUS_SRC_ALPHA: u32 = 0x0000_0060;
pub const GLS_DSTBLEND_DST_ALPHA: u32 = 0x0000_0070;
pub const GLS_DSTBLEND_ONE_MINUS_DST_ALPHA: u32 = 0x0000_0080;
pub const GLS_DEPTHMASK_TRUE: u32 = 0x0000_0100;
pub const GLS_DEPTHFUNC_EQUAL: u32 = 0x0002_0000;
pub const GLS_ATEST_GT_0: u32 = 0x1000_0000;
pub const GLS_ATEST_LT_80: u32 = 0x2000_0000;
pub const GLS_ATEST_GE_80: u32 = 0x4000_0000;

/// `MAX_SHADER_DEFORMS` from `tr_local.h`.
pub const MAX_SHADER_DEFORMS: usize = 3;
/// `MAX_SHADERTEXT_HASH` from `tr_shader.c` — the label hash buckets.
pub const MAX_SHADERTEXT_HASH: usize = 2048;
/// `MAX_SHADER_FILES` from `tr_shader.c`.
pub const MAX_SHADER_FILES: usize = 4096;

// Contents and surface flags from code/game/surfaceflags.h — the subset
// the `surfaceparm` table references.
pub const CONTENTS_LAVA: u32 = 8;
pub const CONTENTS_SLIME: u32 = 16;
pub const CONTENTS_WATER: u32 = 32;
pub const CONTENTS_FOG: u32 = 64;
pub const CONTENTS_AREAPORTAL: u32 = 0x8000;
pub const CONTENTS_PLAYERCLIP: u32 = 0x1_0000;
pub const CONTENTS_MONSTERCLIP: u32 = 0x2_0000;
pub const CONTENTS_CLUSTERPORTAL: u32 = 0x10_0000;
pub const CONTENTS_DONOTENTER: u32 = 0x20_0000;
pub const CONTENTS_ORIGIN: u32 = 0x100_0000;
pub const CONTENTS_DETAIL: u32 = 0x800_0000;
pub const CONTENTS_STRUCTURAL: u32 = 0x1000_0000;
pub const CONTENTS_TRANSLUCENT: u32 = 0x2000_0000;
pub const CONTENTS_NODROP: u32 = 0x8000_0000;
pub const SURF_NODAMAGE: u32 = 0x1;
pub const SURF_SLICK: u32 = 0x2;
pub const SURF_SKY: u32 = 0x4;
pub const SURF_LADDER: u32 = 0x8;
pub const SURF_NOIMPACT: u32 = 0x10;
pub const SURF_NOMARKS: u32 = 0x20;
pub const SURF_FLESH: u32 = 0x40;
pub const SURF_NODRAW: u32 = 0x80;
pub const SURF_HINT: u32 = 0x100;
pub const SURF_NOLIGHTMAP: u32 = 0x400;
pub const SURF_POINTLIGHT: u32 = 0x800;
pub const SURF_METALSTEPS: u32 = 0x1000;
pub const SURF_NOSTEPS: u32 = 0x2000;
pub const SURF_NONSOLID: u32 = 0x4000;
pub const SURF_LIGHTFILTER: u32 = 0x8000;
pub const SURF_ALPHASHADOW: u32 = 0x1_0000;
pub const SURF_NODLIGHT: u32 = 0x2_0000;
pub const SURF_DUST: u32 = 0x4_0000;

/// `infoParms[]` from `tr_shader.c` — the `surfaceparm` table, as
/// `(name, surfaceFlags, contents)`. The C's `clearSolid` column is
/// compiled out (`#if 0`), so it is not carried over.
const INFO_PARMS: &[(&str, u32, u32)] = &[
    ("water", 0, CONTENTS_WATER),
    ("slime", 0, CONTENTS_SLIME),
    ("lava", 0, CONTENTS_LAVA),
    ("playerclip", 0, CONTENTS_PLAYERCLIP),
    ("monsterclip", 0, CONTENTS_MONSTERCLIP),
    ("nodrop", 0, CONTENTS_NODROP),
    ("nonsolid", SURF_NONSOLID, 0),
    ("origin", 0, CONTENTS_ORIGIN),
    ("trans", 0, CONTENTS_TRANSLUCENT),
    ("detail", 0, CONTENTS_DETAIL),
    ("structural", 0, CONTENTS_STRUCTURAL),
    ("areaportal", 0, CONTENTS_AREAPORTAL),
    ("clusterportal", 0, CONTENTS_CLUSTERPORTAL),
    ("donotenter", 0, CONTENTS_DONOTENTER),
    ("fog", 0, CONTENTS_FOG),
    ("sky", SURF_SKY, 0),
    ("lightfilter", SURF_LIGHTFILTER, 0),
    ("alphashadow", SURF_ALPHASHADOW, 0),
    ("hint", SURF_HINT, 0),
    ("slick", SURF_SLICK, 0),
    ("noimpact", SURF_NOIMPACT, 0),
    ("nomarks", SURF_NOMARKS, 0),
    ("ladder", SURF_LADDER, 0),
    ("nodamage", SURF_NODAMAGE, 0),
    ("metalsteps", SURF_METALSTEPS, 0),
    ("flesh", SURF_FLESH, 0),
    ("nosteps", SURF_NOSTEPS, 0),
    ("nodraw", SURF_NODRAW, 0),
    ("pointlight", SURF_POINTLIGHT, 0),
    ("nolightmap", SURF_NOLIGHTMAP, 0),
    ("nodlight", SURF_NODLIGHT, 0),
    ("dust", SURF_DUST, 0),
];

/// `genFunc_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GenFunc {
    #[default]
    None = 0,
    Sin = 1,
    Square = 2,
    Triangle = 3,
    Sawtooth = 4,
    InverseSawtooth = 5,
    Noise = 6,
}

/// `colorGen_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorGen {
    #[default]
    Bad = 0,
    IdentityLighting = 1,
    Identity = 2,
    Entity = 3,
    OneMinusEntity = 4,
    ExactVertex = 5,
    Vertex = 6,
    OneMinusVertex = 7,
    Waveform = 8,
    LightingDiffuse = 9,
    Fog = 10,
    Const = 11,
}

/// `alphaGen_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaGen {
    #[default]
    Identity = 0,
    Skip = 1,
    Entity = 2,
    OneMinusEntity = 3,
    Vertex = 4,
    OneMinusVertex = 5,
    LightingSpecular = 6,
    Waveform = 7,
    Portal = 8,
    Const = 9,
}

/// `texCoordGen_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TexCoordGen {
    #[default]
    Bad = 0,
    Identity = 1,
    Lightmap = 2,
    Texture = 3,
    EnvironmentMapped = 4,
    Fog = 5,
    Vector = 6,
}

/// `waveForm_t`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WaveForm {
    pub func: GenFunc,
    pub base: f32,
    pub amplitude: f32,
    pub phase: f32,
    pub frequency: f32,
}

/// One `tcMod`, in place of `texModInfo_t`'s type tag plus loose fields.
/// The `None` variant is the C's zero-initialised slot: a `tcMod` whose
/// parameters failed to parse still consumes a `TR_MAX_TEXMODS` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum TexMod {
    None,
    /// `transform s00 s01 s10 s11 tx ty`.
    Transform {
        matrix: [[f32; 2]; 2],
        translate: [f32; 2],
    },
    /// `turb base amplitude phase frequency` — the C leaves `wave.func`
    /// untouched (zeroed), so it stays `None` here too.
    Turbulent {
        wave: WaveForm,
    },
    /// `scroll s t`.
    Scroll {
        scroll: [f32; 2],
    },
    /// `scale s t`.
    Scale {
        scale: [f32; 2],
    },
    /// `stretch <waveform>`.
    Stretch {
        wave: WaveForm,
    },
    /// `rotate degrees-per-second`, clockwise positive.
    Rotate {
        speed: f32,
    },
    /// `entityTranslate`.
    EntityTranslate,
}

/// What a stage's texture keyword binds. The name is recorded rather
/// than looked up — see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub enum TextureMap {
    /// `map <name>` (repeat) or `clampmap <name>` (clamp to edge).
    File { name: String, clamp: bool },
    /// `$lightmap`.
    Lightmap,
    /// `$whiteimage`.
    WhiteImage,
    /// `videoMap <name>`.
    Video(String),
    /// `animMap <speed> <frame>...`; repeated `animMap` lines append
    /// frames, since C's `numImageAnimations` accumulates.
    Anim { speed: f32, frames: Vec<String> },
}

/// `textureBundle_t` as `ParseStage` fills it. The C carries two bundles
/// per stage, but only `bundle[0]` is written while parsing; the second
/// is synthesized later by the multitexture collapse.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextureBundle {
    pub map: Option<TextureMap>,
    pub tc_gen: TexCoordGen,
    pub tc_gen_vectors: [Vec3; 2],
    pub tex_mods: Vec<TexMod>,
}

/// `shaderStage_t` as `ParseStage` fills it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShaderStage {
    pub active: bool,
    pub bundle: TextureBundle,
    pub rgb_wave: WaveForm,
    pub rgb_gen: ColorGen,
    pub alpha_wave: WaveForm,
    pub alpha_gen: AlphaGen,
    /// `byte constantColor[4]` for `CGEN_CONST`/`AGEN_CONST`.
    pub constant_color: [u8; 4],
    /// The `GLS_*` mask this stage draws with.
    pub state_bits: u32,
    pub is_detail: bool,
}

/// The parse result of one shader definition — everything `ParseShader`
/// fills. (The `FinishShader` optimisation pass that runs over this
/// afterwards is not ported yet.)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShaderScript {
    pub name: String,
    pub stages: Vec<ShaderStage>,
    /// `shader.portalRange`, written by `alphaGen portal` (from inside
    /// stage parsing, exactly as the C writes the global).
    pub portal_range: Option<f32>,
    /// `deformVertexes`, in file order; at most [`MAX_SHADER_DEFORMS`].
    pub deforms: Vec<DeformStage>,
    /// `skyParms` — set exactly where the C sets `is_sky`, i.e. only
    /// after the whole line parsed.
    pub sky: Option<SkyParms>,
    /// `fogParms`.
    pub fog: Option<FogParms>,
    /// `sort`; defaults to `SS_BAD`, as `ParseShader` leaves it.
    pub sort: ShaderSort,
    /// `cull`; defaults to front-sided.
    pub cull: CullType,
    /// OR of every `surfaceparm`'s flags/contents.
    pub surface_flags: u32,
    pub content_flags: u32,
    pub no_mip_maps: bool,
    pub no_pic_mip: bool,
    pub polygon_offset: bool,
    pub entity_mergable: bool,
    /// `clampTime`.
    pub clamp_time: Option<f32>,
    /// `q3map_sun`: normalized-then-scaled light and built direction.
    pub sun: Option<SunParms>,
    /// Set once the shader closed cleanly, as `explicitlyDefined`.
    pub explicitly_defined: bool,
}

/// `deform_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deform {
    #[default]
    None = 0,
    Wave = 1,
    Normals = 2,
    Bulge = 3,
    Move = 4,
    ProjectionShadow = 5,
    Autosprite = 6,
    Autosprite2 = 7,
    Text0 = 8,
    Text1 = 9,
    Text2 = 10,
    Text3 = 11,
    Text4 = 12,
    Text5 = 13,
    Text6 = 14,
    Text7 = 15,
}

/// `deformStage_t`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeformStage {
    pub deform: Deform,
    pub move_vector: Vec3,
    pub wave: WaveForm,
    /// 1/divisor — 100.0 when the divisor was 0.
    pub spread: f32,
    pub bulge_width: f32,
    pub bulge_height: f32,
    pub bulge_speed: f32,
}

/// `cullType_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CullType {
    #[default]
    FrontSided = 0,
    BackSided = 1,
    TwoSided = 2,
}

/// `shaderSort_t`. The C assigns raw numbers too (`shader.sort =
/// atof(token)`), so this is a newtype around the int the renderer
/// sorts by, with `shaderSort_t`'s constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShaderSort(pub i32);

impl ShaderSort {
    pub const BAD: ShaderSort = ShaderSort(0);
    pub const PORTAL: ShaderSort = ShaderSort(1);
    pub const ENVIRONMENT: ShaderSort = ShaderSort(2);
    pub const OPAQUE: ShaderSort = ShaderSort(3);
    pub const DECAL: ShaderSort = ShaderSort(4);
    pub const SEE_THROUGH: ShaderSort = ShaderSort(5);
    pub const BANNER: ShaderSort = ShaderSort(6);
    pub const FOG: ShaderSort = ShaderSort(7);
    pub const UNDERWATER: ShaderSort = ShaderSort(8);
    pub const BLEND0: ShaderSort = ShaderSort(9);
    pub const BLEND1: ShaderSort = ShaderSort(10);
    pub const BLEND2: ShaderSort = ShaderSort(11);
    pub const BLEND3: ShaderSort = ShaderSort(12);
    pub const BLEND6: ShaderSort = ShaderSort(13);
    pub const STENCIL_SHADOW: ShaderSort = ShaderSort(14);
    pub const ALMOST_NEAREST: ShaderSort = ShaderSort(15);
    pub const NEAREST: ShaderSort = ShaderSort(16);
}

/// `skyParms_t`: the two six-face boxes as name prefixes (lookups are
/// deferred) and the cloud height.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkyParms {
    /// `prefix_{rt,bk,lf,ft,up,dn}.tga` names, or `None` for `-`.
    pub outerbox: Option<[String; 6]>,
    pub innerbox: Option<[String; 6]>,
    pub cloud_height: f32,
}

impl SkyParms {
    /// The C's `suf` table, in order.
    fn box_names(prefix: &str) -> [String; 6] {
        ["rt", "bk", "lf", "ft", "up", "dn"].map(|face| format!("{prefix}_{face}.tga"))
    }
}

/// `fogParms_t`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FogParms {
    pub color: Vec3,
    pub depth_for_opaque: f32,
}

/// `tr.sunLight` / `tr.sunDirection` as one `q3map_sun` leaves them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SunParms {
    pub light: Vec3,
    pub direction: Vec3,
}

/// A shader script could not be parsed — the cases where `ParseShader`
/// or `ParseStage` return `qfalse`, or `ri.Error` would abort the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaderError {
    /// The text ended inside a stage (`no matching '}' found`).
    UnterminatedStage,
    /// The definition never closed (`no concluding '}'`).
    UnterminatedShader,
    /// The definition did not open with `{`.
    ExpectedOpenBrace(String),
    /// An unrecognised shader-level directive, which fails the parse.
    UnknownShaderParameter(String),
    /// No stages, and neither a sky nor fog contents — the C refuses
    /// shaders like this.
    NoStages,
    /// `fogParms`' colour vector failed to parse (the paren warning is
    /// already in the warnings).
    MissingVector,
    /// An unrecognised directive inside a stage, which fails the parse.
    UnknownStageParameter(String),
    /// A keyword's parameter was missing; the keyword is the C's.
    MissingParameter { keyword: &'static str },
    /// More than [`MAX_SHADER_STAGES`] stages. The C has no check here
    /// and would write past `stages[8]`; this port refuses instead.
    TooManyStages,
    /// More than [`TR_MAX_TEXMODS`] `tcMod`s — `ri.Error(ERR_DROP)`.
    TooManyTexMods,
}

/// The shader-script parser: a `COM_ParseExt` cursor over the script
/// text plus the warnings the C would have printed along the way.
pub struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    pub warnings: Vec<String>,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Parser<'a> {
        Parser {
            data: text.as_bytes(),
            pos: 0,
            warnings: Vec::new(),
        }
    }

    /// True once the text has been consumed to the end.
    pub fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    /// `COM_ParseExt(data_p, allowLineBreaks)`: the next token, or `""`
    /// at the end of the text or — when line breaks are not allowed —
    /// at the next newline. Token rules match the C: whitespace and both
    /// comment styles are skipped, `"`-quoted runs keep their contents
    /// verbatim (truncated at [`MAX_TOKEN_CHARS`]), plain words run until
    /// a byte that is `<= ' '` with `char` signed (so bytes `>= 0x80`
    /// count as whitespace and end words, exactly as on the platforms
    /// the C targets), and words of [`MAX_TOKEN_CHARS`] or more are
    /// discarded whole.
    ///
    /// The slices returned always fall on ASCII or lead bytes — the only
    /// bytes that can terminate a word — so they are valid `&str`.
    pub fn next(&mut self, allow_line_breaks: bool) -> &'a str {
        let mut has_new_lines = false;
        loop {
            while self.pos < self.data.len() {
                let c = self.data[self.pos] as i8;
                if c > 32 {
                    break;
                }
                if self.data[self.pos] == b'\n' {
                    has_new_lines = true;
                }
                self.pos += 1;
            }
            if self.pos >= self.data.len() {
                return "";
            }
            // The C's flag is sticky for the whole call, so a newline
            // seen before a comment still stops a non-line-breaking read
            // after it.
            if has_new_lines && !allow_line_breaks {
                return "";
            }
            if self.data[self.pos] == b'/' && self.data.get(self.pos + 1) == Some(&b'/') {
                self.pos += 2;
                while self.pos < self.data.len() && self.data[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            if self.data[self.pos] == b'/' && self.data.get(self.pos + 1) == Some(&b'*') {
                self.pos += 2;
                while self.pos < self.data.len()
                    && !(self.data[self.pos] == b'*' && self.data.get(self.pos + 1) == Some(&b'/'))
                {
                    self.pos += 1;
                }
                if self.pos < self.data.len() {
                    self.pos += 2;
                }
                continue;
            }
            break;
        }

        if self.data[self.pos] == b'"' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < self.data.len() && self.data[self.pos] != b'"' {
                self.pos += 1;
            }
            let end = self.pos.min(start + MAX_TOKEN_CHARS);
            if self.pos < self.data.len() {
                self.pos += 1; // consume the closing quote
            }
            return std::str::from_utf8(&self.data[start..end]).unwrap_or("");
        }

        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] as i8 > 32 {
            self.pos += 1;
        }
        if self.pos - start >= MAX_TOKEN_CHARS {
            return "";
        }
        std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("")
    }

    /// `SkipRestOfLine`: consume through the next newline. The C steps
    /// one byte past the terminator at end of text; this clamps there
    /// instead, which behaves the same for any later parse.
    pub fn skip_rest_of_line(&mut self) {
        while self.pos < self.data.len() {
            let c = self.data[self.pos];
            self.pos += 1;
            if c == b'\n' {
                break;
            }
        }
    }

    /// The current byte offset into the text — the `oldp` the C keeps
    /// around `COM_ParseExt` calls.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// `SkipBracedSection`: run tokens until the brace depth returns to
    /// zero. The C's do-while runs at least once, so text with no
    /// opening brace consumes exactly one token — and a stray `}`
    /// before any `{` drives the depth negative until a `{` balances
    /// it, both reproduced by the signed depth here.
    pub fn skip_braced_section(&mut self) {
        let mut depth: i32 = 0;
        loop {
            let token = self.next(true);
            if token.len() == 1 {
                if token == "{" {
                    depth += 1;
                } else if token == "}" {
                    depth -= 1;
                }
            }
            if depth == 0 || self.at_end() {
                break;
            }
        }
    }

    /// `ParseStage`: reads one `{ ... }` stage into `script`. Appends on
    /// success; on error nothing is added, as the C discards the shader.
    pub fn parse_stage(&mut self, script: &mut ShaderScript) -> Result<(), ShaderError> {
        if script.stages.len() >= MAX_SHADER_STAGES {
            return Err(ShaderError::TooManyStages);
        }

        let mut stage = ShaderStage::default();
        stage.active = true;

        let mut depth_mask_bits = GLS_DEPTHMASK_TRUE;
        let mut depth_mask_explicit = false;
        let mut blend_src_bits = 0;
        let mut blend_dst_bits = 0;
        let mut atest_bits = 0;
        let mut depth_func_bits = 0;

        loop {
            let token = self.next(true).to_string();
            if token.is_empty() {
                self.warn("no matching '}' found".to_string());
                return Err(ShaderError::UnterminatedStage);
            }
            // The C tests token[0] only, so anything starting with
            // '}' closes the stage.
            if token.starts_with('}') {
                break;
            }

            let name = script.name.clone();
            let lower = token.to_ascii_lowercase();

            match lower.as_str() {
                "map" | "clampmap" => {
                    let clamp = lower == "clampmap";
                    let image = self.next(false).to_string();
                    if image.is_empty() {
                        let keyword = if clamp { "clampmap" } else { "map" };
                        self.warn(format!(
                            "missing parameter for '{keyword}' keyword in shader '{name}'"
                        ));
                        return Err(ShaderError::MissingParameter { keyword });
                    }
                    if !clamp && image.eq_ignore_ascii_case("$whiteimage") {
                        stage.bundle.map = Some(TextureMap::WhiteImage);
                    } else if !clamp && image.eq_ignore_ascii_case("$lightmap") {
                        stage.bundle.map = Some(TextureMap::Lightmap);
                    } else {
                        stage.bundle.map = Some(TextureMap::File { name: image, clamp });
                    }
                }
                "animmap" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        // The C's message says "animMmap" — kept as is.
                        self.warn(format!(
                            "missing parameter for 'animMmap' keyword in shader '{name}'"
                        ));
                        return Err(ShaderError::MissingParameter { keyword: "animMap" });
                    }
                    let speed = atof(&token);
                    // Repeated animMap lines accumulate frames, as C's
                    // numImageAnimations keeps counting; the last line's
                    // speed wins.
                    let mut frames = match stage.bundle.map.take() {
                        Some(TextureMap::Anim { frames, .. }) => frames,
                        other => {
                            stage.bundle.map = other;
                            Vec::new()
                        }
                    };
                    loop {
                        let frame = self.next(false).to_string();
                        if frame.is_empty() {
                            break;
                        }
                        if frames.len() < MAX_IMAGE_ANIMATIONS {
                            frames.push(frame);
                        }
                    }
                    stage.bundle.map = Some(TextureMap::Anim { speed, frames });
                }
                "videomap" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        // The C's message says "videoMmap" — kept as is.
                        self.warn(format!(
                            "missing parameter for 'videoMmap' keyword in shader '{name}'"
                        ));
                        return Err(ShaderError::MissingParameter {
                            keyword: "videoMap",
                        });
                    }
                    stage.bundle.map = Some(TextureMap::Video(token));
                }
                "alphafunc" => {
                    let token = self.next(false);
                    if token.is_empty() {
                        self.warn(format!(
                            "missing parameter for 'alphaFunc' keyword in shader '{name}'"
                        ));
                        return Err(ShaderError::MissingParameter {
                            keyword: "alphaFunc",
                        });
                    }
                    atest_bits = name_to_afunc(&token, &name, &mut self.warnings);
                }
                "depthfunc" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        self.warn(format!(
                            "missing parameter for 'depthfunc' keyword in shader '{name}'"
                        ));
                        return Err(ShaderError::MissingParameter {
                            keyword: "depthfunc",
                        });
                    }
                    if token.eq_ignore_ascii_case("lequal") {
                        depth_func_bits = 0;
                    } else if token.eq_ignore_ascii_case("equal") {
                        depth_func_bits = GLS_DEPTHFUNC_EQUAL;
                    } else {
                        self.warn(format!("unknown depthfunc '{token}' in shader '{name}'"));
                    }
                }
                "detail" => {
                    stage.is_detail = true;
                }
                "blendfunc" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        self.warn(format!("missing parm for blendFunc in shader '{name}'"));
                        continue;
                    }
                    if token.eq_ignore_ascii_case("add") {
                        blend_src_bits = GLS_SRCBLEND_ONE;
                        blend_dst_bits = GLS_DSTBLEND_ONE;
                    } else if token.eq_ignore_ascii_case("filter") {
                        blend_src_bits = GLS_SRCBLEND_DST_COLOR;
                        blend_dst_bits = GLS_DSTBLEND_ZERO;
                    } else if token.eq_ignore_ascii_case("blend") {
                        blend_src_bits = GLS_SRCBLEND_SRC_ALPHA;
                        blend_dst_bits = GLS_DSTBLEND_ONE_MINUS_SRC_ALPHA;
                    } else {
                        blend_src_bits = name_to_src_blend_mode(&token, &name, &mut self.warnings);
                        let token = self.next(false).to_string();
                        if token.is_empty() {
                            self.warn(format!("missing parm for blendFunc in shader '{name}'"));
                            continue;
                        }
                        blend_dst_bits = name_to_dst_blend_mode(&token, &name, &mut self.warnings);
                    }
                    // Clear the depth mask for blended surfaces unless
                    // depthwrite said otherwise.
                    if !depth_mask_explicit {
                        depth_mask_bits = 0;
                    }
                }
                "rgbgen" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        self.warn(format!("missing parameters for rgbGen in shader '{name}'"));
                        continue;
                    }
                    if token.eq_ignore_ascii_case("wave") {
                        stage.rgb_wave = self.parse_wave_form(&name);
                        stage.rgb_gen = ColorGen::Waveform;
                    } else if token.eq_ignore_ascii_case("const") {
                        let mut color = [0.0f32; 3];
                        self.parse_vector(3, &mut color, &name);
                        // The C ignores the result of ParseVector here
                        // and scales whatever landed in the buffer; an
                        // unread element stays zero instead of the C's
                        // uninitialised stack.
                        for i in 0..3 {
                            stage.constant_color[i] = (255.0 * color[i]) as u8;
                        }
                        stage.rgb_gen = ColorGen::Const;
                    } else if token.eq_ignore_ascii_case("identity") {
                        stage.rgb_gen = ColorGen::Identity;
                    } else if token.eq_ignore_ascii_case("identitylighting") {
                        stage.rgb_gen = ColorGen::IdentityLighting;
                    } else if token.eq_ignore_ascii_case("entity") {
                        stage.rgb_gen = ColorGen::Entity;
                    } else if token.eq_ignore_ascii_case("oneminusentity") {
                        stage.rgb_gen = ColorGen::OneMinusEntity;
                    } else if token.eq_ignore_ascii_case("vertex") {
                        stage.rgb_gen = ColorGen::Vertex;
                        if stage.alpha_gen == AlphaGen::Identity {
                            stage.alpha_gen = AlphaGen::Vertex;
                        }
                    } else if token.eq_ignore_ascii_case("exactvertex") {
                        stage.rgb_gen = ColorGen::ExactVertex;
                    } else if token.eq_ignore_ascii_case("lightingdiffuse") {
                        stage.rgb_gen = ColorGen::LightingDiffuse;
                    } else if token.eq_ignore_ascii_case("oneminusvertex") {
                        stage.rgb_gen = ColorGen::OneMinusVertex;
                    } else {
                        self.warn(format!(
                            "unknown rgbGen parameter '{token}' in shader '{name}'"
                        ));
                        continue;
                    }
                }
                "alphagen" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        self.warn(format!(
                            "missing parameters for alphaGen in shader '{name}'"
                        ));
                        continue;
                    }
                    if token.eq_ignore_ascii_case("wave") {
                        stage.alpha_wave = self.parse_wave_form(&name);
                        stage.alpha_gen = AlphaGen::Waveform;
                    } else if token.eq_ignore_ascii_case("const") {
                        let token = self.next(false);
                        stage.constant_color[3] = (255.0 * atof(token)) as u8;
                        stage.alpha_gen = AlphaGen::Const;
                    } else if token.eq_ignore_ascii_case("identity") {
                        stage.alpha_gen = AlphaGen::Identity;
                    } else if token.eq_ignore_ascii_case("entity") {
                        stage.alpha_gen = AlphaGen::Entity;
                    } else if token.eq_ignore_ascii_case("oneminusentity") {
                        stage.alpha_gen = AlphaGen::OneMinusEntity;
                    } else if token.eq_ignore_ascii_case("vertex") {
                        stage.alpha_gen = AlphaGen::Vertex;
                    } else if token.eq_ignore_ascii_case("lightingspecular") {
                        stage.alpha_gen = AlphaGen::LightingSpecular;
                    } else if token.eq_ignore_ascii_case("oneminusvertex") {
                        stage.alpha_gen = AlphaGen::OneMinusVertex;
                    } else if token.eq_ignore_ascii_case("portal") {
                        stage.alpha_gen = AlphaGen::Portal;
                        let token = self.next(false);
                        if token.is_empty() {
                            script.portal_range = Some(256.0);
                            self.warn(format!(
                                "missing range parameter for alphaGen portal in shader '{name}', defaulting to 256"
                            ));
                        } else {
                            script.portal_range = Some(atof(token));
                        }
                    } else {
                        self.warn(format!(
                            "unknown alphaGen parameter '{token}' in shader '{name}'"
                        ));
                        continue;
                    }
                }
                "texgen" | "tcgen" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
                        self.warn(format!("missing texgen parm in shader '{name}'"));
                        continue;
                    }
                    if token.eq_ignore_ascii_case("environment") {
                        stage.bundle.tc_gen = TexCoordGen::EnvironmentMapped;
                    } else if token.eq_ignore_ascii_case("lightmap") {
                        stage.bundle.tc_gen = TexCoordGen::Lightmap;
                    } else if token.eq_ignore_ascii_case("texture")
                        || token.eq_ignore_ascii_case("base")
                    {
                        stage.bundle.tc_gen = TexCoordGen::Texture;
                    } else if token.eq_ignore_ascii_case("vector") {
                        // The C ignores the result here as well.
                        self.parse_vector(3, &mut stage.bundle.tc_gen_vectors[0], &name);
                        self.parse_vector(3, &mut stage.bundle.tc_gen_vectors[1], &name);
                        stage.bundle.tc_gen = TexCoordGen::Vector;
                    } else {
                        self.warn(format!("unknown texgen parm in shader '{name}'"));
                    }
                }
                "tcmod" => {
                    self.parse_tex_mod(&mut stage, &name)?;
                }
                "depthwrite" => {
                    depth_mask_bits = GLS_DEPTHMASK_TRUE;
                    depth_mask_explicit = true;
                    continue;
                }
                _ => {
                    self.warn(format!("unknown parameter '{token}' in shader '{name}'"));
                    return Err(ShaderError::UnknownStageParameter(token));
                }
            }
        }

        // If cgen isn't explicitly specified, use either identity or
        // identitylighting.
        if stage.rgb_gen == ColorGen::Bad {
            if blend_src_bits == 0
                || blend_src_bits == GLS_SRCBLEND_ONE
                || blend_src_bits == GLS_SRCBLEND_SRC_ALPHA
            {
                stage.rgb_gen = ColorGen::IdentityLighting;
            } else {
                stage.rgb_gen = ColorGen::Identity;
            }
        }

        // A GL_ONE GL_ZERO blend disables blending and re-enables depth
        // masking.
        if blend_src_bits == GLS_SRCBLEND_ONE && blend_dst_bits == GLS_DSTBLEND_ZERO {
            blend_dst_bits = 0;
            blend_src_bits = 0;
            depth_mask_bits = GLS_DEPTHMASK_TRUE;
        }

        // Decide which agens we can skip. The C compares `alphaGen`
        // against `CGEN_IDENTITY`, so numerically this fires for
        // AGEN_ENTITY (both are 2) and not for AGEN_IDENTITY (0); that
        // quirk is reproduced deliberately.
        if (stage.alpha_gen as u32) == (ColorGen::Identity as u32) {
            if stage.rgb_gen == ColorGen::Identity || stage.rgb_gen == ColorGen::LightingDiffuse {
                stage.alpha_gen = AlphaGen::Skip;
            }
        }

        stage.state_bits =
            depth_mask_bits | blend_src_bits | blend_dst_bits | atest_bits | depth_func_bits;

        script.stages.push(stage);
        Ok(())
    }

    /// `ParseWaveForm`: a gen func followed by base, amplitude, phase
    /// and frequency. Missing parts warn and stop, leaving the rest at
    /// their defaults as the C does.
    fn parse_wave_form(&mut self, name: &str) -> WaveForm {
        let mut wave = WaveForm::default();

        let token = self.next(false).to_string();
        if token.is_empty() {
            self.warn(format!("missing waveform parm in shader '{name}'"));
            return wave;
        }
        wave.func = name_to_gen_func(&token, name, &mut self.warnings);

        for field in ["base", "amplitude", "phase", "frequency"] {
            let token = self.next(false).to_string();
            if token.is_empty() {
                self.warn(format!("missing waveform parm in shader '{name}'"));
                return wave;
            }
            let value = atof(&token);
            match field {
                "base" => wave.base = value,
                "amplitude" => wave.amplitude = value,
                "phase" => wave.phase = value,
                _ => wave.frequency = value,
            }
        }
        wave
    }

    /// `ParseTexMod`: reads one `tcMod` and appends it to the stage. The
    /// C re-tokenises a line-joined copy of the parameters first; the
    /// tokens are identical, so they are read straight from the cursor.
    fn parse_tex_mod(&mut self, stage: &mut ShaderStage, name: &str) -> Result<(), ShaderError> {
        if stage.bundle.tex_mods.len() >= TR_MAX_TEXMODS {
            return Err(ShaderError::TooManyTexMods);
        }
        // Reserve the slot the C writes before parsing; failures leave
        // it as the zeroed TMOD_NONE entry.
        stage.bundle.tex_mods.push(TexMod::None);
        let slot = stage.bundle.tex_mods.len() - 1;

        let kind = self.next(false).to_string();
        if kind.eq_ignore_ascii_case("turb") {
            let mut wave = WaveForm::default();
            let fields = ["base", "amplitude", "phase", "frequency"];
            for (i, field) in fields.iter().enumerate() {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    let what = if i == 0 {
                        "tcMod turb parms"
                    } else {
                        "tcMod turb"
                    };
                    self.warn(format!("missing {what} in shader '{name}'"));
                    stage.bundle.tex_mods[slot] = TexMod::Turbulent { wave };
                    return Ok(());
                }
                let value = atof(&token);
                match *field {
                    "base" => wave.base = value,
                    "amplitude" => wave.amplitude = value,
                    "phase" => wave.phase = value,
                    _ => wave.frequency = value,
                }
            }
            stage.bundle.tex_mods[slot] = TexMod::Turbulent { wave };
        } else if kind.eq_ignore_ascii_case("scale") {
            let mut scale = [0.0f32; 2];
            for i in 0..2 {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    self.warn(format!("missing scale parms in shader '{name}'"));
                    stage.bundle.tex_mods[slot] = TexMod::Scale { scale };
                    return Ok(());
                }
                scale[i] = atof(&token);
            }
            stage.bundle.tex_mods[slot] = TexMod::Scale { scale };
        } else if kind.eq_ignore_ascii_case("scroll") {
            let mut scroll = [0.0f32; 2];
            for i in 0..2 {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    // The C says "scale scroll parms" here — kept as is.
                    self.warn(format!("missing scale scroll parms in shader '{name}'"));
                    stage.bundle.tex_mods[slot] = TexMod::Scroll { scroll };
                    return Ok(());
                }
                scroll[i] = atof(&token);
            }
            stage.bundle.tex_mods[slot] = TexMod::Scroll { scroll };
        } else if kind.eq_ignore_ascii_case("stretch") {
            let token = self.next(false).to_string();
            if token.is_empty() {
                self.warn(format!("missing stretch parms in shader '{name}'"));
                return Ok(());
            }
            let mut wave = WaveForm {
                func: name_to_gen_func(&token, name, &mut self.warnings),
                ..WaveForm::default()
            };
            let fields = ["base", "amplitude", "phase", "frequency"];
            for field in fields {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    self.warn(format!("missing stretch parms in shader '{name}'"));
                    stage.bundle.tex_mods[slot] = TexMod::Stretch { wave };
                    return Ok(());
                }
                let value = atof(&token);
                match field {
                    "base" => wave.base = value,
                    "amplitude" => wave.amplitude = value,
                    "phase" => wave.phase = value,
                    _ => wave.frequency = value,
                }
            }
            stage.bundle.tex_mods[slot] = TexMod::Stretch { wave };
        } else if kind.eq_ignore_ascii_case("transform") {
            let mut matrix = [[0.0f32; 2]; 2];
            let mut translate = [0.0f32; 2];
            let mut values = [0.0f32; 6];
            for value in values.iter_mut() {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    self.warn(format!("missing transform parms in shader '{name}'"));
                    stage.bundle.tex_mods[slot] = TexMod::Transform { matrix, translate };
                    return Ok(());
                }
                *value = atof(&token);
            }
            matrix[0][0] = values[0];
            matrix[0][1] = values[1];
            matrix[1][0] = values[2];
            matrix[1][1] = values[3];
            translate = [values[4], values[5]];
            stage.bundle.tex_mods[slot] = TexMod::Transform { matrix, translate };
        } else if kind.eq_ignore_ascii_case("rotate") {
            let token = self.next(false).to_string();
            if token.is_empty() {
                self.warn(format!("missing tcMod rotate parms in shader '{name}'"));
                return Ok(());
            }
            stage.bundle.tex_mods[slot] = TexMod::Rotate {
                speed: atof(&token),
            };
        } else if kind.eq_ignore_ascii_case("entitytranslate") {
            stage.bundle.tex_mods[slot] = TexMod::EntityTranslate;
        } else {
            self.warn(format!("unknown tcMod '{kind}' in shader '{name}'"));
        }
        Ok(())
    }

    /// `ParseVector`: `( v0 v1 ... )` written into `out`. Returns false
    /// on a missing paren or element after warning, leaving whatever
    /// was read so far — the C fills the caller's buffer as it goes.
    fn parse_vector(&mut self, count: usize, out: &mut [f32], name: &str) -> bool {
        let token = self.next(false).to_string();
        if token != "(" {
            self.warnings
                .push(format!("missing parenthesis in shader '{name}'"));
            return false;
        }
        for i in 0..count {
            let token = self.next(false).to_string();
            if token.is_empty() {
                self.warnings
                    .push(format!("missing vector element in shader '{name}'"));
                return false;
            }
            out[i] = atof(&token);
        }
        let token = self.next(false).to_string();
        if token != ")" {
            self.warnings
                .push(format!("missing parenthesis in shader '{name}'"));
            return false;
        }
        true
    }

    /// `ParseShader`: reads one shader body — the cursor sits where
    /// the C's `FindShaderInShaderText` leaves it, on the opening `{`.
    /// The name must already be set on `script`.
    pub fn parse_shader(&mut self, script: &mut ShaderScript) -> Result<(), ShaderError> {
        let name = script.name.clone();
        let token = self.next(true).to_string();
        if !token.starts_with('{') {
            self.warn(format!(
                "expecting '{{', found '{token}' instead in shader '{name}'"
            ));
            return Err(ShaderError::ExpectedOpenBrace(token));
        }

        loop {
            let token = self.next(true).to_string();
            if token.is_empty() {
                self.warn(format!("no concluding '}}' in shader {name}"));
                return Err(ShaderError::UnterminatedShader);
            }
            // The C tests token[0] only for the braces.
            if token.starts_with('}') {
                break;
            }
            if token.starts_with('{') {
                self.parse_stage(script)?;
                continue;
            }

            let lower = token.to_ascii_lowercase();

            // Skip stuff that only the QuakeEdRadiant needs.
            if lower.starts_with("qer") {
                self.skip_rest_of_line();
                continue;
            }
            if lower == "q3map_sun" {
                self.parse_q3map_sun(script);
                continue;
            }
            if lower == "deformvertexes" {
                self.parse_deform(script, &name);
                continue;
            }
            if lower == "tesssize" {
                self.skip_rest_of_line();
                continue;
            }
            if lower == "clamptime" {
                let token = self.next(false).to_string();
                if !token.is_empty() {
                    script.clamp_time = Some(atof(&token));
                }
                continue;
            }
            // Skip stuff that only the q3map needs.
            if lower.starts_with("q3map") {
                self.skip_rest_of_line();
                continue;
            }
            if lower == "surfaceparm" {
                self.parse_surface_parm(script);
                continue;
            }
            if lower == "nomipmaps" {
                script.no_mip_maps = true;
                script.no_pic_mip = true;
                continue;
            }
            if lower == "nopicmip" {
                script.no_pic_mip = true;
                continue;
            }
            if lower == "polygonoffset" {
                script.polygon_offset = true;
                continue;
            }
            // Allow sprite surfaces from multiple entities to merge.
            if lower == "entitymergable" {
                script.entity_mergable = true;
                continue;
            }
            if lower == "fogparms" {
                let mut color = [0.0f32; 3];
                if !self.parse_vector(3, &mut color, &name) {
                    return Err(ShaderError::MissingVector);
                }
                script.fog = Some(FogParms {
                    color,
                    depth_for_opaque: 0.0,
                });
                let token = self.next(false).to_string();
                if token.is_empty() {
                    self.warn(format!(
                        "missing parm for 'fogParms' keyword in shader '{name}'"
                    ));
                    // The C continues *without* SkipRestOfLine here.
                    continue;
                }
                script.fog.as_mut().unwrap().depth_for_opaque = atof(&token);
                // Skip any old gradient directions.
                self.skip_rest_of_line();
                continue;
            }
            if lower == "portal" {
                script.sort = ShaderSort::PORTAL;
                continue;
            }
            if lower == "skyparms" {
                self.parse_sky_parms(script, &name);
                continue;
            }
            // light <value> determines flaring in q3map.
            if lower == "light" {
                self.next(false);
                continue;
            }
            if lower == "cull" {
                let token = self.next(false).to_string();
                if token.is_empty() {
                    self.warn(format!("missing cull parms in shader '{name}'"));
                    continue;
                }
                if ["none", "twosided", "disable"]
                    .iter()
                    .any(|k| token.eq_ignore_ascii_case(k))
                {
                    script.cull = CullType::TwoSided;
                } else if ["back", "backside", "backsided"]
                    .iter()
                    .any(|k| token.eq_ignore_ascii_case(k))
                {
                    script.cull = CullType::BackSided;
                } else {
                    self.warn(format!("invalid cull parm '{token}' in shader '{name}'"));
                }
                continue;
            }
            if lower == "sort" {
                self.parse_sort(script, &name);
                continue;
            }
            self.warn(format!(
                "unknown general shader parameter '{token}' in '{name}'"
            ));
            return Err(ShaderError::UnknownShaderParameter(token));
        }

        // Ignore shaders that don't have any stages, unless it is a sky
        // or fog.
        if script.stages.is_empty()
            && script.sky.is_none()
            && script.content_flags & CONTENTS_FOG == 0
        {
            return Err(ShaderError::NoStages);
        }

        script.explicitly_defined = true;
        Ok(())
    }

    /// `ParseDeform` — `deformVertexes <subtype> ...`. As in C, the
    /// subtype is read before the `MAX_SHADER_DEFORMS` check, and a
    /// failure after the slot is reserved leaves a zeroed `Deform::None`
    /// entry behind.
    fn parse_deform(&mut self, script: &mut ShaderScript, name: &str) {
        let token = self.next(false).to_string();
        if token.is_empty() {
            self.warn(format!("missing deform parm in shader '{name}'"));
            return;
        }
        if script.deforms.len() >= MAX_SHADER_DEFORMS {
            self.warn(format!("MAX_SHADER_DEFORMS in '{name}'"));
            return;
        }
        script.deforms.push(DeformStage::default());
        let slot = script.deforms.len() - 1;

        if token.eq_ignore_ascii_case("projectionshadow") {
            script.deforms[slot].deform = Deform::ProjectionShadow;
            return;
        }
        if token.eq_ignore_ascii_case("autosprite") {
            script.deforms[slot].deform = Deform::Autosprite;
            return;
        }
        if token.eq_ignore_ascii_case("autosprite2") {
            script.deforms[slot].deform = Deform::Autosprite2;
            return;
        }
        if token.len() >= 4 && token.as_bytes()[..4].eq_ignore_ascii_case(b"text") {
            // token[4] - '0' in C: past the end of the token reads as
            // '\0', so plain "text" and out-of-range digits land on 0.
            let n = token
                .as_bytes()
                .get(4)
                .map_or(-1, |b| *b as i32 - b'0' as i32);
            let n = if (0..=7).contains(&n) { n as usize } else { 0 };
            script.deforms[slot].deform = match n {
                0 => Deform::Text0,
                1 => Deform::Text1,
                2 => Deform::Text2,
                3 => Deform::Text3,
                4 => Deform::Text4,
                5 => Deform::Text5,
                6 => Deform::Text6,
                _ => Deform::Text7,
            };
            return;
        }
        if token.eq_ignore_ascii_case("bulge") {
            let Some(width) = self.deform_parm(name, "bulge ") else {
                return;
            };
            script.deforms[slot].bulge_width = width;
            let Some(height) = self.deform_parm(name, "bulge ") else {
                return;
            };
            script.deforms[slot].bulge_height = height;
            let Some(speed) = self.deform_parm(name, "bulge ") else {
                return;
            };
            script.deforms[slot].bulge_speed = speed;
            script.deforms[slot].deform = Deform::Bulge;
            return;
        }
        if token.eq_ignore_ascii_case("wave") {
            let Some(div) = self.deform_parm(name, "") else {
                return;
            };
            if div != 0.0 {
                script.deforms[slot].spread = 1.0 / div;
            } else {
                script.deforms[slot].spread = 100.0;
                self.warn(format!(
                    "illegal div value of 0 in deformVertexes command for shader '{name}'"
                ));
            }
            script.deforms[slot].wave = self.parse_wave_form(name);
            script.deforms[slot].deform = Deform::Wave;
            return;
        }
        if token.eq_ignore_ascii_case("normal") {
            let Some(amplitude) = self.deform_parm(name, "") else {
                return;
            };
            script.deforms[slot].wave.amplitude = amplitude;
            let Some(frequency) = self.deform_parm(name, "") else {
                return;
            };
            script.deforms[slot].wave.frequency = frequency;
            script.deforms[slot].deform = Deform::Normals;
            return;
        }
        if token.eq_ignore_ascii_case("move") {
            for axis in 0..3 {
                let Some(value) = self.deform_parm(name, "") else {
                    return;
                };
                script.deforms[slot].move_vector[axis] = value;
            }
            script.deforms[slot].wave = self.parse_wave_form(name);
            script.deforms[slot].deform = Deform::Move;
            return;
        }
        self.warn(format!(
            "unknown deformVertexes subtype '{token}' found in shader '{name}'"
        ));
    }

    /// One of `ParseDeform`'s float parameters: `None` after warning,
    /// with the C's message (`what` inserts the subtype, if any).
    fn deform_parm(&mut self, name: &str, what: &str) -> Option<f32> {
        let token = self.next(false).to_string();
        if token.is_empty() {
            self.warn(format!(
                "missing deformVertexes {what}parm in shader '{name}'"
            ));
            None
        } else {
            Some(atof(&token))
        }
    }

    /// `ParseSkyParms` — `skyParms <outerbox> <cloudheight> <innerbox>`,
    /// where `-` means no box. The sky flag goes on only at the very
    /// end, which is where the C sets `is_sky`; `R_InitSkyTexCoords`
    /// runs there in C and is deferred here.
    fn parse_sky_parms(&mut self, script: &mut ShaderScript, name: &str) {
        let mut sky = SkyParms::default();

        let outer = self.next(false).to_string();
        if outer.is_empty() {
            self.warn(format!("'skyParms' missing parameter in shader '{name}'"));
            return;
        }
        if outer != "-" {
            sky.outerbox = Some(SkyParms::box_names(&outer));
        }

        let cloud = self.next(false).to_string();
        if cloud.is_empty() {
            self.warn(format!("'skyParms' missing parameter in shader '{name}'"));
            return;
        }
        let height = atof(&cloud);
        sky.cloud_height = if height == 0.0 { 512.0 } else { height };

        let inner = self.next(false).to_string();
        if inner.is_empty() {
            self.warn(format!("'skyParms' missing parameter in shader '{name}'"));
            return;
        }
        if inner != "-" {
            sky.innerbox = Some(SkyParms::box_names(&inner));
        }

        script.sky = Some(sky);
    }

    /// `ParseSort`. Named keywords map to `shaderSort_t` constants;
    /// anything else is stored as the raw number, exactly as
    /// `shader.sort = atof(token)` stores it.
    fn parse_sort(&mut self, script: &mut ShaderScript, name: &str) {
        let token = self.next(false).to_string();
        if token.is_empty() {
            self.warn(format!("missing sort parameter in shader '{name}'"));
            return;
        }
        script.sort = if token.eq_ignore_ascii_case("portal") {
            ShaderSort::PORTAL
        } else if token.eq_ignore_ascii_case("sky") {
            ShaderSort::ENVIRONMENT
        } else if token.eq_ignore_ascii_case("opaque") {
            ShaderSort::OPAQUE
        } else if token.eq_ignore_ascii_case("decal") {
            ShaderSort::DECAL
        } else if token.eq_ignore_ascii_case("seethrough") {
            ShaderSort::SEE_THROUGH
        } else if token.eq_ignore_ascii_case("banner") {
            ShaderSort::BANNER
        } else if token.eq_ignore_ascii_case("additive") {
            ShaderSort::BLEND1
        } else if token.eq_ignore_ascii_case("nearest") {
            ShaderSort::NEAREST
        } else if token.eq_ignore_ascii_case("underwater") {
            ShaderSort::UNDERWATER
        } else {
            ShaderSort(atof(&token) as i32)
        };
    }

    /// `ParseSurfaceParm` — ORs the `infoParms` entry into the shader.
    /// Unknown names (and an empty token) are silently ignored, as in C.
    fn parse_surface_parm(&mut self, script: &mut ShaderScript) {
        let token = self.next(false).to_string();
        for (parm, surface, contents) in INFO_PARMS {
            if token.eq_ignore_ascii_case(parm) {
                script.surface_flags |= surface;
                script.content_flags |= contents;
                break;
            }
        }
    }

    /// `q3map_sun <r> <g> <b> <intensity> <yaw> <pitch>` — the sun
    /// globals `tr.sunLight`/`tr.sunDirection`, kept per shader. The
    /// colour is normalized then scaled; the direction comes from the
    /// two angles, whose trigonometry runs in `double` in the C, so it
    /// does here too.
    fn parse_q3map_sun(&mut self, script: &mut ShaderScript) {
        let mut light = [0.0f32; 3];
        for component in &mut light {
            *component = atof(self.next(false));
        }
        let len = (light[0] * light[0] + light[1] * light[1] + light[2] * light[2]).sqrt();
        if len > 0.0 {
            for component in &mut light {
                *component /= len;
            }
        }
        let intensity = atof(self.next(false)) as f64;
        for component in &mut light {
            *component = (*component as f64 * intensity) as f32;
        }

        let a = atof(self.next(false)) as f64 / 180.0 * std::f64::consts::PI;
        let b = atof(self.next(false)) as f64 / 180.0 * std::f64::consts::PI;
        script.sun = Some(SunParms {
            light,
            direction: [
                (a.cos() * b.cos()) as f32,
                (a.sin() * b.cos()) as f32,
                b.sin() as f32,
            ],
        });
    }
}

/// `COM_Compress`: strips both comment styles, collapses runs of
/// newlines to one `\n` and runs of spaces/tabs to one space ahead of
/// the next token, keeps quoted strings verbatim, and drops trailing
/// whitespace. `ScanAndLoadShaderFiles` runs it over each file as it is
/// appended.
pub fn compress_text(text: &str) -> String {
    let data = text.as_bytes();
    // Bytes are only ever deleted, and always at ASCII boundaries that
    // cannot sit inside a multi-byte character, so the result is still
    // valid UTF-8.
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut i = 0;
    let mut newline = false;
    let mut whitespace = false;

    while i < data.len() {
        let c = data[i];
        if c == b'/' && data.get(i + 1) == Some(&b'/') {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && data.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < data.len() && !(data[i] == b'*' && data.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            if i < data.len() {
                i += 2;
            }
        } else if c == b'\n' || c == b'\r' {
            newline = true;
            i += 1;
        } else if c == b' ' || c == b'\t' {
            whitespace = true;
            i += 1;
        } else {
            if newline {
                out.push(b'\n');
                newline = false;
                whitespace = false;
            }
            if whitespace {
                out.push(b' ');
                whitespace = false;
            }
            if c == b'"' {
                out.push(b'"');
                i += 1;
                while i < data.len() && data[i] != b'"' {
                    out.push(data[i]);
                    i += 1;
                }
                if i < data.len() {
                    out.push(b'"');
                    i += 1;
                }
            } else {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `generateHashValue`: the renderer's name hash — lowercased, stopping
/// at the extension, `\` folded to `/`, `letter * (i + 119)` summed in
/// `long`, folded by `hash ^ hash>>10 ^ hash>>20`, then masked to
/// `size`. (`PATH_SEP` is `/` on this platform, so its conversion is a
/// no-op after the backslash fold; `tolower` on non-ASCII bytes is
/// skipped — shader names are ASCII.)
pub fn generate_hash_value(name: &str, size: usize) -> usize {
    let bytes = name.as_bytes();
    let mut hash: i64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let mut letter = bytes[i].to_ascii_lowercase();
        if letter == b'.' {
            break;
        }
        if letter == b'\\' {
            letter = b'/';
        }
        hash += letter as i64 * (i as i64 + 119);
        i += 1;
    }
    hash ^= hash >> 10;
    hash ^= hash >> 20;
    (hash & (size as i64 - 1)) as usize
}

/// The combined text of every `.shader` file plus its label hash table:
/// `ScanAndLoadShaderFiles` builds this, `FindShaderInShaderText`
/// queries it.
pub struct ShaderTextIndex {
    text: String,
    /// Per-bucket label offsets into `text`, in text order.
    buckets: Vec<Vec<usize>>,
}

impl ShaderTextIndex {
    /// `ScanAndLoadShaderFiles`: the files are concatenated in the C's
    /// order — reverse, each preceded by a newline and `COM_Compress`ed
    /// — then every label is indexed. The C's per-file boundary guard
    /// trips on that reversed layout and indexes only the first shader
    /// of all but one file, which `FindShaderInShaderText`'s linear
    /// fallback masks; the port simply indexes everything.
    pub fn build(files: &[&str]) -> ShaderTextIndex {
        let files = &files[..files.len().min(MAX_SHADER_FILES)];

        let mut text = String::new();
        for file in files.iter().rev() {
            text.push('\n');
            text.push_str(&compress_text(file));
        }

        let mut buckets = vec![Vec::new(); MAX_SHADERTEXT_HASH];
        let mut parser = Parser::new(&text);
        loop {
            let old = parser.position();
            let token = parser.next(true);
            if token.is_empty() {
                break;
            }
            buckets[generate_hash_value(token, MAX_SHADERTEXT_HASH)].push(old);
            parser.skip_braced_section();
        }

        ShaderTextIndex { text, buckets }
    }

    /// The combined shader text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// `FindShaderInShaderText`: the text following the shader's name
    /// label — where `ParseShader` picks up — or `None`. The hash
    /// bucket is tried first and the whole text scanned as the
    /// fallback, exactly as the C walks it; names are matched
    /// case-insensitively (`Q_stricmp`).
    pub fn find(&self, name: &str) -> Option<&str> {
        for &offset in &self.buckets[generate_hash_value(name, MAX_SHADERTEXT_HASH)] {
            let mut parser = Parser {
                data: self.text.as_bytes(),
                pos: offset,
                warnings: Vec::new(),
            };
            let token = parser.next(true);
            if token.eq_ignore_ascii_case(name) {
                return Some(&self.text[parser.position()..]);
            }
        }

        let mut parser = Parser::new(&self.text);
        loop {
            let token = parser.next(true);
            if token.is_empty() {
                return None;
            }
            if token.eq_ignore_ascii_case(name) {
                return Some(&self.text[parser.position()..]);
            }
            parser.skip_braced_section();
        }
    }
}

/// `atof`: the token parsed as a float, or 0.0 when it does not parse
/// (which is what `atof` returns on a failed conversion). The C also
/// accepts a float *prefix* — "1.5x" reads as 1.5 there — but shader
/// scripts never rely on that.
fn atof(token: &str) -> f32 {
    token.parse::<f32>().unwrap_or(0.0)
}

/// `NameToGenFunc`; unknown names warn and fall back to sin, as in C.
fn name_to_gen_func(token: &str, name: &str, warnings: &mut Vec<String>) -> GenFunc {
    if token.eq_ignore_ascii_case("sin") {
        GenFunc::Sin
    } else if token.eq_ignore_ascii_case("square") {
        GenFunc::Square
    } else if token.eq_ignore_ascii_case("triangle") {
        GenFunc::Triangle
    } else if token.eq_ignore_ascii_case("sawtooth") {
        GenFunc::Sawtooth
    } else if token.eq_ignore_ascii_case("inversesawtooth") {
        GenFunc::InverseSawtooth
    } else if token.eq_ignore_ascii_case("noise") {
        GenFunc::Noise
    } else {
        warnings.push(format!("invalid genfunc name '{token}' in shader '{name}'"));
        GenFunc::Sin
    }
}

/// `NameToAFunc`; unknown names warn and return 0 (no alpha test).
fn name_to_afunc(token: &str, name: &str, warnings: &mut Vec<String>) -> u32 {
    if token.eq_ignore_ascii_case("GT0") {
        GLS_ATEST_GT_0
    } else if token.eq_ignore_ascii_case("LT128") {
        GLS_ATEST_LT_80
    } else if token.eq_ignore_ascii_case("GE128") {
        GLS_ATEST_GE_80
    } else {
        warnings.push(format!(
            "invalid alphaFunc name '{token}' in shader '{name}'"
        ));
        0
    }
}

/// `NameToSrcBlendMode`; unknown names warn and substitute GL_ONE.
fn name_to_src_blend_mode(token: &str, name: &str, warnings: &mut Vec<String>) -> u32 {
    if token.eq_ignore_ascii_case("GL_ONE") {
        GLS_SRCBLEND_ONE
    } else if token.eq_ignore_ascii_case("GL_ZERO") {
        GLS_SRCBLEND_ZERO
    } else if token.eq_ignore_ascii_case("GL_DST_COLOR") {
        GLS_SRCBLEND_DST_COLOR
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_DST_COLOR") {
        GLS_SRCBLEND_ONE_MINUS_DST_COLOR
    } else if token.eq_ignore_ascii_case("GL_SRC_ALPHA") {
        GLS_SRCBLEND_SRC_ALPHA
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_SRC_ALPHA") {
        GLS_SRCBLEND_ONE_MINUS_SRC_ALPHA
    } else if token.eq_ignore_ascii_case("GL_DST_ALPHA") {
        GLS_SRCBLEND_DST_ALPHA
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_DST_ALPHA") {
        GLS_SRCBLEND_ONE_MINUS_DST_ALPHA
    } else if token.eq_ignore_ascii_case("GL_SRC_ALPHA_SATURATE") {
        GLS_SRCBLEND_ALPHA_SATURATE
    } else {
        warnings.push(format!(
            "unknown blend mode '{token}' in shader '{name}', substituting GL_ONE"
        ));
        GLS_SRCBLEND_ONE
    }
}

/// `NameToDstBlendMode`; unknown names warn and substitute GL_ONE.
fn name_to_dst_blend_mode(token: &str, name: &str, warnings: &mut Vec<String>) -> u32 {
    if token.eq_ignore_ascii_case("GL_ONE") {
        GLS_DSTBLEND_ONE
    } else if token.eq_ignore_ascii_case("GL_ZERO") {
        GLS_DSTBLEND_ZERO
    } else if token.eq_ignore_ascii_case("GL_SRC_ALPHA") {
        GLS_DSTBLEND_SRC_ALPHA
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_SRC_ALPHA") {
        GLS_DSTBLEND_ONE_MINUS_SRC_ALPHA
    } else if token.eq_ignore_ascii_case("GL_DST_ALPHA") {
        GLS_DSTBLEND_DST_ALPHA
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_DST_ALPHA") {
        GLS_DSTBLEND_ONE_MINUS_DST_ALPHA
    } else if token.eq_ignore_ascii_case("GL_SRC_COLOR") {
        GLS_DSTBLEND_SRC_COLOR
    } else if token.eq_ignore_ascii_case("GL_ONE_MINUS_SRC_COLOR") {
        GLS_DSTBLEND_ONE_MINUS_SRC_COLOR
    } else {
        warnings.push(format!(
            "unknown blend mode '{token}' in shader '{name}', substituting GL_ONE"
        ));
        GLS_DSTBLEND_ONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses one stage body — the text *after* the opening `{`, which
    /// is where `ParseShader` leaves the cursor before calling
    /// `ParseStage` — and returns the script plus the warnings.
    fn stage(text: &str) -> Result<(ShaderScript, Vec<String>), ShaderError> {
        let mut script = ShaderScript {
            name: "tests/stage".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(text);
        let result = parser.parse_stage(&mut script);
        let warnings = parser.warnings;
        result.map(|()| (script, warnings))
    }

    fn only_stage(text: &str) -> ShaderStage {
        let (script, _) = stage(text).unwrap();
        assert_eq!(script.stages.len(), 1);
        script.stages.into_iter().next().unwrap()
    }

    #[test]
    fn lexes_tokens_like_com_parse_ext() {
        let mut p = Parser::new("  foo\nbar { // comment\n } /* block */ baz \"quoted str\"");
        assert_eq!(p.next(true), "foo");
        assert_eq!(p.next(true), "bar");
        assert_eq!(p.next(true), "{");
        // A newline stops a non-line-breaking read, even when a comment
        // was skipped first — the C's flag is sticky for the whole call.
        assert_eq!(p.next(false), "");
        assert_eq!(p.next(true), "}");
        assert_eq!(p.next(true), "baz");
        assert_eq!(p.next(true), "quoted str");
        assert_eq!(p.next(true), "");
        assert!(p.at_end());
    }

    #[test]
    fn a_comment_before_a_token_does_not_hide_the_following_line() {
        let mut p = Parser::new("// header\nfoo");
        assert_eq!(p.next(false), "");
        assert_eq!(p.next(false), "foo");
        // Without line breaks allowed, a comment mid-line still lets the
        // next token through.
        let mut p = Parser::new("a // tail\nb");
        assert_eq!(p.next(false), "a");
        assert_eq!(p.next(false), "");
        assert_eq!(p.next(false), "b");
    }

    #[test]
    fn over_long_words_are_discarded_but_quotes_truncate() {
        let long = "b".repeat(1024);
        let text = format!("{long} c");
        let mut p = Parser::new(&text);
        // The C drops a token of MAX_TOKEN_CHARS outright...
        assert_eq!(p.next(true), "");
        assert_eq!(p.next(true), "c");

        // ...but one character less survives.
        let kept = "d".repeat(1023);
        let text = format!("{kept} e");
        let mut p = Parser::new(&text);
        assert_eq!(p.next(true), kept);

        // Quoted strings truncate to the buffer instead of being lost.
        let quoted = format!("\"{}\" x", "a".repeat(1100));
        let mut p = Parser::new(&quoted);
        assert_eq!(p.next(true).len(), 1024);
        assert_eq!(p.next(true), "x");
    }

    #[test]
    fn skip_rest_of_line_consumes_through_the_newline() {
        let mut p = Parser::new("qer_info \"foo bar\"\nnext");
        assert_eq!(p.next(true), "qer_info");
        p.skip_rest_of_line();
        assert_eq!(p.next(true), "next");

        // At end of text it clamps instead of stepping past the end.
        let mut p = Parser::new("abc");
        assert_eq!(p.next(true), "abc");
        p.skip_rest_of_line();
        assert!(p.at_end());
    }

    #[test]
    fn a_plain_map_stage_defaults_to_identity_lighting() {
        let (script, warnings) = stage("map textures/foo/bar }").unwrap();
        assert!(warnings.is_empty());
        let s = &script.stages[0];
        assert!(s.active);
        assert_eq!(
            s.bundle.map,
            Some(TextureMap::File {
                name: "textures/foo/bar".into(),
                clamp: false
            })
        );
        assert_eq!(s.rgb_gen, ColorGen::IdentityLighting);
        assert_eq!(s.alpha_gen, AlphaGen::Identity);
        assert_eq!(s.bundle.tc_gen, TexCoordGen::Bad);
        assert_eq!(s.state_bits, GLS_DEPTHMASK_TRUE);
        assert!(!s.is_detail);
    }

    #[test]
    fn lightmap_whiteimage_and_clampmap_bind() {
        assert_eq!(
            only_stage("map $lightmap }").bundle.map,
            Some(TextureMap::Lightmap)
        );
        assert_eq!(
            only_stage("map $whiteimage }").bundle.map,
            Some(TextureMap::WhiteImage)
        );
        assert_eq!(
            only_stage("clampmap textures/foo/clamp }").bundle.map,
            Some(TextureMap::File {
                name: "textures/foo/clamp".into(),
                clamp: true
            })
        );
        let s = only_stage("detail map textures/foo }");
        assert!(s.is_detail);
    }

    #[test]
    fn blendfunc_selects_the_simple_and_complex_blends() {
        // add = GL_ONE GL_ONE; the depth mask clears for blended stages.
        let s = only_stage("blendfunc add }");
        assert_eq!(s.state_bits, GLS_SRCBLEND_ONE | GLS_DSTBLEND_ONE);
        assert_eq!(s.rgb_gen, ColorGen::IdentityLighting);

        // filter's source is not one of the identity-friendly modes, so
        // the rgbGen default flips to identity.
        let s = only_stage("blendfunc filter }");
        assert_eq!(s.state_bits, GLS_SRCBLEND_DST_COLOR | GLS_DSTBLEND_ZERO);
        assert_eq!(s.rgb_gen, ColorGen::Identity);

        let s = only_stage("blendfunc GL_SRC_ALPHA GL_ONE_MINUS_SRC_ALPHA }");
        assert_eq!(
            s.state_bits,
            GLS_SRCBLEND_SRC_ALPHA | GLS_DSTBLEND_ONE_MINUS_SRC_ALPHA
        );

        // GL_ONE GL_ZERO disables blending entirely and restores the
        // depth mask.
        let s = only_stage("blendfunc GL_ONE GL_ZERO }");
        assert_eq!(s.state_bits, GLS_DEPTHMASK_TRUE);
        assert_eq!(s.rgb_gen, ColorGen::IdentityLighting);

        // An explicit depthwrite before blendfunc survives its mask
        // clearing.
        let s = only_stage("depthwrite blendfunc add }");
        assert_eq!(
            s.state_bits,
            GLS_DEPTHMASK_TRUE | GLS_SRCBLEND_ONE | GLS_DSTBLEND_ONE
        );
    }

    #[test]
    fn alphafunc_and_depthfunc_contribute_state_bits() {
        let s = only_stage("alphafunc GE128 depthfunc equal }");
        assert_eq!(
            s.state_bits,
            GLS_DEPTHMASK_TRUE | GLS_ATEST_GE_80 | GLS_DEPTHFUNC_EQUAL
        );
        assert_eq!(
            only_stage("alphafunc GT0 }").state_bits,
            GLS_DEPTHMASK_TRUE | GLS_ATEST_GT_0
        );
        assert_eq!(
            only_stage("alphafunc LT128 }").state_bits,
            GLS_DEPTHMASK_TRUE | GLS_ATEST_LT_80
        );
        // lequal is the zero default; unknown names warn and do nothing.
        assert_eq!(
            only_stage("depthfunc lequal }").state_bits,
            GLS_DEPTHMASK_TRUE
        );
        let (_, warnings) = stage("alphafunc GE99 depthfunc lequalish }").unwrap();
        assert!(warnings
            .iter()
            .any(|w| w == "invalid alphaFunc name 'GE99' in shader 'tests/stage'"));
        assert!(warnings
            .iter()
            .any(|w| w == "unknown depthfunc 'lequalish' in shader 'tests/stage'"));
    }

    #[test]
    fn rgbgen_variants_parse() {
        let s = only_stage("rgbGen wave sin 0 1 0 2 }");
        assert_eq!(s.rgb_gen, ColorGen::Waveform);
        assert_eq!(
            s.rgb_wave,
            WaveForm {
                func: GenFunc::Sin,
                base: 0.0,
                amplitude: 1.0,
                phase: 0.0,
                frequency: 2.0
            }
        );

        let s = only_stage("rgbGen const ( 1 0.5 0 ) }");
        assert_eq!(s.rgb_gen, ColorGen::Const);
        assert_eq!(s.constant_color, [255, 127, 0, 0]);

        // rgbGen vertex implies alphaGen vertex while alphaGen is still
        // its zero default.
        let s = only_stage("rgbGen vertex }");
        assert_eq!(s.rgb_gen, ColorGen::Vertex);
        assert_eq!(s.alpha_gen, AlphaGen::Vertex);

        assert_eq!(only_stage("rgbGen identity }").rgb_gen, ColorGen::Identity);
        assert_eq!(
            only_stage("rgbGen identityLighting }").rgb_gen,
            ColorGen::IdentityLighting
        );
        assert_eq!(only_stage("rgbGen entity }").rgb_gen, ColorGen::Entity);
        assert_eq!(
            only_stage("rgbGen oneMinusEntity }").rgb_gen,
            ColorGen::OneMinusEntity
        );
        assert_eq!(
            only_stage("rgbGen exactVertex }").rgb_gen,
            ColorGen::ExactVertex
        );
        assert_eq!(
            only_stage("rgbGen lightingDiffuse }").rgb_gen,
            ColorGen::LightingDiffuse
        );
        assert_eq!(
            only_stage("rgbGen oneMinusVertex }").rgb_gen,
            ColorGen::OneMinusVertex
        );

        // Unknown names warn but keep the default; a missing parameter
        // warns and skips the keyword entirely.
        let s = only_stage("rgbGen sparkly }");
        assert_eq!(s.rgb_gen, ColorGen::IdentityLighting);
        let (script, warnings) = stage("rgbGen\n map textures/foo }").unwrap();
        assert!(warnings
            .iter()
            .any(|w| w == "missing parameters for rgbGen in shader 'tests/stage'"));
        assert_eq!(script.stages[0].rgb_gen, ColorGen::IdentityLighting);
    }

    #[test]
    fn rgbgen_const_tolerates_a_missing_paren_like_the_c() {
        // ParseVector's result is ignored here: the elements read so
        // far land in the constant color, and the warning is collected.
        let s = only_stage("rgbGen const ( 1 0\n map textures/foo }");
        assert_eq!(s.rgb_gen, ColorGen::Const);
        assert_eq!(s.constant_color, [255, 0, 0, 0]);
    }

    #[test]
    fn alphagen_variants_and_the_portal_range() {
        let s = only_stage("alphaGen const 0.5 }");
        assert_eq!(s.alpha_gen, AlphaGen::Const);
        assert_eq!(s.constant_color, [0, 0, 0, 127]);

        let (script, warnings) = stage("alphaGen portal 512 }").unwrap();
        assert_eq!(script.stages[0].alpha_gen, AlphaGen::Portal);
        assert_eq!(script.portal_range, Some(512.0));
        assert!(warnings.is_empty());

        // A missing range defaults to 256 with a warning, and the
        // newline after `portal` is what makes the range "missing".
        let (script, warnings) = stage("alphaGen portal\n map textures/foo }").unwrap();
        assert_eq!(script.portal_range, Some(256.0));
        assert!(warnings
            .iter()
            .any(|w| w.starts_with("missing range parameter for alphaGen portal")));

        let s = only_stage("alphaGen lightingSpecular }");
        assert_eq!(s.alpha_gen, AlphaGen::LightingSpecular);
        let (_, warnings) = stage("alphaGen shimmery }").unwrap();
        assert!(warnings
            .iter()
            .any(|w| w == "unknown alphaGen parameter 'shimmery' in shader 'tests/stage'"));
    }

    #[test]
    fn the_alpha_gen_skip_check_reproduces_the_c_enum_quirk() {
        // The C compares alphaGen against CGEN_IDENTITY (2), which is
        // AGEN_ENTITY — so entity gets skipped, identity does not.
        let s = only_stage("rgbGen identity\n alphaGen entity }");
        assert_eq!(s.alpha_gen, AlphaGen::Skip);

        let s = only_stage("rgbGen lightingDiffuse\n alphaGen entity }");
        assert_eq!(s.alpha_gen, AlphaGen::Skip);

        // Default rgbGen (identityLighting) is not in the skip set.
        let s = only_stage("alphaGen entity }");
        assert_eq!(s.alpha_gen, AlphaGen::Entity);

        // Nor is an explicit rgbGen wave.
        let s = only_stage("rgbGen wave sin 0 0 0 1\n alphaGen entity }");
        assert_eq!(s.alpha_gen, AlphaGen::Entity);
    }

    #[test]
    fn tcgen_selects_the_generator() {
        assert_eq!(
            only_stage("tcGen environment }").bundle.tc_gen,
            TexCoordGen::EnvironmentMapped
        );
        assert_eq!(
            only_stage("texgen lightmap }").bundle.tc_gen,
            TexCoordGen::Lightmap
        );
        assert_eq!(
            only_stage("tcGen texture }").bundle.tc_gen,
            TexCoordGen::Texture
        );
        assert_eq!(
            only_stage("tcGen base }").bundle.tc_gen,
            TexCoordGen::Texture
        );

        let s = only_stage("tcGen vector ( 1 0 0 ) ( 0 1 0 ) }");
        assert_eq!(s.bundle.tc_gen, TexCoordGen::Vector);
        assert_eq!(s.bundle.tc_gen_vectors, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);

        // Unknown generators warn without failing the stage.
        let s = only_stage("tcGen wobbly }");
        assert_eq!(s.bundle.tc_gen, TexCoordGen::Bad);
    }

    #[test]
    fn tcmods_parse_in_order_up_to_the_limit() {
        let s = only_stage(
            "map textures/foo \
             tcMod scroll 0.1 -0.2 \
             tcMod scale 0.5 0.25 \
             tcMod rotate 45 \
             tcMod turb 0 3 0 1.5 }",
        );
        assert_eq!(
            s.bundle.tex_mods,
            vec![
                TexMod::Scroll {
                    scroll: [0.1, -0.2]
                },
                TexMod::Scale { scale: [0.5, 0.25] },
                TexMod::Rotate { speed: 45.0 },
                TexMod::Turbulent {
                    wave: WaveForm {
                        func: GenFunc::None,
                        base: 0.0,
                        amplitude: 3.0,
                        phase: 0.0,
                        frequency: 1.5
                    }
                },
            ]
        );

        let s = only_stage(
            "tcMod stretch sin 0.25 0.35 0.45 0.55 \
             tcMod transform 1 0 0 1 0.5 0.25 \
             tcMod entityTranslate }",
        );
        assert_eq!(
            s.bundle.tex_mods,
            vec![
                TexMod::Stretch {
                    wave: WaveForm {
                        func: GenFunc::Sin,
                        base: 0.25,
                        amplitude: 0.35,
                        phase: 0.45,
                        frequency: 0.55
                    }
                },
                TexMod::Transform {
                    matrix: [[1.0, 0.0], [0.0, 1.0]],
                    translate: [0.5, 0.25]
                },
                TexMod::EntityTranslate,
            ]
        );
    }

    #[test]
    fn the_fifth_tcmod_is_refused() {
        let text = "tcMod rotate 1 tcMod rotate 2 tcMod rotate 3 tcMod rotate 4 tcMod rotate 5 }";
        assert_eq!(stage(text), Err(ShaderError::TooManyTexMods));
    }

    #[test]
    fn a_broken_tcmod_keeps_its_slot_and_warns() {
        // A failed tcMod still consumes a TR_MAX_TEXMODS entry in the C
        // (the zeroed TMOD_NONE), and partial parameters survive.
        let (script, warnings) = stage("tcMod wobble }").unwrap();
        assert_eq!(script.stages[0].bundle.tex_mods, vec![TexMod::None]);
        assert!(warnings
            .iter()
            .any(|w| w == "unknown tcMod 'wobble' in shader 'tests/stage'"));

        let (script, warnings) = stage("tcMod scale 0.5\n map textures/foo }").unwrap();
        assert_eq!(
            script.stages[0].bundle.tex_mods,
            vec![TexMod::Scale { scale: [0.5, 0.0] }]
        );
        assert!(warnings
            .iter()
            .any(|w| w == "missing scale parms in shader 'tests/stage'"));
    }

    #[test]
    fn animmap_accumulates_frames_and_caps_at_eight() {
        // animMap reads to the end of its line, so the closing brace
        // has to be on the next line — exactly as in the C.
        let s = only_stage("animMap 5 f0 f1 f2 f3 f4 f5 f6 f7 f8 f9\n }");
        assert_eq!(
            s.bundle.map,
            Some(TextureMap::Anim {
                speed: 5.0,
                frames: (0..8).map(|i| format!("f{i}")).collect()
            })
        );

        // Repeated animMap lines append frames; the last speed wins.
        let s = only_stage("animMap 5 a b c\n animMap 2 d e\n }");
        assert_eq!(
            s.bundle.map,
            Some(TextureMap::Anim {
                speed: 2.0,
                frames: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]
            })
        );

        // A brace on the same line is consumed as a frame name, leaving
        // the stage unterminated — the C fails this input the same way.
        assert_eq!(stage("animMap 5 f0 }"), Err(ShaderError::UnterminatedStage));
    }

    #[test]
    fn videomap_records_its_name() {
        assert_eq!(
            only_stage("videoMap video/foo.roq }").bundle.map,
            Some(TextureMap::Video("video/foo.roq".into()))
        );
    }

    #[test]
    fn missing_keyword_parameters_fail_the_stage() {
        for keyword in [
            "map",
            "clampmap",
            "animMap",
            "videoMap",
            "alphaFunc",
            "depthfunc",
        ] {
            let expected = ShaderError::MissingParameter {
                keyword: match keyword {
                    "map" => "map",
                    "clampmap" => "clampmap",
                    "animMap" => "animMap",
                    "videoMap" => "videoMap",
                    "alphaFunc" => "alphaFunc",
                    _ => "depthfunc",
                },
            };
            assert_eq!(stage(keyword), Err(expected), "{keyword}");
        }
    }

    #[test]
    fn unknown_stage_parameters_fail_the_stage() {
        assert_eq!(
            stage("map foo frobnicate }"),
            Err(ShaderError::UnknownStageParameter("frobnicate".into()))
        );
    }

    #[test]
    fn an_unterminated_stage_fails_and_says_so() {
        let mut script = ShaderScript {
            name: "tests/stage".into(),
            ..Default::default()
        };
        let mut parser = Parser::new("map textures/foo");
        assert_eq!(
            parser.parse_stage(&mut script),
            Err(ShaderError::UnterminatedStage)
        );
        assert_eq!(
            parser.warnings.last().map(String::as_str),
            Some("no matching '}' found")
        );
        assert!(script.stages.is_empty());
    }

    #[test]
    fn a_ninth_stage_is_refused() {
        let text = "map a } ".repeat(MAX_SHADER_STAGES);
        let mut script = ShaderScript {
            name: "tests/stage".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(&text);
        for _ in 0..MAX_SHADER_STAGES {
            parser.parse_stage(&mut script).unwrap();
        }
        assert_eq!(
            parser.parse_stage(&mut script),
            Err(ShaderError::TooManyStages)
        );
    }

    /// Parses one full shader body (braces included) under a fixed
    /// name, the way `parse_shader` expects it.
    fn shader(body: &str) -> Result<(ShaderScript, Vec<String>), ShaderError> {
        let mut script = ShaderScript {
            name: "tests/shader".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(body);
        let result = parser.parse_shader(&mut script);
        let warnings = parser.warnings;
        result.map(|()| (script, warnings))
    }

    #[test]
    fn a_full_shader_parses() {
        let (script, warnings) = shader("{\n  {\n    map textures/foo/bar\n  }\n}").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(script.stages.len(), 1);
        assert!(script.explicitly_defined);
        assert_eq!(script.sort, ShaderSort::BAD);
        assert_eq!(script.cull, CullType::FrontSided);
        assert_eq!(script.surface_flags, 0);
        assert!(!script.no_mip_maps && !script.no_pic_mip);
        assert!(!script.polygon_offset && !script.entity_mergable);
        assert_eq!(script.clamp_time, None);
        assert!(script.sun.is_none() && script.fog.is_none() && script.sky.is_none());
    }

    #[test]
    fn the_driver_requires_an_opening_brace_and_a_closing_one() {
        let mut script = ShaderScript {
            name: "tests/shader".into(),
            ..Default::default()
        };
        let mut parser = Parser::new("map foo }");
        assert_eq!(
            parser.parse_shader(&mut script),
            Err(ShaderError::ExpectedOpenBrace("map".into()))
        );
        assert!(parser
            .warnings
            .iter()
            .any(|w| w == "expecting '{', found 'map' instead in shader 'tests/shader'"));
        let (err, warnings) = shader_err("{ qer_info \"x\"");
        assert_eq!(err, ShaderError::UnterminatedShader);
        assert!(parser_warnings_contain(
            &warnings,
            "no concluding '}' in shader tests/shader"
        ));
    }

    /// Small helper: is `needle` in `warnings`?
    fn parser_warnings_contain(warnings: &[String], needle: &str) -> bool {
        warnings.iter().any(|w| w == needle)
    }

    /// Parses a shader body expected to fail, returning the error and
    /// the warnings collected on the way.
    fn shader_err(body: &str) -> (ShaderError, Vec<String>) {
        let mut script = ShaderScript {
            name: "tests/shader".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(body);
        let result = parser.parse_shader(&mut script);
        let warnings = parser.warnings;
        (result.unwrap_err(), warnings)
    }

    #[test]
    fn empty_shaders_are_refused_unless_sky_or_fog() {
        assert_eq!(shader("{ }"), Err(ShaderError::NoStages));

        // A sky needs no stages.
        let (script, warnings) = shader("{ skyparms env/cloud - - }").unwrap();
        assert!(warnings.is_empty());
        assert!(script.sky.is_some());
        assert!(script.stages.is_empty());
        assert!(script.explicitly_defined);

        // Neither does fog.
        let (script, _) = shader("{ surfaceparm fog }").unwrap();
        assert_eq!(script.content_flags & CONTENTS_FOG, CONTENTS_FOG);

        // But an unknown directive fails before that check.
        assert_eq!(
            shader("{ frobnicate { map foo } }"),
            Err(ShaderError::UnknownShaderParameter("frobnicate".into()))
        );
    }

    #[test]
    fn qer_q3map_and_the_light_tesssize_skips_work() {
        let (script, warnings) =
            shader("{\n  qer_info \"hello world\"\n  q3map_lightimage textures/x\n  {\n    map foo\n  }\n}")
                .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(script.stages.len(), 1);

        // `light` eats exactly one token, so the map survives on the
        // same line...
        let (script, _) = shader("{ light 300 { map foo } }").unwrap();
        assert_eq!(script.stages.len(), 1);
        // ...while tesssize skips the rest of its line.
        let (script, _) = shader("{ tesssize 64\n { map foo } }").unwrap();
        assert_eq!(script.stages.len(), 1);
    }

    #[test]
    fn shader_level_flags_are_recorded() {
        let (script, _) = shader("{ nomipmaps { map foo } }").unwrap();
        assert!(script.no_mip_maps && script.no_pic_mip);

        let (script, _) = shader("{ nopicmip { map foo } }").unwrap();
        assert!(!script.no_mip_maps && script.no_pic_mip);

        let (script, _) = shader("{ polygonOffset entityMergable { map foo } }").unwrap();
        assert!(script.polygon_offset && script.entity_mergable);

        let (script, _) = shader("{ clampTime 4.5 { map foo } }").unwrap();
        assert_eq!(script.clamp_time, Some(4.5));
        // A clampTime with no value on its line reads nothing.
        let (script, _) = shader("{ clampTime\n { map foo } }").unwrap();
        assert_eq!(script.clamp_time, None);
    }

    #[test]
    fn surfaceparm_accumulates_flags() {
        let (script, _) = shader("{ surfaceparm sky surfaceparm nodraw { map foo } }").unwrap();
        assert_eq!(script.surface_flags, SURF_SKY | SURF_NODRAW);

        let (script, _) = shader("{ surfaceparm trans surfaceparm nonsolid { map foo } }").unwrap();
        assert_eq!(script.surface_flags, SURF_NONSOLID);
        assert_eq!(script.content_flags, CONTENTS_TRANSLUCENT);

        // Unknown names are silently ignored, as in the C.
        let (script, warnings) = shader("{ surfaceparm flurble { map foo } }").unwrap();
        assert_eq!(script.surface_flags, 0);
        assert!(warnings.is_empty());
    }

    #[test]
    fn cull_and_sort_keywords_map_like_the_c() {
        let (script, _) = shader("{ cull none { map foo } }").unwrap();
        assert_eq!(script.cull, CullType::TwoSided);
        let (script, _) = shader("{ cull twosided { map foo } }").unwrap();
        assert_eq!(script.cull, CullType::TwoSided);
        let (script, _) = shader("{ cull backside { map foo } }").unwrap();
        assert_eq!(script.cull, CullType::BackSided);

        // Unknown cull parms warn and keep the default; a missing one
        // warns too.
        let (script, warnings) = shader("{ cull sideways { map foo } }").unwrap();
        assert_eq!(script.cull, CullType::FrontSided);
        assert!(parser_warnings_contain(
            &warnings,
            "invalid cull parm 'sideways' in shader 'tests/shader'"
        ));
        let (_, warnings) = shader("{ cull\n { map foo } }").unwrap();
        assert!(parser_warnings_contain(
            &warnings,
            "missing cull parms in shader 'tests/shader'"
        ));

        let (script, _) = shader("{ sort opaque { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort::OPAQUE);
        let (script, _) = shader("{ sort sky { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort::ENVIRONMENT);
        let (script, _) = shader("{ sort additive { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort::BLEND1);
        let (script, _) = shader("{ sort underwater { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort::UNDERWATER);
        // Numeric sorts store the raw number, as `= atof` does.
        let (script, _) = shader("{ sort 10 { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort(10));
        // `portal` is its own keyword.
        let (script, _) = shader("{ portal { map foo } }").unwrap();
        assert_eq!(script.sort, ShaderSort::PORTAL);
        let (_, warnings) = shader("{ sort\n { map foo } }").unwrap();
        assert!(parser_warnings_contain(
            &warnings,
            "missing sort parameter in shader 'tests/shader'"
        ));
    }

    #[test]
    fn fogparms_fills_the_fog_block() {
        let (script, warnings) = shader("{ fogParms ( 0.5 0.5 0.5 ) 128\n { map foo } }").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            script.fog,
            Some(FogParms {
                color: [0.5, 0.5, 0.5],
                depth_for_opaque: 128.0
            })
        );

        // A bad colour vector fails the shader; the paren warning is
        // already collected by ParseVector.
        let mut script = ShaderScript {
            name: "tests/shader".into(),
            ..Default::default()
        };
        let mut parser = Parser::new("{ fogParms 0.5 0.5 0.5 128 }");
        assert_eq!(
            parser.parse_shader(&mut script),
            Err(ShaderError::MissingVector)
        );
        assert!(parser
            .warnings
            .iter()
            .any(|w| w == "missing parenthesis in shader 'tests/shader'"));

        // A missing depth keeps the colour with a zero depth and does
        // not skip the rest of the line, just as the C continues.
        let (script, warnings) = shader("{ fogParms ( 1 1 1 )\n { map foo } }").unwrap();
        assert_eq!(
            script.fog,
            Some(FogParms {
                color: [1.0, 1.0, 1.0],
                depth_for_opaque: 0.0
            })
        );
        assert!(parser_warnings_contain(
            &warnings,
            "missing parm for 'fogParms' keyword in shader 'tests/shader'"
        ));
    }

    #[test]
    fn skyparms_builds_the_six_faces_and_defaults_the_cloud_height() {
        let (script, _) = shader("{ skyparms env/cloud 512 - { map foo } }").unwrap();
        let sky = script.sky.unwrap();
        let outer = sky.outerbox.unwrap();
        assert_eq!(outer[0], "env/cloud_rt.tga");
        assert_eq!(outer[3], "env/cloud_ft.tga");
        assert_eq!(outer[5], "env/cloud_dn.tga");
        assert!(sky.innerbox.is_none());
        assert_eq!(sky.cloud_height, 512.0);

        // A zero cloud height falls back to 512.
        let (script, _) = shader("{ skyparms env/x 0 - }").unwrap();
        assert_eq!(script.sky.unwrap().cloud_height, 512.0);

        // "-" for the outer box, a real inner box.
        let (script, _) = shader("{ skyparms - 256 env/inner }").unwrap();
        let sky = script.sky.unwrap();
        assert!(sky.outerbox.is_none());
        assert_eq!(sky.innerbox.unwrap()[0], "env/inner_rt.tga");

        // A missing inner box warns and leaves the shader unmarked —
        // the C returns before setting is_sky.
        let (script, warnings) = shader("{ skyparms env/x 512\n { map foo } }").unwrap();
        assert!(script.sky.is_none());
        assert!(parser_warnings_contain(
            &warnings,
            "'skyParms' missing parameter in shader 'tests/shader'"
        ));
    }

    #[test]
    fn deform_directives_parse() {
        let (script, warnings) =
            shader("{ deformVertexes wave 2 sin 0 1 0 1 { map foo } }").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(script.deforms.len(), 1);
        let d = &script.deforms[0];
        assert_eq!(d.deform, Deform::Wave);
        assert_eq!(d.spread, 0.5);
        assert_eq!(
            d.wave,
            WaveForm {
                func: GenFunc::Sin,
                base: 0.0,
                amplitude: 1.0,
                phase: 0.0,
                frequency: 1.0
            }
        );

        // A zero divisor warns and uses the 100 fallback.
        let (script, warnings) =
            shader("{ deformVertexes wave 0 sin 0 1 0 1 { map foo } }").unwrap();
        assert_eq!(script.deforms[0].spread, 100.0);
        assert!(parser_warnings_contain(
            &warnings,
            "illegal div value of 0 in deformVertexes command for shader 'tests/shader'"
        ));

        let (script, _) = shader("{ deformVertexes bulge 2 3 4 { map foo } }").unwrap();
        let d = &script.deforms[0];
        assert_eq!(d.deform, Deform::Bulge);
        assert_eq!(
            (d.bulge_width, d.bulge_height, d.bulge_speed),
            (2.0, 3.0, 4.0)
        );

        // `normal` reads amplitude first, then frequency — as the C
        // does, whatever the docs suggest.
        let (script, _) = shader("{ deformVertexes normal 0.5 0.1 { map foo } }").unwrap();
        let d = &script.deforms[0];
        assert_eq!(d.deform, Deform::Normals);
        assert_eq!(d.wave.amplitude, 0.5);
        assert_eq!(d.wave.frequency, 0.1);

        let (script, _) =
            shader("{ deformVertexes move 1 2 3 square 0 0 0 1 { map foo } }").unwrap();
        let d = &script.deforms[0];
        assert_eq!(d.deform, Deform::Move);
        assert_eq!(d.move_vector, [1.0, 2.0, 3.0]);
        assert_eq!(d.wave.func, GenFunc::Square);

        let (script, _) = shader("{ deformVertexes projectionShadow { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::ProjectionShadow);
        let (script, _) = shader("{ deformVertexes autoSprite { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Autosprite);
        let (script, _) = shader("{ deformVertexes autoSprite2 { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Autosprite2);

        // text[0-7]; a bare "text" or an out-of-range digit lands on 0,
        // because token[4] reads past the token.
        let (script, _) = shader("{ deformVertexes text3 { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Text3);
        let (script, _) = shader("{ deformVertexes text { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Text0);
        let (script, _) = shader("{ deformVertexes text9 { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Text0);

        // An unknown subtype keeps the reserved (zeroed) slot and warns.
        let (script, warnings) = shader("{ deformVertexes wobble { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::None);
        assert!(parser_warnings_contain(
            &warnings,
            "unknown deformVertexes subtype 'wobble' found in shader 'tests/shader'"
        ));

        // Only three deforms fit; the fourth warns without storing.
        let body = format!(
            "{{ {} {{ map foo }} }}",
            "deformVertexes autosprite ".repeat(MAX_SHADER_DEFORMS + 1)
        );
        let (script, warnings) = shader(&body).unwrap();
        assert_eq!(script.deforms.len(), MAX_SHADER_DEFORMS);
        assert!(parser_warnings_contain(
            &warnings,
            &format!("MAX_SHADER_DEFORMS in 'tests/shader'")
        ));

        // A missing subtype warns without consuming a slot.
        let (script, warnings) = shader("{ deformVertexes\n { map foo } }").unwrap();
        assert!(script.deforms.is_empty());
        assert!(parser_warnings_contain(
            &warnings,
            "missing deform parm in shader 'tests/shader'"
        ));

        // Missing wave parameters stop early but keep the slot — and
        // the wave deform is still set, because ParseWaveForm returns
        // void in C.
        let (script, warnings) = shader("{ deformVertexes wave 0\n { map foo } }").unwrap();
        assert_eq!(script.deforms[0].deform, Deform::Wave);
        assert_eq!(script.deforms[0].spread, 100.0);
        assert!(parser_warnings_contain(
            &warnings,
            "missing waveform parm in shader 'tests/shader'"
        ));
    }

    #[test]
    fn q3map_sun_computes_the_light_and_direction() {
        // Light (0,1,0) is already unit, times intensity 2; yaw 90° and
        // pitch 0° point the direction along +Y.
        let (script, _) = shader("{ q3map_sun 0 1 0 2 90 0 { map foo } }").unwrap();
        let sun = script.sun.unwrap();
        assert_eq!(sun.light, [0.0, 2.0, 0.0]);
        for (i, expected) in [0.0, 1.0, 0.0].iter().enumerate() {
            assert!(
                (sun.direction[i] - expected).abs() < 1e-6,
                "direction[{i}] = {}",
                sun.direction[i]
            );
        }

        // Non-unit input is normalized before scaling: (0,0,4) -> (0,0,1)
        // * 3, and pitch 90° points straight up.
        let (script, _) = shader("{ q3map_sun 0 0 4 3 0 90 { map foo } }").unwrap();
        let sun = script.sun.unwrap();
        assert_eq!(sun.light, [0.0, 0.0, 3.0]);
        assert!(sun.direction[0].abs() < 1e-6 && sun.direction[1].abs() < 1e-6);
        assert!((sun.direction[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn compress_collapses_whitespace_and_strips_comments() {
        let input = "// header\n\n\n   textures/x   {\n\tmap textures/x // inline\n}\n\n\n";
        assert_eq!(compress_text(input), "\ntextures/x {\nmap textures/x\n}");

        assert_eq!(compress_text("a /* c */ b"), "a b");
        // Trailing whitespace is dropped with nothing left to emit it.
        assert_eq!(compress_text("x  \n\n"), "x");
        // Newlines and spaces collapse independently: one newline, one
        // space, and a newline swallows the spaces after it.
        assert_eq!(compress_text("a\n\n   \nb   c"), "a\nb c");
    }

    #[test]
    fn compress_keeps_quoted_strings_verbatim() {
        assert_eq!(compress_text("a \"b   c\" d"), "a \"b   c\" d");
        // Comment markers inside quotes are not comments.
        assert_eq!(
            compress_text("\"// not a comment\""),
            "\"// not a comment\""
        );
    }

    #[test]
    fn hash_value_matches_the_c_formula() {
        // Case, extension and backslash folding all happen before the
        // sum, as in generateHashValue.
        assert_eq!(
            generate_hash_value("Textures/Foo", MAX_SHADERTEXT_HASH),
            generate_hash_value("textures/foo", MAX_SHADERTEXT_HASH)
        );
        assert_eq!(
            generate_hash_value("foo.shader", MAX_SHADERTEXT_HASH),
            generate_hash_value("foo", MAX_SHADERTEXT_HASH)
        );
        assert_eq!(
            generate_hash_value("foo\\bar", MAX_SHADERTEXT_HASH),
            generate_hash_value("foo/bar", MAX_SHADERTEXT_HASH)
        );
        assert_eq!(generate_hash_value("", MAX_SHADERTEXT_HASH), 0);
        for name in ["a", "textures/foo", "very/long/shader/name"] {
            assert!(
                generate_hash_value(name, MAX_SHADERTEXT_HASH) < MAX_SHADERTEXT_HASH,
                "{name}"
            );
        }
    }

    #[test]
    fn the_index_finds_and_parses_shaders() {
        let one = "textures/one\n{\n{\n    map textures/one\n}\n}\n";
        let two = "// a comment\ntextures/two\n{\n{\nmap textures/two\n}\n}\n";
        let index = ShaderTextIndex::build(&[one, two]);

        // The found text starts where ParseShader expects the '{'.
        let found = index.find("textures/one").expect("textures/one");
        let mut script = ShaderScript {
            name: "textures/one".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(found);
        parser.parse_shader(&mut script).unwrap();
        assert_eq!(script.stages.len(), 1);
        assert_eq!(
            script.stages[0].bundle.map,
            Some(TextureMap::File {
                name: "textures/one".into(),
                clamp: false
            })
        );

        // Nested braces in the stage body walk correctly.
        let found = index.find("textures/two").expect("textures/two");
        let mut script = ShaderScript {
            name: "textures/two".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(found);
        parser.parse_shader(&mut script).unwrap();
        assert_eq!(script.stages.len(), 1);

        // Names compare case-insensitively (Q_stricmp).
        assert!(index.find("TEXTURES/ONE").is_some());

        // The extension drops out of the hash but not out of the
        // comparison, so a filename never matches a shader name...
        assert_eq!(index.find("textures/one.tga"), None);
        // ...and neither does a name that was never defined.
        assert_eq!(index.find("textures/missing"), None);
    }

    #[test]
    fn duplicate_labels_resolve_in_combined_text_order() {
        // ScanAndLoadShaderFiles appends the files in reverse, so the
        // last-listed file's definition comes first in the combined
        // text — and wins, as it does in the C's linear fallback.
        let first = "textures/dup\n{\n{\nmap textures/from_a\n}\n}\n";
        let second = "textures/dup\n{\n{\nmap textures/from_b\n}\n}\n";
        let index = ShaderTextIndex::build(&[first, second]);

        let found = index.find("textures/dup").expect("textures/dup");
        let mut script = ShaderScript {
            name: "textures/dup".into(),
            ..Default::default()
        };
        let mut parser = Parser::new(found);
        parser.parse_shader(&mut script).unwrap();
        assert_eq!(
            script.stages[0].bundle.map,
            Some(TextureMap::File {
                name: "textures/from_b".into(),
                clamp: false
            })
        );
    }

    #[test]
    fn the_index_tolerates_stray_tokens_and_caps_the_file_list() {
        // A label with no braces: SkipBracedSection consumes exactly
        // one token (its do-while runs once), and the scan carries on
        // to the next label — as in the C.
        let index = ShaderTextIndex::build(&["orphan textures/x\nreal\n{\nmap textures/y\n}\n"]);
        assert!(index.find("orphan").is_some());
        assert!(index.find("real").is_some());

        // Files beyond MAX_SHADER_FILES are dropped, as the C's
        // buffers[MAX_SHADER_FILES] cap drops them.
        let filler = "textures/filler\n{\nmap textures/filler\n}\n";
        let extra = "textures/overflow\n{\nmap textures/overflow\n}\n";
        let mut files = vec![filler; MAX_SHADER_FILES];
        files.push(extra);
        let index = ShaderTextIndex::build(&files);
        assert!(index.find("textures/filler").is_some());
        assert_eq!(index.find("textures/overflow"), None);
    }
}
