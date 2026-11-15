#version 450

// World geometry: one matrix, the lightmap folded into the vertex colour, and
// the stage's own texture-coordinate transform (scroll and scale).

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_color;

layout(push_constant) uniform Push {
    mat4 view_projection;
    // `xy` scale the texture coordinates, `zw` scroll them.
    vec4 uv_transform;
    // `x` is 1 for a stage the baked lightmap must not tint.
    vec4 stage_mode;
    // `x` amplitude, `y` phase, `z` frequency, `w` seconds.
    vec4 turbulence;
    // `x` cosine and `y` sine of the rotation, `z` the stretch scale.
    vec4 tc_mods;
} push;

layout(location = 0) out vec2 uv;
layout(location = 1) out vec4 color;

void main() {
    vec2 st = in_uv * push.uv_transform.xy + push.uv_transform.zw;
    // `RB_CalcStretchTexCoords` and `RB_CalcRotateTexCoords` both turn the
    // coordinates about the middle of the texture.
    if (push.tc_mods.z != 1.0) {
        st = (st - vec2(0.5)) * push.tc_mods.z + vec2(0.5);
    }
    if (push.tc_mods.x != 1.0 || push.tc_mods.y != 0.0) {
        vec2 centred = st - vec2(0.5);
        st = vec2(centred.x * push.tc_mods.x - centred.y * push.tc_mods.y,
                  centred.x * push.tc_mods.y + centred.y * push.tc_mods.x)
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
