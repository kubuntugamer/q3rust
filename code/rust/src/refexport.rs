//! The renderer interface, ported from `refexport_t` in
//! `code/renderer/tr_public.h`.
//!
//! `GetRefAPI( REF_API_VERSION, &ri )` returns exactly this interface: the
//! client talks to the refresh module through it, and the OpenGL renderer in
//! `code/renderer` satisfies it today. This trait is the same interface
//! expressed in Rust, so the Vulkan renderer of a later phase can implement
//! it and nothing else about the client has to change.
//!
//! Shape of the port:
//!
//! - method names are the C entry points in snake case, in the same order
//!   as `refexport_t`;
//! - raw pointers and `(buffer, size)` pairs become slices and `Option`,
//!   so a NULL out-parameter in C is `None` here;
//! - `qboolean` returns become `bool`.
//!
//! Not ported: `refimport_t`, the imports the refresh module pulls from the
//! engine, and `__USEA3D`'s extra entry point.

use crate::ref_types::{
    FontInfo, GlConfig, MarkFragment, Orientation, PolyVert, QHandle, RefDef, RefEntity,
    StereoFrame, Vec3,
};

/// The refresh module's exported interface (`refexport_t`).
///
/// Every method takes `&mut self` because the C module is stateful even for
/// queries such as `LerpTag` and `ModelBounds`.
pub trait RefExport {
    /// Called before the library is unloaded. When the system is only
    /// reconfiguring, pass `false` to keep the screen from flashing to the
    /// desktop.
    fn shutdown(&mut self, destroy_window: bool);

    /// Makes any existing media pointers invalid and reports the current
    /// configuration, including screen width and height, which the client
    /// uses to size display elements.
    fn begin_registration(&mut self, config: &mut GlConfig);

    /// All data used in a level should be registered before rendering any
    /// frames to prevent disk hits, but registration can happen later.
    fn register_model(&mut self, name: &str) -> QHandle;
    fn register_skin(&mut self, name: &str) -> QHandle;
    fn register_shader(&mut self, name: &str) -> QHandle;
    fn register_shader_no_mip(&mut self, name: &str) -> QHandle;
    fn load_world(&mut self, name: &str);

    /// The vis data is large enough that the clipmodel subsystem shares it.
    fn set_world_vis_data(&mut self, vis: &[u8]);

    /// Draws a tiny polygon with each texture, forcing the textures into
    /// card memory.
    fn end_registration(&mut self);

    /// A scene is built with this plus the various `add_*` calls, and
    /// nothing is drawn until [`RefExport::render_scene`].
    fn clear_scene(&mut self);
    fn add_ref_entity_to_scene(&mut self, re: &RefEntity);

    /// Adds `num_polys` polygons of `verts_per_poly` vertices each, laid
    /// out consecutively in `verts`. Returns the number of polygons added.
    fn add_poly_to_scene(
        &mut self,
        shader: QHandle,
        verts_per_poly: i32,
        verts: &[PolyVert],
        num_polys: i32,
    ) -> i32;

    fn light_for_point(
        &mut self,
        point: &Vec3,
        ambient_light: &mut Vec3,
        directed_light: &mut Vec3,
        light_dir: &mut Vec3,
    ) -> i32;

    fn add_light_to_scene(&mut self, org: &Vec3, intensity: f32, r: f32, g: f32, b: f32);
    fn add_additive_light_to_scene(&mut self, org: &Vec3, intensity: f32, r: f32, g: f32, b: f32);
    fn render_scene(&mut self, fd: &RefDef);

    /// `None` means the default color of 1,1,1,1.
    fn set_color(&mut self, rgba: Option<&[f32; 4]>);

    /// A white shader is used when `shader` is 0.
    fn draw_stretch_pic(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        s1: f32,
        t1: f32,
        s2: f32,
        t2: f32,
        shader: QHandle,
    );

    /// Draws images for cinematic rendering as 32-bit RGBA.
    fn draw_stretch_raw(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        cols: i32,
        rows: i32,
        data: &[u8],
        client: i32,
        dirty: bool,
    );

    fn upload_cinematic(
        &mut self,
        w: i32,
        h: i32,
        cols: i32,
        rows: i32,
        data: &[u8],
        client: i32,
        dirty: bool,
    );

    fn begin_frame(&mut self, stereo_frame: StereoFrame);

    /// Timing info is only reported through the out-parameters the caller
    /// supplies; `None` skips it, as a NULL pointer does in C.
    fn end_frame(&mut self, front_end_msec: Option<&mut i32>, back_end_msec: Option<&mut i32>);

    fn mark_fragments(
        &mut self,
        points: &[Vec3],
        projection: &Vec3,
        max_points: i32,
        point_buffer: &mut [Vec3],
        max_fragments: i32,
        fragment_buffer: &mut [MarkFragment],
    ) -> i32;

    fn lerp_tag(
        &mut self,
        tag: &mut Orientation,
        model: QHandle,
        start_frame: i32,
        end_frame: i32,
        frac: f32,
        tag_name: &str,
    ) -> i32;

    fn model_bounds(&mut self, model: QHandle, mins: &mut Vec3, maxs: &mut Vec3);

    fn register_font(&mut self, font_name: &str, point_size: i32, font: &mut FontInfo);

    fn remap_shader(&mut self, old_shader: &str, new_shader: &str, offset_time: &str);

    /// Copies the next quoted string from the entity text into `buffer`,
    /// returning `false` when the text is exhausted.
    fn get_entity_token(&mut self, buffer: &mut String) -> bool;

    fn in_pvs(&mut self, p1: &Vec3, p2: &Vec3) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ref_types::RefEntityType;

    /// A renderer that does nothing, to prove the interface is implementable
    /// and to pin the signatures against accidental change.
    #[derive(Default)]
    struct StubRenderer {
        frames: i32,
        shader: QHandle,
        scene_entities: usize,
    }

    impl RefExport for StubRenderer {
        fn shutdown(&mut self, _destroy_window: bool) {}

        fn begin_registration(&mut self, _config: &mut GlConfig) {}

        fn register_model(&mut self, _name: &str) -> QHandle {
            1
        }

        fn register_skin(&mut self, _name: &str) -> QHandle {
            2
        }

        fn register_shader(&mut self, _name: &str) -> QHandle {
            3
        }

        fn register_shader_no_mip(&mut self, _name: &str) -> QHandle {
            4
        }

        fn load_world(&mut self, _name: &str) {}

        fn set_world_vis_data(&mut self, _vis: &[u8]) {}

        fn end_registration(&mut self) {}

        fn clear_scene(&mut self) {
            self.scene_entities = 0;
        }

        fn add_ref_entity_to_scene(&mut self, _re: &RefEntity) {
            self.scene_entities += 1;
        }

        fn add_poly_to_scene(
            &mut self,
            _shader: QHandle,
            _verts_per_poly: i32,
            _verts: &[PolyVert],
            num_polys: i32,
        ) -> i32 {
            num_polys
        }

        fn light_for_point(
            &mut self,
            _point: &Vec3,
            ambient_light: &mut Vec3,
            directed_light: &mut Vec3,
            light_dir: &mut Vec3,
        ) -> i32 {
            *ambient_light = [0.0; 3];
            *directed_light = [0.0; 3];
            *light_dir = [0.0, 1.0, 0.0];
            1
        }

        fn add_light_to_scene(&mut self, _org: &Vec3, _intensity: f32, _r: f32, _g: f32, _b: f32) {}

        fn add_additive_light_to_scene(
            &mut self,
            _org: &Vec3,
            _intensity: f32,
            _r: f32,
            _g: f32,
            _b: f32,
        ) {
        }

        fn render_scene(&mut self, _fd: &RefDef) {}

        fn set_color(&mut self, _rgba: Option<&[f32; 4]>) {}

        fn draw_stretch_pic(
            &mut self,
            _x: f32,
            _y: f32,
            _w: f32,
            _h: f32,
            _s1: f32,
            _t1: f32,
            _s2: f32,
            _t2: f32,
            _shader: QHandle,
        ) {
        }

        fn draw_stretch_raw(
            &mut self,
            _x: i32,
            _y: i32,
            _w: i32,
            _h: i32,
            _cols: i32,
            _rows: i32,
            _data: &[u8],
            _client: i32,
            _dirty: bool,
        ) {
        }

        fn upload_cinematic(
            &mut self,
            _w: i32,
            _h: i32,
            _cols: i32,
            _rows: i32,
            _data: &[u8],
            _client: i32,
            _dirty: bool,
        ) {
        }

        fn begin_frame(&mut self, _stereo_frame: StereoFrame) {
            self.frames += 1;
        }

        fn end_frame(&mut self, front_end_msec: Option<&mut i32>, back_end_msec: Option<&mut i32>) {
            if let Some(front) = front_end_msec {
                *front = 1;
            }
            if let Some(back) = back_end_msec {
                *back = 2;
            }
        }

        fn mark_fragments(
            &mut self,
            _points: &[Vec3],
            _projection: &Vec3,
            _max_points: i32,
            _point_buffer: &mut [Vec3],
            _max_fragments: i32,
            fragment_buffer: &mut [MarkFragment],
        ) -> i32 {
            if let Some(first) = fragment_buffer.first_mut() {
                *first = MarkFragment::default();
                1
            } else {
                0
            }
        }

        fn lerp_tag(
            &mut self,
            tag: &mut Orientation,
            _model: QHandle,
            _start_frame: i32,
            _end_frame: i32,
            _frac: f32,
            _tag_name: &str,
        ) -> i32 {
            *tag = Orientation::default();
            0
        }

        fn model_bounds(&mut self, _model: QHandle, mins: &mut Vec3, maxs: &mut Vec3) {
            *mins = [-16.0; 3];
            *maxs = [16.0; 3];
        }

        fn register_font(&mut self, font_name: &str, _point_size: i32, font: &mut FontInfo) {
            let name = font_name.as_bytes();
            let len = name.len().min(font.name.len() - 1);
            font.name[..len].copy_from_slice(&name[..len]);
        }

        fn remap_shader(&mut self, _old: &str, _new: &str, _offset_time: &str) {}

        fn get_entity_token(&mut self, buffer: &mut String) -> bool {
            if buffer.is_empty() {
                buffer.push_str("\"token\"");
                true
            } else {
                false
            }
        }

        fn in_pvs(&mut self, _p1: &Vec3, _p2: &Vec3) -> bool {
            true
        }
    }

    #[test]
    fn the_interface_is_implementable_and_round_trips() {
        let mut renderer = StubRenderer::default();

        let mut config = GlConfig::default();
        config.vid_width = 1280;
        config.vid_height = 720;
        renderer.begin_registration(&mut config);
        assert_eq!(config.vid_width, 1280);

        renderer.begin_frame(StereoFrame::Center);
        renderer.clear_scene();
        renderer.add_ref_entity_to_scene(&RefEntity::new(RefEntityType::Model));
        renderer.add_ref_entity_to_scene(&RefEntity::new(RefEntityType::Sprite));
        assert_eq!(renderer.scene_entities, 2);
        renderer.render_scene(&RefDef::default());

        let mut front = 0;
        let mut back = 0;
        renderer.end_frame(Some(&mut front), Some(&mut back));
        assert_eq!((front, back), (1, 2));
        // The out-parameters are optional, as NULL is in C.
        renderer.end_frame(None, None);

        let mut first = MarkFragment::default();
        let mut buffer = [first];
        first.first_point = 0;
        let count = renderer.mark_fragments(
            &[[0.0; 3]],
            &[0.0, 1.0, 0.0],
            1,
            &mut [[0.0; 3]],
            1,
            &mut buffer,
        );
        assert_eq!(count, 1);

        let mut font = FontInfo::default();
        renderer.register_font("fonts/font", 16, &mut font);
        assert_eq!(&font.name[..10], b"fonts/font");

        let mut token = String::new();
        assert!(renderer.get_entity_token(&mut token));
        assert_eq!(token, "\"token\"");
        assert!(!renderer.get_entity_token(&mut token));

        let mut mins = [0.0; 3];
        let mut maxs = [0.0; 3];
        renderer.model_bounds(1, &mut mins, &mut maxs);
        assert_eq!(mins, [-16.0; 3]);
        assert_eq!(maxs, [16.0; 3]);

        let mut color = None;
        renderer.set_color(color.take());
        renderer.shutdown(false);
        assert_eq!(renderer.frames, 1);
    }
}
