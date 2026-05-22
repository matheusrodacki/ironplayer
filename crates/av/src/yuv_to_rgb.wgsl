// YUV → RGB fragment shader. SPEC-AV-003
//
// Parâmetros:
//   matrix     : mat3x3f coluna-maior; converte [Y, U-0.5, V-0.5] → RGB normalizado.
//   offset     : vec3f   bias aplicado ao resultado (cte RGB proveniente da remoção do
//                        offset de chroma e da escala de range de luma).
//   range_scale: f32     fator de escala para o canal Y (luma).
//                        TV-range 8-bit → 255/219 ≈ 1.1644
//                        Full-range      → 1.0
//
// WGSL override: DECODE_SRGB (f32)
//   0.0 (padrão) → target linear (ex.: Bgra8Unorm); saída em gamma-encoded (Y'CbCr′ → R′G′B′).
//   1.0          → target sRGB   (ex.: Bgra8UnormSrgb); saída linearizada para
//                  compensar a dupla codificação gamma pelo framebuffer.

override DECODE_SRGB: f32 = 0.0;

struct YuvParams {
    /// Matriz 3×3 coluna-maior para mistura R′G′B′.
    matrix: mat3x3<f32>,
    /// Offset constante RGB pós-conversão.
    offset: vec3<f32>,
    /// Fator de escala do canal de luma Y (TV-range vs full-range).
    range_scale: f32,
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp:  sampler;
@group(0) @binding(4) var<uniform> params: YuvParams;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0)       uv:       vec2<f32>,
}

// ── Vértice: triângulo de tela cheia ──────────────────────────────────────────

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOutput {
    // Triângulo que cobre exatamente o NDC [-1, +1] × [-1, +1].
    // UV (0,0) = canto superior-esquerdo; (1,1) = canto inferior-direito.
    var pos: array<vec2<f32>, 3> = array(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var uv_coords: array<vec2<f32>, 3> = array(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(pos[vi], 0.0, 1.0);
    out.uv       = uv_coords[vi];
    return out;
}

// ── Fragmento: YUV → RGB ──────────────────────────────────────────────────────

fn srgb_to_linear_channel(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let u = textureSample(tex_u, samp, in.uv).r - 0.5;
    let v = textureSample(tex_v, samp, in.uv).r - 0.5;

    // Aplica escala de range ao canal Y antes da multiplicação de matriz.
    let yuv = vec3<f32>((y - params.offset.x) * params.range_scale, u, v);
    var rgb  = params.matrix * yuv + params.offset;
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    // Quando o framebuffer alvo é sRGB o driver gamma-encoda a saída linear;
    // linearizamos aqui para compensar (resultando em identidade net).
    if DECODE_SRGB > 0.5 {
        rgb = vec3<f32>(
            srgb_to_linear_channel(rgb.r),
            srgb_to_linear_channel(rgb.g),
            srgb_to_linear_channel(rgb.b),
        );
    }

    return vec4<f32>(rgb, 1.0);
}
