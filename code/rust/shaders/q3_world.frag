#version 450

// One shader stage of the world: the diffuse map, tinted by the baked lightmap
// for the lit pass. Detail and additive stages carry their own colour instead.

layout(set = 0, binding = 0) uniform sampler2D diffuse_map;

layout(push_constant) uniform Push {
    mat4 view_projection;
    vec4 uv_transform;
    vec4 stage_mode;
} push;

layout(location = 0) in vec2 uv;
layout(location = 1) in vec4 color;

layout(location = 0) out vec4 out_color;

void main() {
    vec4 texel = texture(diffuse_map, uv);
    if (texel.a < 0.01) {
        discard;
    }
    vec3 tint = mix(color.rgb, vec3(1.0), push.stage_mode.x);
    out_color = vec4(texel.rgb * tint, texel.a);
}
