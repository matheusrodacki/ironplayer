// Shader WGSL: NV12 semi-planar YUV → RGB — Fase C zero-copy.
//
// Dois planos:
//   tex_y  — R8Unorm   (luma Y, W × H)
//   tex_uv — Rg8Unorm  (croma UV interleaved, W/2 × H/2; R=U/Cb, G=V/Cr)
//
// Usa o mesmo UBO de 64 bytes que o pipeline YUV420P tri-planar (YuvParams):
//   mat3x3f matrix         — 48 bytes (3 colunas × vec4f)
//   vec3f   offset         — 12 bytes
//   f32     range_scale    —  4 bytes
//
// Referência: mesma estrutura de `yuv_to_rgb.wgsl`, apenas binding 1 e 2
// substituídos por um único `tex_uv` Rg8Unorm.
//
// SPEC-AV-RENDER-NV12-001

override DECODE_SRGB: f32 = 0.0;

struct YuvParams {
    /// Colunas da matriz YUV→RGB (3 × vec4f em std140).
    col0: vec4<f32>,
    col1: vec4<f32>,
    col2: vec4<f32>,
    /// xyz = offset RGB constante; w = range_scale para Y.
    offset_and_range: vec4<f32>,
}

@group(0) @binding(0) var tex_y:  texture_2d<f32>;   // R8Unorm  — plano luma
@group(0) @binding(1) var tex_uv: texture_2d<f32>;   // Rg8Unorm — plano croma UV
@group(0) @binding(2) var samp:   sampler;
@group(0) @binding(3) var<uniform> params: YuvParams;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0)       uv:       vec2<f32>,
}

/// Vértice: triângulo fullscreen sem VBO.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOutput {
    // Dois triângulos degenerados → um único triângulo que cobre toda a tela.
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[vi], 0.0, 1.0);
    out.uv       = uvs[vi];
    return out;
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

/// Fragment: NV12 → RGB via matriz BT.xxx configurável.
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y  = textureSample(tex_y,  samp, in.uv).r;
    let uv = textureSample(tex_uv, samp, in.uv).rg;

    // Centraliza croma: NV12 armazena U/V em [0, 1]; 128/255 ≈ 0.502 ≈ 0.5.
    let u = uv.r - 0.5;
    let v = uv.g - 0.5;

    let offset      = params.offset_and_range.xyz;
    let range_scale = params.offset_and_range.w;

    // Aplica escala de range no luma (1.164 para TV-range, 1.0 para full).
    let y_scaled = (y - offset.x) * range_scale;

    // Reconstrói o vetor YUV e aplica a matriz de cor.
    let yuv = vec3<f32>(y_scaled, u, v);

    // A matriz está armazenada em três colunas vec4f (std140 mat3x3).
    let col0 = params.col0.xyz;
    let col1 = params.col1.xyz;
    let col2 = params.col2.xyz;
    var rgb = mat3x3<f32>(col0, col1, col2) * yuv + offset;

    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    // Decodifica sRGB → linear quando o framebuffer de saída é sRGB.
    if DECODE_SRGB > 0.5 {
        rgb = vec3<f32>(
            srgb_to_linear(rgb.r),
            srgb_to_linear(rgb.g),
            srgb_to_linear(rgb.b),
        );
    }

    return vec4<f32>(rgb, 1.0);
}
