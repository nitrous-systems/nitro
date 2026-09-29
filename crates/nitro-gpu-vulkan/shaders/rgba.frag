// Sample one texture. Used for RGBA *and* YCbCr textures: the YCbCr->RGB
// conversion lives in the immutable sampler of the descriptor set layout,
// so the shader is the same. `opaque` forces alpha to 1 (XR24, NV12).
// Regenerate the .spv with `just gpu-shaders`.
#version 450
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(push_constant) uniform PC {
    vec4 dst;
    vec4 src;
    uint opaque;
} pc;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;
void main() {
    vec4 c = texture(tex, uv);
    if (pc.opaque != 0u) {
        c.a = 1.0;
    }
    color = c;
}
