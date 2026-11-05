#version 450

// World geometry: one matrix, the lightmap folded into the vertex colour.

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_color;

layout(push_constant) uniform Push {
    mat4 view_projection;
} push;

layout(location = 0) out vec2 uv;
layout(location = 1) out vec4 color;

void main() {
    uv = in_uv;
    color = in_color;
    gl_Position = push.view_projection * vec4(in_position, 1.0);
}
