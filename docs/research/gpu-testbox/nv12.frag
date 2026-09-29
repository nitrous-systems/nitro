#version 450
layout(location=0) in vec2 uv;
layout(location=0) out vec4 o;
layout(set=0,binding=0) uniform sampler2D tex;   // immutable sampler carries the VkSamplerYcbcrConversion
void main(){ o=vec4(texture(tex,uv).rgb,1); }
