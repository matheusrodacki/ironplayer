# Otimização do Pipeline de Vídeo (pré-SPEC-09)

## Overview

Refatorar o pipeline CPU de vídeo do IronPlayer para viabilizar 1080i H.264/HEVC fluído em servidores broadcast sem GPU dedicada. O pipeline atual decodifica via FFmpeg single-thread, converte YUV→RGB24 via swscale por frame e realinha para RGBA8 na CPU antes do upload wgpu. A meta é: multithreading no decoder, eliminação do swscale via upload YUV direto + shader WGSL, deinterlacing bwdif para 1080i e métricas de pipeline no painel UI. Antes de iniciar a tarefa, leia o plano na integra `.github\prompts\plan-cpuPipelineOptimization.prompt.md`

Pipeline-alvo:
```
FFmpeg decode (N threads) → (se interlaced) bwdif → YUV planes → egui PaintCallback
                                                                      └─ 3× R8/R16Unorm textures
                                                                      └─ WGSL fragment shader (YUV→RGB)
```

## Tasks

- [x] Task 1: Implementar infra FFI mínima e multithreading do decoder — carregar `av_dict_set`/`av_dict_free` em `FfmpegLib` (`crates/av/src/ffi/mod.rs`), criar `crates/av/src/codec.rs` com struct `CodecConfig` (`thread_count`, `thread_type`, `skip_loop_filter`, `flag2_fast`), alterar `FfmpegCodecContext::open` para aceitar `&CodecConfig` e passar `AVDictionary` como terceiro argumento de `avcodec_open2`, expor bloco `[decoder]` em `src/config.rs` e `ironstream.toml`. Default conservador: `thread_count = num_cpus`, demais flags desligadas.

- [x] Task 2: Substituir swscale por upload YUV direto — remover `to_rgb24`, `FfmpegSwsContext` e `rgb24_to_rgba8_into` do codebase; criar tipo `YuvFrame { planes: [Vec<u8>; 3], width, height, colorspace, color_range, ten_bit }` em `crates/av/src/video_queue.rs`; alterar `crates/av/src/decoder.rs` para reusar `AVFrame` alocado em `CodecState` e produzir `YuvFrame` (suporte a `YUV420P` 8-bit e `YUV420P10LE` 10-bit).

- [x] Task 3: Implementar pipeline de renderização GPU com shader WGSL — reescrever `crates/av/src/renderer.rs` com `egui::PaintCallback` + `egui_wgpu`; criar 3 texturas `R8Unorm`/`R16Unorm` (Y, U, V); escrever shader WGSL com uniform `YuvParams { matrix: mat3x3<f32>, offset: vec3<f32>, range_scale: f32 }` parametrizado por colorspace (BT.601/709/2020) e `color_range` (TV range vs full range), lidos de `YuvFrame`; substituir `ui.image()` pelo PaintCallback em `crates/ui/src/panels/`.

- [x] Task 4: Implementar deinterlacing 1080i via bwdif e repaint reativo — criar `crates/av/src/deinterlace.rs` carregando `libavfilter` (DLLs já em `ffmpeg/`) com grafo `bwdif=mode=send_frame:parity=auto:deint=interlaced`, ativado quando `AVFrame::interlaced_frame == 1`, executando antes do upload YUV; trocar `ctx.request_repaint_after(16ms)` por `ctx.request_repaint()` na chegada do frame em `crates/ui/src/lib.rs`; garantir `PresentMode::Fifo` na `SurfaceConfiguration` do eframe.

- [x] Task 5: Expor perfis de codec e métricas de pipeline — adicionar perfis `fast`/`accurate` no toml (`skip_loop_filter = NonRef`, `flag2_fast`); expor em `crates/ts/src/metrics.rs`: `decoder_threads_used`, `deinterlacer_active`, `decode_time_ms_p50/p99` por PID, `gpu_upload_bytes_per_sec`, `colorspace` e `color_range`; exibir no painel UI; validar com `cargo test -p av -- --include-ignored` e `cargo clippy -p av -- -D warnings`.

## Technical Details

- **Stack**: Rust 1.78, `crates/av` (FFmpeg 7.x dynamic FFI), `crates/ui` (egui/eframe 0.29 + wgpu D3D11 backend), crossbeam-channel bounded, tokio.
- **Dependências chave**: `egui_wgpu::CallbackFn` (verificar API exata no eframe 0.29 antes de implementar Task 3); `libavfilter` DLLs já em `ffmpeg/`.
- **Restrições de segurança**: zero `unsafe` fora de `crates/av/src/ffi/`; canais bounded com `try_send_latest` — sem `unbounded()`; todo parsing retorna `Result`, nunca `.unwrap()` em dados externos.
- **Excluído do escopo**: D3D11VA / hwaccel Vulkan (SPEC-09); libyuv SIMD; frame pool via `get_buffer2`.
- **Gate de cada task**: `cargo test -p av` verde + `cargo clippy -p av -- -D warnings` sem avisos.
- **Baseline antes de começar**: medir CPU% e `video_frames_dropped` com fixture 1080i H.264 do operador antes da Task 1. Alvo pós-Task 2+3: queda ≥ 30 % CPU, zero drops sustentados.
