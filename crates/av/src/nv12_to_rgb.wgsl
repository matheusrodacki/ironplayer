// Shader WGSL: NV12/P010 semi-planar YUV → RGB — Fase D.
//
// Fase D: matriz YUV→RGB parametrizada por colorspace e TRC. Conteúdo HDR10
// (PQ/HLG + BT.2020) passa por tone mapping HDR→SDR e gamut BT.2020→BT.709
// antes da codificação para o framebuffer (BT.1886 em UNORM, sRGB em sRGB).

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

fn bt1886_to_linear(c: f32) -> f32 {
    return pow(clamp(c, 0.0, 1.0), 2.4);
}

fn pq_to_linear(c: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let x = pow(clamp(c, 0.0, 1.0), 1.0 / m2);
    let num = max(x - c1, 0.0);
    let den = max(c2 - c3 * x, 1e-6);
    return pow(num / den, 1.0 / m1);
}

fn hlg_to_linear(c: f32) -> f32 {
    if c <= 0.5 {
        return (c * c) / 3.0;
    }
    let a = 0.17883277;
    let b = 0.28466892;
    let cc = 0.55991073;
    return (exp((c - cc) / a) + b) / 12.0;
}

fn decode_transfer(rgb: vec3<f32>, mode: f32) -> vec3<f32> {
    if mode < 0.5 {
        return vec3<f32>(
            bt1886_to_linear(rgb.r),
            bt1886_to_linear(rgb.g),
            bt1886_to_linear(rgb.b),
        );
    }
    if mode < 1.5 {
        return vec3<f32>(
            pq_to_linear(rgb.r),
            pq_to_linear(rgb.g),
            pq_to_linear(rgb.b),
        );
    }
    if mode < 2.5 {
        return vec3<f32>(
            hlg_to_linear(rgb.r),
            hlg_to_linear(rgb.g),
            hlg_to_linear(rgb.b),
        );
    }
    return vec3<f32>(
        srgb_to_linear(rgb.r),
        srgb_to_linear(rgb.g),
        srgb_to_linear(rgb.b),
    );
}

fn linear_to_bt1886(c: f32) -> f32 {
    return pow(clamp(c, 0.0, 1.0), 1.0 / 2.4);
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        return c * 12.92;
    }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

fn bt2020_to_bt709(rgb: vec3<f32>) -> vec3<f32> {
    // Colunas da matriz BT.2020→BT.709 (ITU-R BT.2407); WGSL usa colunas.
    return mat3x3<f32>(
        vec3(1.6605, -0.1246, -0.0182),
        vec3(-0.5876, 1.1329, -0.1006),
        vec3(-0.0728, -0.0083, 1.1187),
    ) * rgb;
}

/// Tone mapping HDR→SDR. Entrada em luz linear (PQ: 1.0 = 10000 nits).
fn tone_map_hdr_to_sdr(rgb: vec3<f32>, transfer: f32) -> vec3<f32> {
    var c = rgb;
    if transfer < 1.5 {
        c = rgb * 25.0;
    } else if transfer < 2.5 {
        c = rgb * 2.5;
    }
    c = c / (1.0 + c);
    return clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
}

/// Fragment: NV12 → RGB via matriz BT.xxx configurável.
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y  = textureSample(tex_y,  samp, in.uv).r;
    let uv = textureSample(tex_uv, samp, in.uv).rg;

    let uv_center = params.offset_and_range.y;
    let u = uv.r - uv_center;
    let v = uv.g - uv_center;

    let y_offset    = params.offset_and_range.x;
    let range_scale = params.offset_and_range.w;
    let transfer    = params.col0.w;
    let hdr_clip    = params.col1.w;
    let gamut_map   = params.offset_and_range.z;

    let y_scaled = max((y - y_offset) * range_scale, 0.0);

    // Reconstrói o vetor YUV e aplica a matriz de cor.
    let yuv = vec3<f32>(y_scaled, u, v);

    // A matriz está armazenada em três colunas vec4f (std140 mat3x3).
    let col0 = params.col0.xyz;
    let col1 = params.col1.xyz;
    let col2 = params.col2.xyz;
    var rgb = mat3x3<f32>(col0, col1, col2) * yuv;
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    if hdr_clip > 0.5 {
        rgb = decode_transfer(rgb, transfer);
        if gamut_map > 0.5 {
            rgb = bt2020_to_bt709(rgb);
        }
        rgb = tone_map_hdr_to_sdr(rgb, transfer);
        if DECODE_SRGB > 0.5 {
            rgb = vec3<f32>(
                linear_to_srgb(rgb.r),
                linear_to_srgb(rgb.g),
                linear_to_srgb(rgb.b),
            );
        } else {
            rgb = vec3<f32>(
                linear_to_bt1886(rgb.r),
                linear_to_bt1886(rgb.g),
                linear_to_bt1886(rgb.b),
            );
        }
    } else if DECODE_SRGB > 0.5 {
        rgb = decode_transfer(rgb, transfer);
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    return vec4<f32>(rgb, 1.0);
}
