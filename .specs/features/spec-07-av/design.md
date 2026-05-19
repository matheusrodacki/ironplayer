# TDD: `crates/av` — Bridge FFmpeg, Renderização e Áudio

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-AV-001, SPEC-AV-002, SPEC-AV-003, SPEC-AV-004
- **Fase:** Alpha v0.2

---

## Contexto e Problema

O crate `av` é a camada mais complexa do IronPlayer por duas razões:

1. **FFI com C:** integração com libavcodec (FFmpeg) exige código `unsafe` que precisa ser isolado e auditado.
2. **Renderização GPU:** upload de frames para textura wgpu e exibição em egui via `TextureId`.
3. **Sincronização A/V:** o buffer de jitter de áudio precisa compensar variações de latência sem causar clicks.

**Decisão central:** TODO `unsafe` é isolado em `av::ffi` — o resto do crate é safe Rust com wrappers idiomáticos.

---

## Escopo

**In-scope:**
- `PesAssembler`: monta PES packets a partir de payloads TS (PUSI-driven)
- `FfmpegDecoder`: wrapper safe sobre libavcodec (h264, hevc, mpeg2video, aac, ac3, mp2, eac3)
- `VideoRenderer`: upload de `VideoFrame` para textura wgpu (D3D11); fallback CPU
- `AudioOutput`: saída WASAPI via cpal com buffer de jitter

**Out-of-scope:**
- Encodificação ou recodificação
- Subtitles / CC (roadmap)
- Múltiplos streams de vídeo simultâneos

---

## Solução Técnica

### Estrutura do crate

```
crates/av/
├── Cargo.toml
└── src/
    ├── lib.rs              # re-exports
    ├── pes.rs              # SPEC-AV-001: PesAssembler, PesPacket
    ├── decoder.rs          # SPEC-AV-002: FfmpegDecoder, DecodedFrame
    ├── renderer.rs         # SPEC-AV-003: VideoRenderer
    ├── audio.rs            # SPEC-AV-004: AudioOutput, AudioRingBuffer
    ├── codec.rs            # VideoCodec, AudioCodec enums + from_stream_type
    └── ffi/
        └── mod.rs          # Todo código unsafe isolado aqui
```

### Dependências (`Cargo.toml`)

```toml
[dependencies]
ts   = { path = "../ts" }        # tipos Pid, PesData
ffmpeg-next  = "7"               # wrapper safe sobre ffmpeg-sys-next
wgpu         = "22"
egui         = { version = "0.29", features = [] }
cpal         = "0.15"
bytes        = { workspace = true }
thiserror    = { workspace = true }
tracing      = { workspace = true }
crossbeam-channel = { workspace = true }

[build-dependencies]
# ffmpeg-next precisa das DLLs em ffmpeg/ no path
```

---

## Contratos de Interface

### `PesAssembler` (SPEC-AV-001)

```rust
pub struct PesAssembler {
    buffers: HashMap<Pid, PesBuffer>,
    tx:      crossbeam_channel::Sender<PesPacket>,
}

pub struct PesPacket {
    pub pid:       Pid,
    pub stream_id: u8,
    pub pts:       Option<i64>,   // 90 kHz; None se ausente
    pub dts:       Option<i64>,
    pub data:      Bytes,         // payload ES (sem PES header)
}

impl PesPacket {
    /// SPEC-AV-001a
    pub fn pts_duration(&self) -> Option<std::time::Duration> {
        self.pts.map(|p| Duration::from_micros((p as u64 * 1_000_000) / 90_000))
    }
}

impl PesAssembler {
    pub fn push(&mut self, pid: Pid, pusi: bool, data: Bytes);
}
```

**Algoritmo (SPEC-AV-001):**
```
Se PUSI=true:
  1. Verificar magic bytes PES: 0x00 0x00 0x01
  2. stream_id = data[3]
  3. pes_length = data[4..5] (0 = unbounded, comum em vídeo)
  4. Decodificar PTS/DTS se pts_dts_flags indicar
  5. Calcular header_length; payload = data[header_len..]
  6. Iniciar PesBuffer

Se PUSI=false:
  Append ao buffer existente para o PID

Emitir PesPacket quando:
  - pes_length != 0 E buffer atingiu pes_length bytes
  - OU próximo PUSI=true (força emissão do PES anterior, comum em vídeo)
```

**Decodificação PTS/DTS (SPEC-AV-001b):**
```
pts_dts_flags = PES_header[1] >> 6   (2 bits)
0b10 → apenas PTS (5 bytes seguindo o flags byte)
0b11 → PTS e DTS (5+5 bytes)
0b00 → nenhum

PTS 33 bits:
  val = (byte[0] & 0x0E) << 29 | byte[1] << 22 | (byte[2] & 0xFE) << 14
       | byte[3] << 7 | (byte[4] >> 1)
```

---

### `FfmpegDecoder` (SPEC-AV-002)

```rust
pub struct FfmpegDecoder {
    kind:    DecoderKind,
    inner:   ffi::CodecContextWrapper,  // unsafe dentro de ffi/
}

pub enum DecoderKind {
    Video { codec: VideoCodec, width: u32, height: u32, fps: f64 },
    Audio { codec: AudioCodec, sample_rate: u32, channels: u16 },
}

impl FfmpegDecoder {
    /// SPEC-AV-002a: inicializa a partir do stream_type da PMT
    pub fn from_stream_type(stream_type: u8) -> Result<Self, AvError>;

    /// SPEC-AV-002b: envia PES; retorna 0+ frames decodificados
    pub fn decode(&mut self, pes: &PesPacket) -> Result<Vec<DecodedFrame>, AvError>;
}

pub enum DecodedFrame {
    Video(VideoFrame),
    Audio(AudioFrame),
}

pub struct VideoFrame {
    pub width:  u32,
    pub height: u32,
    pub pts:    Option<i64>,
    pub data:   Vec<u8>,   // RGB24 fixo
    pub stride: u32,
}

pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels:    u16,
    pub pts:         Option<i64>,
    pub samples:     Vec<f32>,   // PCM interleaved f32
}
```

**Codecs suportados v1.0 (SPEC-AV-002c):**

| `stream_type` | `VideoCodec` | decoder      |
| ------------- | ------------ | ------------ |
| 0x1B          | `H264`       | `h264`       |
| 0x24          | `Hevc`       | `hevc`       |
| 0x02          | `Mpeg2Video` | `mpeg2video` |

| `stream_type` | `AudioCodec` | decoder |
| ------------- | ------------ | ------- |
| 0x0F / 0x11   | `Aac`        | `aac`   |
| 0x81          | `Ac3`        | `ac3`   |
| 0x87          | `Eac3`       | `eac3`  |
| 0x03 / 0x04   | `Mp2`        | `mp2`   |

**Contrato de erro (SPEC-AV-002b):**
- Erro de frame individual: loga `WARN`, retorna `Ok(vec![])`
- Erro fatal (contexto inválido): retorna `Err(AvError::DecoderFailed)`
- Nunca faz `unwrap()` / `panic!()` em código safe

---

### `VideoRenderer` (SPEC-AV-003)

```rust
pub struct VideoRenderer {
    device:       wgpu::Device,
    queue:        wgpu::Queue,
    texture:      wgpu::Texture,
    texture_view: wgpu::TextureView,
    texture_id:   egui::TextureId,
    gpu_mode:     bool,
}

impl VideoRenderer {
    /// SPEC-AV-003a: cria renderer; tenta D3D11; fallback para CPU se falhar
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, renderer: &mut egui_wgpu::Renderer) -> Self;

    /// SPEC-AV-003a: upload frame; recria textura se dimensões mudarem
    pub fn upload_frame(&mut self, frame: &VideoFrame);

    /// SPEC-AV-003b: TextureId para uso em egui::Image
    pub fn texture_id(&self) -> egui::TextureId;

    /// SPEC-AV-003c
    pub fn is_gpu_mode(&self) -> bool;
}
```

**Fallback CPU (SPEC-AV-003c):**
```
Se wgpu::Device::create() falhar:
  Usar egui::ColorImage::from_rgb() para criar imagem CPU
  Registrar como egui texture via Context::load_texture()
  is_gpu_mode() = false
```

---

### `AudioOutput` (SPEC-AV-004)

```rust
pub struct AudioOutput {
    stream:      cpal::Stream,
    buffer:      Arc<Mutex<AudioRingBuffer>>,
    sample_rate: u32,
    channels:    u16,
}

struct AudioRingBuffer {
    data:     VecDeque<f32>,
    capacity: usize,  // default: 100ms de amostras
}

impl AudioOutput {
    /// SPEC-AV-004a: thread-safe; pode ser chamado do decoder thread
    pub fn push_samples(&self, frame: &AudioFrame);

    /// SPEC-AV-004b: volume 0.0–2.0; >1.0 = boost com clip
    pub fn set_volume(&self, volume: f32);

    /// SPEC-AV-004c: ocupação do buffer (0.0–1.0) para indicador de saúde
    pub fn buffer_level(&self) -> f32;
}
```

**Drop de áudio (SPEC-CHAN-001 + SPEC-AV-004):**
```
Se buffer > 2× capacidade nominal: descartar frames mais antigos
Garantir que WASAPI callback sempre receba amostras (silêncio se buffer vazio)
```

---

## Estratégia de Testes

### Unitários

```
spec_av_001_pes_single_packet_pusi
spec_av_001_pes_fragmented_4_packets
spec_av_001_pes_pts_decode_known_value
spec_av_001_pes_no_pts

spec_av_002a_from_stream_type_h264
spec_av_002a_from_stream_type_unknown_returns_err
spec_av_002b_decode_returns_empty_on_frame_error   (mock de erro de frame)

spec_av_004_buffer_level_empty
spec_av_004_buffer_level_full
spec_av_004_volume_clip_above_1
```

### Integração

```
spec_av_integration_pes_to_frame   — fixture H.264 real → VideoFrame não-vazio
```

> **Nota:** testes de `FfmpegDecoder` precisam das DLLs FFmpeg no path; marcar com `#[cfg(feature = "integration")]` para CI.

---

## Considerações de Segurança

- **Isolamento de `unsafe`:** todo código FFI com libavcodec fica em `ffi/mod.rs`. Qualquer PR que adicione `unsafe` fora deste módulo requer revisão obrigatória.
- `VideoFrame::data` (RGB24) é alocado pela libavcodec e copiado para `Vec<u8>` owned antes de cruzar a barreira FFI — sem referências pendentes a memória C.
- `AudioRingBuffer` protegido por `Mutex` — acesso da thread de decodificação e do callback WASAPI.
- DLLs FFmpeg carregadas via path fixo (relativo ao executável) — sem carregamento de path fornecido pelo usuário.

---

## Riscos e Mitigações

| Risco                                                       | Mitigação                                                                                    |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| libavcodec aloca contextos globais não thread-safe          | Criar um `AVCodecContext` por `FfmpegDecoder`; nunca compartilhar                            |
| Frames com dimensões diferentes causam realocação frequente | Cache de textura por dimensão; evitar recriação em cada frame                                |
| `cpal` WASAPI pode falhar se dispositivo de saída mudou     | Tratar `StreamError` como recuperável; tentar reconectar em dispositivo padrão               |
| DLLs FFmpeg com versão errada (ABI incompatível)            | Verificar versão via `avcodec_version()` no startup; logar erro e abortar com mensagem clara |
