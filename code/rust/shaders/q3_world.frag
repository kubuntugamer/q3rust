#version 450

// The diffuse stage with the baked lightmap multiplied in, as
// RB_CalcDiffuseColor does for the world.

layout(set = 0, binding = 0) uniform sampler2D diffuse_map;

layout(location = 0) in vec2 uv;
layout(location = 1) in vec4 color;

layout(location = 0) out vec4 out_color;

void main() {
    vec4 texel = texture(diffuse_map, uv);
    if (texel.a < 0.01) {
        discard;
    }
    out_color = vec4(texel.rgb * color.rgb, 1.0);
}
