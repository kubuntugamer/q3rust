//! Shader script parsing, ported from `code/renderer/tr_shader.c`.
//!
//! This phase ports the tokenizer and the *stage* half of the shader
//! parser: [`Parser::next`] is `COM_ParseExt`, and [`Parser::parse_stage`]
//! is `ParseStage` together with everything under it — `ParseVector`,
//! `ParseWaveForm`, `ParseTexMod` and the name-to-value tables
//! (`NameToAFunc`, `NameToSrcBlendMode`, `NameToDstBlendMode`,
//! `NameToGenFunc`). The shader-level directives (`deformVertexes`,
//! `skyParms`, `sort`, `surfaceparm`, ...) and the `ParseShader` driver
//! arrive with the next phase, which is why [`ShaderScript`] currently
//! only carries the fields stage parsing touches.
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

/// The parse result of one shader definition. Phase 4c fills the name,
/// stages and `alphaGen portal` range; the remaining shader-level
/// directives arrive with the `ParseShader` driver next phase.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShaderScript {
    pub name: String,
    pub stages: Vec<ShaderStage>,
    /// `shader.portalRange`, written by `alphaGen portal` (from inside
    /// stage parsing, exactly as the C writes the global).
    pub portal_range: Option<f32>,
}

/// A shader script could not be parsed — the cases where `ParseStage`
/// returns `qfalse` or `ri.Error` would abort the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaderError {
    /// The text ended inside a stage (`no matching '}' found`).
    UnterminatedStage,
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
            if token == "}" {
                break;
            }

            let name = script.name.clone();
            let lower = token.to_ascii_lowercase();

            match lower.as_str() {
                "map" | "clampmap" => {
                    let clamp = lower == "clampmap";
                    let image = self.next(false).to_string();
                    if image.is_empty() {
                        return Err(ShaderError::MissingParameter {
                            keyword: if clamp { "clampmap" } else { "map" },
                        });
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
                        return Err(ShaderError::MissingParameter {
                            keyword: "videoMap",
                        });
                    }
                    stage.bundle.map = Some(TextureMap::Video(token));
                }
                "alphafunc" => {
                    let token = self.next(false);
                    if token.is_empty() {
                        return Err(ShaderError::MissingParameter {
                            keyword: "alphaFunc",
                        });
                    }
                    atest_bits = name_to_afunc(&token, &name, &mut self.warnings);
                }
                "depthfunc" => {
                    let token = self.next(false).to_string();
                    if token.is_empty() {
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
}
