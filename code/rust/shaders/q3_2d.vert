#version 450

// 2D layer: positions arrive in engine pixels with the origin at the top left,
// which is how Quake's DrawStretchPic addresses the screen. The push constant
// carries whatever mapping the renderer decided on, so the shader stays fixed
// when the virtual resolution changes.

layout(push_constant) uniform Mapping {
    vec2 scale;   // 2/width, -2/height
    vec2 offset;  // -1, +1
} map;

layout(location = 0) in vec2 in_pos;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_color;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;

void main() {
    v_uv = in_uv;
    v_color = in_color;
    gl_Position = vec4(in_pos * map.scale + map.offset, 0.0, 1.0);
}
