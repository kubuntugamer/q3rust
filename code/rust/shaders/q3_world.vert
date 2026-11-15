#version 450

// World geometry: one matrix, the lightmap folded into the vertex colour, and
// the stage's own texture-coordinate generation, scroll, scale and texmods.

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_color;
layout(location = 3) in vec3 in_normal;

layout(push_constant) uniform Push {
    mat4 view_projection;
    // `xy` scale the texture coordinates, `zw` scroll them.
    vec4 uv_transform;
    // `x` is 1 for a stage the baked lightmap must not tint, `y` selects
    // `tcGen environment`, `z` is the rotation in radians, `w` the stretch.
    vec4 stage_mode;
    // `x` amplitude, `y` phase, `z` frequency, `w` seconds.
    vec4 turbulence;
    // `xyz` the eye the environment map reflects about.
    vec4 camera;
} push;

layout(location = 0) out vec2 uv;
layout(location = 1) out vec4 color;

void main() {
    vec2 st;
    if (push.stage_mode.y != 0.0) {
        // `RB_CalcEnvironmentTexCoords`: reflect the view in the surface
        // normal and read the environment as the reflection's y and z.
        vec3 normal = normalize(in_normal);
        vec3 viewer = normalize(push.camera.xyz - in_position);
        vec3 reflected = normal * (2.0 * dot(normal, viewer)) - viewer;
        st = vec2(0.5 + reflected.y * 0.5, 0.5 - reflected.z * 0.5);
        st = st * push.uv_transform.xy + push.uv_transform.zw;
    } else {
        st = in_uv * push.uv_transform.xy + push.uv_transform.zw;
    }

    // `RB_CalcStretchTexCoords` and `RB_CalcRotateTexCoords` both turn the
    // coordinates about the middle of the texture.
    if (push.stage_mode.w != 1.0) {
        st = (st - vec2(0.5)) * push.stage_mode.w + vec2(0.5);
    }
    if (push.stage_mode.z != 0.0) {
        float cs = cos(push.stage_mode.z);
        float sn = sin(push.stage_mode.z);
        vec2 centred = st - vec2(0.5);
        st = vec2(centred.x * cs - centred.y * sn,
                  centred.x * sn + centred.y * cs)
            + vec2(0.5);
    }

    if (push.turbulence.x != 0.0) {
        // `RB_CalcTurbulentTexCoords`: one turn over the C's sine table,
        // addressed in 1/1024 of a world unit along each axis.
        float now = push.turbulence.y + push.turbulence.w * push.turbulence.z;
        st.x += sin(6.2831853 * ((in_position.x + in_position.z) * 0.0009765625 + now))
            * push.turbulence.x;
        st.y += sin(6.2831853 * (in_position.y * 0.0009765625 + now))
            * push.turbulence.x;
    }
    uv = st;
    color = in_color;
    gl_Position = push.view_projection * vec4(in_position, 1.0);
}
