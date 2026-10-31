//! Renderer interface types, ported from `code/cgame/tr_types.h` and the
//! handful of shared types in `code/game/q_shared.h` that
//! `code/renderer/tr_public.h` refers to.
//!
//! These are the value types that cross the renderer boundary, so they keep
//! the field order and the numeric values of the C types. They are ordinary
//! Rust values, not C memory: a shim between this interface and the C
//! renderer copies field by field.

/// `MAX_STRING_CHARS` in `q_shared.h`.
pub const MAX_STRING_CHARS: usize = 1024;
/// `BIG_INFO_STRING` in `q_shared.h`, shared with [`crate::cmd`].
pub use crate::cmd::BIG_INFO_STRING;

/// `MAX_QPATH` in `q_shared.h`.
pub const MAX_QPATH: usize = 64;
/// `MAX_MAP_AREA_BYTES` in `q_shared.h`.
pub const MAX_MAP_AREA_BYTES: usize = 32;

/// `REF_API_VERSION` in `tr_public.h`; a renderer built against another
/// version is refused by `GetRefAPI`.
pub const REF_API_VERSION: i32 = 8;

pub const MAX_RENDER_STRINGS: usize = 8;
pub const MAX_RENDER_STRING_LENGTH: usize = 32;

// renderfx flags
pub const RF_MINLIGHT: i32 = 1;
pub const RF_THIRD_PERSON: i32 = 2;
pub const RF_FIRST_PERSON: i32 = 4;
pub const RF_DEPTHHACK: i32 = 8;
pub const RF_NOSHADOW: i32 = 64;
pub const RF_LIGHTING_ORIGIN: i32 = 128;
pub const RF_SHADOW_PLANE: i32 = 256;
pub const RF_WRAP_FRAMES: i32 = 512;

// refdef flags
pub const RDF_NOWORLDMODEL: i32 = 1;
pub const RDF_HYPERSPACE: i32 = 4;

/// `vec3_t`.
pub type Vec3 = [f32; 3];

/// `qhandle_t`, an opaque handle into the refresh module.
pub type QHandle = i32;

/// `polyVert_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PolyVert {
    pub xyz: Vec3,
    pub st: [f32; 2],
    pub modulate: [u8; 4],
}

/// `poly_t`. The C type points at its vertices; a Rust list owns them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Poly {
    pub shader: QHandle,
    pub num_verts: i32,
    pub verts: Vec<PolyVert>,
}

/// `refEntityType_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefEntityType {
    Model = 0,
    Poly = 1,
    Sprite = 2,
    Beam = 3,
    RailCore = 4,
    RailRings = 5,
    Lightning = 6,
    /// Doesn't draw anything, just info for portals.
    PortalSurface = 7,
    MaxRefEntityType = 8,
}

/// `refEntity_t`: everything the scene needs to draw one entity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefEntity {
    pub re_type: RefEntityType,
    pub renderfx: i32,

    /// Opaque type outside refresh.
    pub h_model: QHandle,

    /// So multi-part models can be lit identically (`RF_LIGHTING_ORIGIN`).
    pub lighting_origin: Vec3,
    /// Projection shadows go here, stencils go slightly lower.
    pub shadow_plane: f32,

    /// Rotation vectors.
    pub axis: [Vec3; 3],
    /// Axis are not normalized, i.e. they have scale.
    pub non_normalized_axes: bool,
    /// Also used as `MODEL_BEAM`'s "from".
    pub origin: Vec3,
    /// Also used as `MODEL_BEAM`'s diameter.
    pub frame: i32,

    /// Also used as `MODEL_BEAM`'s "to".
    pub oldorigin: Vec3,
    pub oldframe: i32,
    /// 0.0 = current, 1.0 = old.
    pub backlerp: f32,

    /// Inline skin index.
    pub skin_num: i32,
    /// Default skin when `0`.
    pub custom_skin: QHandle,
    /// Use one image for the entire entity.
    pub custom_shader: QHandle,

    /// Colors used by rgbgen entity shaders.
    pub shader_rgba: [u8; 4],
    /// Texture coordinates used by tcMod entity modifiers.
    pub shader_tex_coord: [f32; 2],
    /// Subtract from refdef time to control effect start times.
    pub shader_time: f32,

    pub radius: f32,
    pub rotation: f32,
}

impl RefEntity {
    pub fn new(re_type: RefEntityType) -> Self {
        RefEntity {
            re_type,
            ..Default::default()
        }
    }
}

impl Default for RefEntityType {
    fn default() -> Self {
        RefEntityType::Model
    }
}

/// `refdef_t`: the view the scene is rendered from.
#[derive(Debug, Clone, PartialEq)]
pub struct RefDef {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub fov_x: f32,
    pub fov_y: f32,
    pub vieworg: Vec3,
    /// Transformation matrix.
    pub viewaxis: [Vec3; 3],
    /// Time in milliseconds for shader effects and other time dependent
    /// rendering issues.
    pub time: i32,
    /// `RDF_NOWORLDMODEL` and friends.
    pub rdflags: i32,
    /// 1 bits prevent the associated area from rendering at all.
    pub areamask: [u8; MAX_MAP_AREA_BYTES],
    /// Text messages for deform text shaders, as NUL-terminated bytes.
    pub text: [[u8; MAX_RENDER_STRING_LENGTH]; MAX_RENDER_STRINGS],
}

impl Default for RefDef {
    fn default() -> Self {
        RefDef {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            fov_x: 0.0,
            fov_y: 0.0,
            vieworg: [0.0; 3],
            viewaxis: [[0.0; 3]; 3],
            time: 0,
            rdflags: 0,
            areamask: [0; MAX_MAP_AREA_BYTES],
            text: [[0; MAX_RENDER_STRING_LENGTH]; MAX_RENDER_STRINGS],
        }
    }
}

/// `stereoFrame_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StereoFrame {
    #[default]
    Center = 0,
    Left = 1,
    Right = 2,
}

/// `textureCompression_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextureCompression {
    #[default]
    None = 0,
    S3tc = 1,
}

/// `glDriverType_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GlDriverType {
    /// Integrated with the window system. Tests check for values greater
    /// than this to detect mini-drivers, so it must stay the lowest value.
    #[default]
    Icd = 0,
    Standalone = 1,
    Voodoo = 2,
}

/// `glHardwareType_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GlHardwareType {
    /// Where everything works the way it should.
    #[default]
    Generic = 0,
    /// Voodoo Banshee or Voodoo3: no secondary display adapter exists.
    Voodoo2D3D = 1,
    /// Where you can't interpolate alpha.
    Riva128 = 2,
    /// Where you can't modulate alpha on alpha textures.
    RagePro = 3,
    /// Where you don't have src*dst.
    Permedia2 = 4,
}

/// `glconfig_t`: the video configuration reported by `BeginRegistration`.
#[derive(Clone, PartialEq)]
pub struct GlConfig {
    pub renderer_string: [u8; MAX_STRING_CHARS],
    pub vendor_string: [u8; MAX_STRING_CHARS],
    pub version_string: [u8; MAX_STRING_CHARS],
    pub extensions_string: [u8; BIG_INFO_STRING],

    /// Queried from the driver.
    pub max_texture_size: i32,
    /// Multitexture ability.
    pub max_active_textures: i32,

    pub color_bits: i32,
    pub depth_bits: i32,
    pub stencil_bits: i32,

    pub driver_type: GlDriverType,
    pub hardware_type: GlHardwareType,

    pub device_supports_gamma: bool,
    pub texture_compression: TextureCompression,
    pub texture_env_add_available: bool,

    pub vid_width: i32,
    pub vid_height: i32,
    /// The screen's physical width / height, which may differ from
    /// vidWidth / vidHeight if the pixels are non-square.
    pub window_aspect: f32,

    pub display_frequency: i32,

    /// Does rendering consume the entire screen?
    pub is_fullscreen: bool,
    pub stereo_enabled: bool,
    /// Dual processor.
    pub smp_active: bool,
}

impl Default for GlConfig {
    fn default() -> Self {
        GlConfig {
            renderer_string: [0; MAX_STRING_CHARS],
            vendor_string: [0; MAX_STRING_CHARS],
            version_string: [0; MAX_STRING_CHARS],
            extensions_string: [0; BIG_INFO_STRING],
            max_texture_size: 0,
            max_active_textures: 0,
            color_bits: 0,
            depth_bits: 0,
            stencil_bits: 0,
            driver_type: GlDriverType::default(),
            hardware_type: GlHardwareType::default(),
            device_supports_gamma: false,
            texture_compression: TextureCompression::default(),
            texture_env_add_available: false,
            vid_width: 0,
            vid_height: 0,
            window_aspect: 0.0,
            display_frequency: 0,
            is_fullscreen: false,
            stereo_enabled: false,
            smp_active: false,
        }
    }
}

impl std::fmt::Debug for GlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlConfig")
            .field("renderer_string", &first_cstr(&self.renderer_string))
            .field("vendor_string", &first_cstr(&self.vendor_string))
            .field("version_string", &first_cstr(&self.version_string))
            .field("max_texture_size", &self.max_texture_size)
            .field("vid_width", &self.vid_width)
            .field("vid_height", &self.vid_height)
            .field("is_fullscreen", &self.is_fullscreen)
            .finish()
    }
}

/// Reads a NUL-terminated byte string for display.
fn first_cstr(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("<invalid utf8>")
}

/// `markFragment_t`, returned by `MarkFragments`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkFragment {
    pub first_point: i32,
    pub num_points: i32,
}

/// `orientation_t`, filled in by `LerpTag`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Orientation {
    pub origin: Vec3,
    pub axis: [Vec3; 3],
}

/// `GLYPHS_PER_FONT`, from `GLYPH_END - GLYPH_START + 1`.
pub const GLYPHS_PER_FONT: usize = 256;

/// `glyphInfo_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GlyphInfo {
    /// Number of scan lines.
    pub height: i32,
    /// Top of glyph in buffer.
    pub top: i32,
    /// Bottom of glyph in buffer.
    pub bottom: i32,
    /// Width for copying.
    pub pitch: i32,
    /// X adjustment.
    pub x_skip: i32,
    pub image_width: i32,
    pub image_height: i32,
    /// X offset in image where glyph starts.
    pub s: f32,
    /// Y offset in image where glyph starts.
    pub t: f32,
    pub s2: f32,
    pub t2: f32,
    /// Handle to the shader with the glyph.
    pub glyph: QHandle,
    /// `char shaderName[32]`, NUL-terminated.
    pub shader_name: [u8; 32],
}

/// `fontInfo_t`, filled in by `RegisterFont`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontInfo {
    pub glyphs: [GlyphInfo; GLYPHS_PER_FONT],
    pub glyph_scale: f32,
    /// `char name[MAX_QPATH]`, NUL-terminated.
    pub name: [u8; MAX_QPATH],
}

impl Default for FontInfo {
    fn default() -> Self {
        FontInfo {
            glyphs: std::array::from_fn(|_| GlyphInfo::default()),
            glyph_scale: 0.0,
            name: [0; MAX_QPATH],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_entity_types_keep_the_c_values() {
        assert_eq!(RefEntityType::Model as i32, 0);
        assert_eq!(RefEntityType::Poly as i32, 1);
        assert_eq!(RefEntityType::Sprite as i32, 2);
        assert_eq!(RefEntityType::Beam as i32, 3);
        assert_eq!(RefEntityType::RailCore as i32, 4);
        assert_eq!(RefEntityType::RailRings as i32, 5);
        assert_eq!(RefEntityType::Lightning as i32, 6);
        assert_eq!(RefEntityType::PortalSurface as i32, 7);
        assert_eq!(RefEntityType::MaxRefEntityType as i32, 8);
    }

    #[test]
    fn flags_keep_the_c_values() {
        assert_eq!(RF_MINLIGHT, 1);
        assert_eq!(RF_THIRD_PERSON, 2);
        assert_eq!(RF_FIRST_PERSON, 4);
        assert_eq!(RF_DEPTHHACK, 8);
        assert_eq!(RF_NOSHADOW, 64);
        assert_eq!(RF_LIGHTING_ORIGIN, 128);
        assert_eq!(RF_SHADOW_PLANE, 256);
        assert_eq!(RF_WRAP_FRAMES, 512);
        assert_eq!(RDF_NOWORLDMODEL, 1);
        assert_eq!(RDF_HYPERSPACE, 4);
    }

    #[test]
    fn shared_sizes_match_q_shared() {
        assert_eq!(REF_API_VERSION, 8);
        assert_eq!(MAX_STRING_CHARS, 1024);
        assert_eq!(BIG_INFO_STRING, 8192);
        assert_eq!(MAX_QPATH, 64);
        assert_eq!(MAX_MAP_AREA_BYTES, 32);
        assert_eq!(GLYPHS_PER_FONT, 256);
        assert_eq!(MAX_RENDER_STRINGS, 8);
        assert_eq!(MAX_RENDER_STRING_LENGTH, 32);
    }

    #[test]
    fn driver_and_hardware_enums_keep_the_c_order() {
        assert_eq!(GlDriverType::Icd as i32, 0);
        assert_eq!(GlDriverType::Standalone as i32, 1);
        assert_eq!(GlDriverType::Voodoo as i32, 2);
        assert_eq!(GlHardwareType::Generic as i32, 0);
        assert_eq!(GlHardwareType::Voodoo2D3D as i32, 1);
        assert_eq!(GlHardwareType::Riva128 as i32, 2);
        assert_eq!(GlHardwareType::RagePro as i32, 3);
        assert_eq!(GlHardwareType::Permedia2 as i32, 4);
        assert_eq!(TextureCompression::None as i32, 0);
        assert_eq!(TextureCompression::S3tc as i32, 1);
        assert_eq!(StereoFrame::Center as i32, 0);
        assert_eq!(StereoFrame::Left as i32, 1);
        assert_eq!(StereoFrame::Right as i32, 2);
    }

    #[test]
    fn refdef_defaults_are_zeroed() {
        let fd = RefDef::default();
        assert_eq!(fd.areamask, [0; MAX_MAP_AREA_BYTES]);
        assert_eq!(fd.text, [[0; MAX_RENDER_STRING_LENGTH]; MAX_RENDER_STRINGS]);
        assert_eq!(fd.viewaxis, [[0.0; 3]; 3]);
        assert_eq!(fd.rdflags, 0);
    }

    #[test]
    fn ref_entity_defaults_to_a_model() {
        let re = RefEntity::new(RefEntityType::Sprite);
        assert_eq!(re.re_type, RefEntityType::Sprite);
        assert_eq!(re.renderfx, 0);
        assert_eq!(re.backlerp, 0.0);
        assert!(!re.non_normalized_axes);

        let default = RefEntity::default();
        assert_eq!(default.re_type, RefEntityType::Model);
    }

    #[test]
    fn glconfig_debug_shows_the_strings_without_dumping_them() {
        let mut config = GlConfig::default();
        config.renderer_string[..10].copy_from_slice(b"TestOpenGL");
        config.renderer_string[10] = 0;
        config.vid_width = 1920;

        let debug = format!("{config:?}");
        assert!(debug.contains("TestOpenGL"));
        assert!(debug.contains("1920"));
        // The 8 KiB extensions buffer must not appear in the output.
        assert!(debug.len() < 400);
    }

    #[test]
    fn font_info_has_one_glyph_per_code_point() {
        let font = FontInfo::default();
        assert_eq!(font.glyphs.len(), 256);
        assert_eq!(font.glyphs[65].height, 0);
        assert_eq!(font.name, [0; MAX_QPATH]);
    }
}
