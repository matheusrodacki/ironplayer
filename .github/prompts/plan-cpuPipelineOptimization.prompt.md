# Plan: Otimização do Pipeline de Vídeo (pré-SPEC-09)

Otimizar o pipeline atual (sem hwaccel) atacando hotspots em [crates/av/src/decoder.rs](crates/av/src/decoder.rs), [crates/av/src/ffi/mod.rs](crates/av/src/ffi/mod.rs) e [crates/av/src/renderer.rs](crates/av/src/renderer.rs). Foco: viabilizar 1080i H.264/HEVC fluído nos servidores broadcast (alta contagem de cores, iGPU mínima). Fase intermediária entre o player atual e o SPEC-09 GPU.

Pipeline-alvo após conclusão:

```
FFmpeg decode → (se interlaced) bwdif → YUV planes → egui PaintCallback
                                                          └─ 3× R8Unorm textures
                                                          └─ WGSL fragment shader (YUV→RGB)
```

## Hotspots confirmados

| #   | Onde                                                                                                                                | Problema                                              | Ganho est.             |
| --- | ----------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- | ---------------------- |
| H1  | [crates/av/src/ffi/mod.rs#L587](crates/av/src/ffi/mod.rs#L587) — `avcodec_open2(ctx, codec, NULL)` sem `thread_count`/`thread_type` | Decoder single-thread (ou heurística)                 | **4–8×**               |
| H2  | [crates/av/src/ffi/mod.rs#L751](crates/av/src/ffi/mod.rs#L751) — `to_rgb24` chama `sws_getContext`+`sws_freeContext` por frame      | Swscale recriado por frame + conversão YUV→RGB na CPU | eliminado na Fase 2    |
| H3  | Mesma função: `SWS_BILINEAR` + `vec![0u8; …]`                                                                                       | Filtro inútil + zero-init de 6–25 MB                  | eliminado na Fase 2    |
| H4  | [crates/av/src/decoder.rs#L194](crates/av/src/decoder.rs#L194) — `FfmpegFrame::alloc` por chamada                                   | Alloc de `AVFrame` por PES                            | 1–3 %                  |
| H5  | [crates/av/src/renderer.rs#L81](crates/av/src/renderer.rs#L81) — `rgb24_to_rgba8_into` (segunda passada CPU)                        | Loop extra sobre frame inteiro                        | eliminado na Fase 2    |
| H6  | Nenhum `skip_loop_filter` / `flag2_fast` configurado                                                                                | Decoder em precisão máxima sem necessidade            | 10–25 % H.264          |
| H7  | Sem deinterlacing — 1080i entregue como dois campos costurados                                                                      | Stutter visual em movimento                           | qualidade              |
| H8  | `av_dict_set` / `av_dict_free` não expostos em `FfmpegLib`                                                                          | Bloqueia H1 + H6                                      | —                      |
| H9  | Upload pós-conversão: 8.3 MB/frame RGBA (1080p) via `queue.write_texture`                                                           | Dobro do necessário; YUV420 são 3.1 MB                | ~55 % largura de banda |

## Fases

### Fase 1 — Infra FFI + threading do decoder _(bloqueante, maior ganho absoluto)_

Carregar `av_dict_set` / `av_dict_free` em `FfmpegLib`. Criar `CodecConfig` (`thread_count`, `thread_type`, `skip_loop_filter`, `flag2_fast`) em novo arquivo `crates/av/src/codec.rs`. Alterar `FfmpegCodecContext::open` para aceitar `&CodecConfig` e montar `AVDictionary` antes de chamar `avcodec_open2`. Expor `[decoder]` no `ironstream.toml`.

Default conservador: `thread_count = num_cpus`, `skip_loop_filter = Default`, `flag2_fast = false`.

_Toca em_: [crates/av/src/ffi/mod.rs](crates/av/src/ffi/mod.rs), `crates/av/src/codec.rs` (novo), `src/config.rs`.

### Fase 2 — Upload YUV direto + conversão por shader _(elimina swscale do caminho hot)_

Em vez de converter YUV→RGBA na CPU via swscale:

1. **Decoder entrega `YuvFrame`**: ao receber `AVFrame` com formato `AV_PIX_FMT_YUV420P` (8-bit) ou `AV_PIX_FMT_YUV420P10LE` (10-bit HEVC), copiar os três planos (Y, U, V) como `Vec<u8>` para um novo tipo `YuvFrame { planes, width, height, colorspace, color_range, ten_bit }`. Reuso de `AVFrame` alocado em `CodecState` — sem alloc por frame.
2. **Upload em três texturas**: `R8Unorm` (8-bit) ou `R16Unorm` (10-bit) — uma por plano Y/U/V — via `queue.write_texture`.
3. **Shader WGSL parametrizado**: bind group com as três texturas + uniform `YuvParams { matrix: mat3x3<f32>, offset: vec3<f32>, range_scale: f32 }`. Matrizes embutidas para BT.601, BT.709 e BT.2020; `color_range` determina `range_scale` (TV range 16–235/240 vs full range 0–255). Seleção por frame a partir de `AVFrame::colorspace` + `AVFrame::color_range`.
4. **`egui::PaintCallback`**: substituir `ui.image()` pela alocação de espaço + callback que executa a pipeline custom na `RenderPass` do `egui_wgpu`.

Remove completamente: swscale, `to_rgb24`, `rgb24_to_rgba8_into`, `FfmpegSwsContext`. Remove H2, H3, H4, H5, H9.

_Toca em_: [crates/av/src/ffi/mod.rs](crates/av/src/ffi/mod.rs) (remoção de `to_rgb24` e `FfmpegSwsContext`), [crates/av/src/decoder.rs](crates/av/src/decoder.rs) (produz `YuvFrame`), [crates/av/src/renderer.rs](crates/av/src/renderer.rs) (reescrita: PaintCallback + pipeline WGSL), `crates/av/src/video_queue.rs` (tipo `YuvFrame`), [crates/ui/src/panels/](crates/ui/src/panels/) (uso do PaintCallback).

### Fase 2.5 — Repaint reativo + `PresentMode::Fifo` _(quick-win, paralela)_

- Trocar `ctx.request_repaint_after(Duration::from_millis(16))` por `ctx.request_repaint()` disparado pelo lado receptor do `BoundedSender<YuvFrame>` na chegada do frame — zero polling, latência de display mínima.
- Garantir `present_mode = PresentMode::Fifo` na `SurfaceConfiguration` do eframe. O `wgpu_hal` log "Suboptimal present" é sintoma de present mode incorreto, não de bug — filtrar o log (`wgpu_hal=error`) é paliativo.

_Toca em_: [crates/ui/src/lib.rs](crates/ui/src/lib.rs), configuração de surface do eframe.

### Fase 3 — Tuning de codec _(depende da Fase 1)_

Documentar trade-offs e expor perfis `fast`/`accurate` no toml:

- `skip_loop_filter = NonRef` — reduz 10–25 % CPU em H.264, artefato imperceptível em monitoramento
- `flag2_fast = true` — desativa sub-ME e parte do in-loop filter

_Toca em_: `crates/av/src/codec.rs`, `src/config.rs`.

### Fase 4 — Deinterlacing 1080i via bwdif _(depende da Fase 2, executa antes do upload)_

Carregar `libavfilter` (DLLs já presentes em `ffmpeg/`). Novo `Deinterlacer` em `crates/av/src/deinterlace.rs` com grafo `bwdif=mode=send_frame:parity=auto:deint=interlaced`. Ativado quando `AVFrame::interlaced_frame == 1`. Executa **antes** do upload YUV — saída é `AVFrame` progressivo que alimenta normalmente a Fase 2.

Não fazer deinterlace no shader (qualidade inaceitável para conteúdo broadcast).

_Toca em_: `crates/av/src/deinterlace.rs` (novo), [crates/av/src/decoder.rs](crates/av/src/decoder.rs).

**Invariantes pós-implementação (L-003, não regredir):**

- Buffer source do grafo: incluir `colorspace` e `range` do frame de entrada (evita reconfiguração do link e perda de contexto temporal do bwdif).
- PTS de saída: dividir por 2 via `rescale_bwdif_output_pts` — o filtro dobra o tick count; sem isso a `VideoQueue` retém todos os frames (`TooEarly` vs `AudioClock` 90 kHz).
- Teste de regressão: `cargo test -p av spec_av_005_bwdif_output_pts_halved`.

### Fase 5 — Métricas de pipeline _(depende das Fases 1+2)_

Expor em `crates/ts/src/metrics.rs` e no painel UI:

- `decoder_threads_used` (lido de `AVCodecContext::active_thread_type`)
- `deinterlacer_active` (bool por PID)
- `decode_time_ms_{p50,p99}` por PID
- `gpu_upload_bytes_per_sec` (calculado: planos YUV × fps)
- `colorspace` + `color_range` atuais (diagnóstico de conteúdo)

## Arquivos críticos

| Arquivo                                                      | Fase    | O que muda                                                                          |
| ------------------------------------------------------------ | ------- | ----------------------------------------------------------------------------------- |
| [crates/av/src/ffi/mod.rs](crates/av/src/ffi/mod.rs)         | 1, 2    | Adiciona `av_dict_set`/`av_dict_free`; remove `to_rgb24`/`FfmpegSwsContext`         |
| [crates/av/src/decoder.rs](crates/av/src/decoder.rs)         | 1, 2, 4 | `CodecState` recebe `CodecConfig`; produz `YuvFrame`; chama `Deinterlacer`          |
| [crates/av/src/renderer.rs](crates/av/src/renderer.rs)       | 2       | Reescrita: PaintCallback + 3 texturas + pipeline WGSL; remove `rgb24_to_rgba8_into` |
| `crates/av/src/codec.rs`                                     | 1       | Novo — `CodecConfig` público                                                        |
| `crates/av/src/deinterlace.rs`                               | 4       | Novo — `Deinterlacer` via libavfilter                                               |
| [crates/av/src/video_queue.rs](crates/av/src/video_queue.rs) | 2       | `VideoFrame` → `YuvFrame`                                                           |
| [crates/ui/src/lib.rs](crates/ui/src/lib.rs)                 | 2.5     | Repaint reativo; `PresentMode::Fifo`                                                |
| `src/config.rs`                                              | 1, 3    | Bloco `[decoder]`                                                                   |

## Verificação

- `cargo test -p av -- --include-ignored` (feature `ffmpeg-integration` existente).
- `cargo clippy -p av -- -D warnings`.
- Baseline **antes da Fase 1** com fixture 1080i H.264 do operador: registrar CPU% e `video_frames_dropped`. Após Fase 1+2: alvo ≥ 30 % queda de CPU e zero drops sustentados.
- Correctness de cor: frame capturado antes e depois da Fase 2 deve ser visualmente idêntico para BT.709 full-range e para BT.601 TV-range — testar com ambas as fixtures.
- 10-bit: fixture HEVC `yuv420p10le` deve renderizar sem artefatos de escala (confirmar `R16Unorm` no upload).
- Métricas: painel mostra `decoder_threads_used >= 4` e `colorspace` correto por stream.

## Decisões assumidas

- **Excluído explicitamente**: D3D11VA / hwaccel Vulkan (servidores sem GPU dedicada — escopo do SPEC-09); libyuv SIMD (reavaliar após Fase 2); frame pool customizado (`get_buffer2`).
- `VideoFrame` deixa de existir como tipo público — substituído por `YuvFrame { planes: [Vec<u8>; 3], width, height, colorspace, color_range, ten_bit }`. Afeta apenas `video_queue.rs` e `renderer.rs`.
- Frame threading adiciona latência de ~`thread_count` frames (≈ 320 ms @ 25 fps com N=8) — aceitável para monitoramento broadcast; documentado no `CodecConfig`.
- PTS-based repaint (agendar próximo repaint pelo delta de PTS) é ideia diferida — requer sistema de sincronização A/V (`clock.rs`) que está fora deste escopo.

## Considerações abertas

1. **Defaults conservador vs. agressivo?** Recomendado: conservador. Operador opta por `[decoder.profile = "fast"]` no toml para ativar `skip_loop_filter = NonRef`.
   - Opção A: conservador _(recomendado)_
   - Opção B: agressivo por default
   - Opção C: detectar 1080i automaticamente e ativar `fast` só nesse caso
2. **Ordem de entrega**: 1 → 2 → 2.5 → 5 → 3 → 4. Fases 1+2 entregam ~70–80 % do ganho total.
3. **Baseline numérico antes de começar**: medir com `tracy` ou Process Explorer na fixture do operador _antes da Fase 1_. Sem referência objetiva, "ficou mais rápido" vira opinião.
4. **`egui_wgpu::CallbackFn` vs. `egui_wgpu::Callback`**: verificar qual API o eframe 0.29 expõe — a interface de PaintCallback mudou entre 0.27 e 0.29; confirmar antes de implementar a Fase 2.
