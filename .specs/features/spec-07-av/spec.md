# Spec + Tasks: `crates/av` — Bridge FFmpeg

- **Spec-IDs:** SPEC-AV-001, SPEC-AV-002, SPEC-AV-003, SPEC-AV-004
- **Fase:** Alpha v0.2

---

## Requisitos

| ID           | Requisito                               | Critério                                |
| ------------ | --------------------------------------- | --------------------------------------- |
| SPEC-AV-001  | `PesAssembler` remonta PES fragmentado  | PES H.264 em 4 pacotes TS remontado     |
| SPEC-AV-001a | `pts_duration()` correto                | PTS 90kHz → Duration com µs             |
| SPEC-AV-002a | `FfmpegDecoder::from_stream_type`       | H.264, HEVC, Mpeg2, AAC, AC-3, MP2      |
| SPEC-AV-002b | `decode()` retorna frames               | VideoFrame não-vazio para fixture H.264 |
| SPEC-AV-003  | Vídeo exibido sem tearing (1080p/25fps) | Validação visual                        |
| SPEC-AV-003c | Fallback CPU se D3D11 indisponível      | `is_gpu_mode() == false` funciona       |
| SPEC-AV-004  | Áudio sincronizado (desvio < 40 ms)     | Medido contra PTS                       |
| SPEC-AV-004c | `buffer_level()` range 0.0–1.0          | UI pode exibir indicador                |

---

## Tasks

### T01 — `PesAssembler` (SPEC-AV-001)

**Done when:** `spec_av_001_*` passam; PES H.264 de 4 pacotes remontado corretamente.

```
cargo test -p av spec_av_001
```

### T02 — `codec.rs` + `FfmpegDecoder::from_stream_type` (SPEC-AV-002a)

**Depende de:** T01

**Done when:** Todos os `stream_type` suportados retornam `Ok`; desconhecido retorna `Err`.

### T03 — `FfmpegDecoder::decode` e módulo `ffi/` (SPEC-AV-002b)

**Depende de:** T02

**Done when:**
- `spec_av_002b_decode_returns_empty_on_frame_error` passa
- `spec_av_integration_pes_to_frame` (com DLLs FFmpeg) produz `VideoFrame` não-vazio
- Nenhum `unwrap()` fora de `ffi/`

### T04 — `AudioOutput` e `AudioRingBuffer` (SPEC-AV-004)

**Done when:**
- `push_samples`, `set_volume`, `buffer_level` implementados
- Buffer clip funciona para volume > 1.0
- Drop de frames quando buffer > 2× capacidade

### T05 — `VideoRenderer` (SPEC-AV-003)

**Depende de:** T03

**Done when:**
- Upload de `VideoFrame` RGB24 para textura wgpu
- Fallback `egui::ColorImage` se D3D11 indisponível
- `texture_id()` válido para uso em `egui::Image`

### T06 — Verificação de versão das DLLs FFmpeg

**Depende de:** T03

**Done when:** `main.rs` verifica `avcodec_version()` no startup e aborta com mensagem clara se ABI incompatível.

### T07 — Clippy + auditoria de unsafe

**Depende de:** T01–T06

**Done when:**
- `cargo clippy -p av -- -D warnings` passa
- Todos os `unsafe` estão dentro de `ffi/mod.rs`
- Nenhum `unwrap()`/`expect()` em caminhos não-FFI
