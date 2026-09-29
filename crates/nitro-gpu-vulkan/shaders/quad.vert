// One textured quad as a 4-vertex triangle strip, no vertex buffer.
// Regenerate the .spv with `just gpu-shaders`.
#version 450
layout(push_constant) uniform PC {
    vec4 dst;   // x0, y0, x1, y1 in NDC
    vec4 src;   // u0, v0, u1, v1, normalized
    uint opaque;
} pc;
layout(location = 0) out vec2 uv;
void main() {
    vec2 t = vec2(float(gl_VertexIndex & 1), float(gl_VertexIndex >> 1));
    uv = mix(pc.src.xy, pc.src.zw, t);
    gl_Position = vec4(mix(pc.dst.xy, pc.dst.zw, t), 0.0, 1.0);
}
