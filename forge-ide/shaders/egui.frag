#version 450

layout(set = 0, binding = 0) uniform sampler2D font_tex;

layout(location = 0) in  vec4 in_color;
layout(location = 1) in  vec2 in_uv;
layout(location = 0) out vec4 out_color;

vec3 linear_from_srgb(vec3 s) {
    bvec3 lo = lessThan(s, vec3(0.04045));
    return mix(pow((s + 0.055) / 1.055, vec3(2.4)), s / 12.92, lo);
}

void main() {
    vec4 tex    = texture(font_tex, in_uv);
    vec4 linear = vec4(linear_from_srgb(in_color.rgb), in_color.a);
    out_color   = linear * tex;
}
