#version 450

// Every 2D draw in Quake is a shader with a texture, a vertex color and either
// an opaque or a blendfunc stage. The renderer picks the blend state per
// pipeline, so this shader only has to modulate and let the alpha through.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) out vec4 out_color;

void main() {
    vec4 texel = texture(tex, v_uv);
    out_color = texel * v_color;
}
