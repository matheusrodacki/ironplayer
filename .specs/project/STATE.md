# IronPlayer — State

> Memória persistente do projeto: decisões, bloqueadores, lições aprendidas, ideias deferidas.

---

## Decisões Arquiteturais

| Data       | ID    | Decisão                                                   | Razão                                                                       |
| ---------- | ----- | --------------------------------------------------------- | --------------------------------------------------------------------------- |
| 2026-05-19 | D-001 | Usar `crossbeam-channel` (bounded) para pipeline TS       | Canais sync de alta performance; backpressure nativa; sem tokio no hot path |
| 2026-05-19 | D-002 | `ts` e `net` sem dependências internas (zero FFI)         | Facilita testes unitários, portabilidade futura para Linux                  |
| 2026-05-19 | D-003 | FFmpeg apenas para decode (não para demux nem transport)  | Mantém parser TS em Rust puro; FFmpeg só para codec A/V                     |
| 2026-05-19 | D-004 | UI lê `MetricsSnapshot` imutável via `tokio::sync::watch` | UI nunca bloqueia o pipeline; 1 Hz de atualização é suficiente              |
| 2026-05-19 | D-005 | Todo `unsafe` isolado no módulo `av::ffi`                 | Facilita auditoria de segurança; resto do código é safe Rust                |
| 2026-05-19 | D-006 | egui/eframe com backend wgpu (D3D11 no Windows)           | Immediate-mode simplifica estado de UI; D3D11 nativo no target              |
| 2026-05-19 | D-007 | MSRV Rust 1.78 (stable)                                   | Suporte a `impl Trait` em posições variadas; disponível no CI               |
| 2026-06-24 | D-008 | `AudioClockHandle::anchor_pts` imutável na UI; skew de mux absorvido pela `VideoQueue` (cap=64) | `shift_anchor` na UI adiantava vídeo ~1–2 s vs áudio audível; cf. L-002 |
| 2026-06-26 | D-009 | UI Broadcast migrada de egui para **Slint 1.17** (femtovg); `crates/ui` removida, nova `crates/ui-slint`; vídeo via conversão CPU YUV→RGBA em thread worker (sem device wgpu compartilhado) | POC de reestilização (spec-11-slint). femtovg escolhido por compatibilidade com `+crt-static` (Skia/`skia-bindings` conflita). `av` mantém egui interno. Decoder já entrega planos na CPU → não precisa de zero-copy para um 1º corte. cf. L-006 |

---

## Bloqueadores / Riscos

| ID    | Risco                                                             | Mitigação                                                                  |
| ----- | ----------------------------------------------------------------- | -------------------------------------------------------------------------- |
| R-001 | Licença das DLLs FFmpeg (LGPL)                                    | Linkar dinamicamente; distribuir DLLs separadas; não incorporar ao binário |
| R-002 | `ffmpeg-next` crate pode ficar desatualizado                      | Manter fork interno se necessário; versão fixada no Cargo.lock             |
| R-003 | wgpu D3D11 pode não funcionar em VMs / GPUs antigas               | Fallback CPU via `swscale` já especificado em SPEC-AV-003c                 |
| R-004 | Fixtures de teste para DVB real podem ter restrições de copyright | Usar capturas sintéticas geradas por script em vez de broadcasts reais     |

---

## Ideias Deferidas (pós-v1.0)

- Suporte Linux/macOS (wgpu já é cross-platform; cpal também)
- Input via HTTP(S) / HLS / DASH (adicionar crate `reqwest` + parser)
- CLI separada para análise headless em pipelines
- Exportação de relatório de análise em JSON/CSV
- Plugin system para decoders externos
- Suporte a arquivos `.ts` locais (além de UDP/RTP)

---

## Lições Aprendidas

### L-001 — D3D11VA / A/V sync: armadilhas de debug (2026-06-23)

Sessão de debug em stream multicast H.264 (`h264_d3d11va`, Intel Arc). Sintomas e **invariantes que não podem regredir**:

| Sintoma | Causa raiz | Invariante obrigatória |
| ------- | ---------- | ---------------------- |
| Tela preta + `Map UV: 0x80070057` | `Map(subresource 1)` em textura staging NV12 — formatos planares só permitem `Map(0)`; UV fica em `pData + RowPitch × Height` | `extract_nv12_planes`: um único `Map(0)`; `CopySubresourceRegion` para Y/UV continua válido |
| Tela preta + offset A/V ~+80 s, milhares de frames *held* | UI não re-adotava `AudioClockHandle` após reset/troca de serviço (`clock_uses_audio` bloqueava upgrade; contador de samples congelado) | `adopted_audio_clock_id` + re-adotar handle quando `audio-out` publica novo id |
| Vídeo em "zig-zag" / frames repetidos | `AddRef` na textura D3D11 **não** impede reuso da **slice** do pool após `AVFrame::unref`; cópia adiada para a UI lia surface já reescrita | Staging copy (`extract_nv12_planes`) **no decoder**, enquanto o `AVFrame` está vivo; canal transporta `NvPlanes` (CPU), não `D3d11Texture` |
| Desync A/V (wall vs áudio) | `video_clock_initialized` servia para wall **e** bloqueava upgrade para `AudioClock` | Flags separadas: `clock_uses_audio` vs `video_clock_initialized` |

**Checklist rápido em regressão GPU:**

1. Log `poll_video_frames: falha no upload` com `Map UV` → rever `d3d11_impl::extract_nv12_planes`.
2. Imagem ok mas batimento cíclico → verificar se cópia NV12 ainda ocorre na thread UI (deve ser no `av-decode`).
3. Áudio ok, vídeo preto/travado após trocar serviço → verificar re-adoption do `AudioClockHandle` em `ui::poll_video_frames`.
4. `Pool frames: 1` no painel Debug A/V é esperado com cópia imediata; não confundir com pool FFmpeg.

Refs: `crates/av/src/hw/d3d11_impl.rs`, `crates/av/src/decoder.rs` (`try_hw_zero_copy`), `crates/ui/src/lib.rs` (`poll_video_frames`), `.specs/features/spec-09-gpu-decode/tdd-sprint-02-gpu-decode.md` §16.

### L-002 — A/V sync: não deslocar âncora do AudioClock (2026-06-24)

Sessão de debug em stream multicast H.264 + AC-3/MPEG (`udp://@239.0.0.1:1234`, serviço 6002). Sintomas e **invariantes que não podem regredir**:

| Sintoma | Causa raiz | Invariante obrigatória |
| ------- | ---------- | ---------------------- |
| Vídeo ~1–2 s adiantado vs áudio audível (AC3 pior que MPEG) | `align_mux_skew_if_needed` + `correct_av_drift` deslocavam `AudioClockHandle::anchor_pts` para frente (`shift_anchor`) quando o front da `VideoQueue` estava à frente do clock. O vídeo ficava alinhado ao clock *deslocado* (~0 ms no painel), mas ~1,4 s à frente do áudio *verdadeiro* (WASAPI). Evidência: `offset_vs_true_audio_ms ≈ shift_total_ms` em logs de sessão abd466 | **Nunca** chamar `shift_anchor` na UI para corrigir skew de mux ou drift A/V. O áudio é o master; o vídeo segue via `VideoQueue::pop_ready` |
| Vídeo congelado no startup se não deslocar âncora | Lead de mux real: áudio decodifica imediatamente; vídeo só após 1º IDR (~1,66 s de diferença PTS). Fila de 16 frames não segurava o burst inicial | `VideoQueue::DEFAULT_CAPACITY = 64` (~2,1 s @ 30 fps). Startup pode exibir ~1,5 s só áudio antes do 1º frame — comportamento correto |
| Decode HW aparentemente mais lento que CPU | Readback GPU→CPU por frame (`extract_nv12_planes`) ~3–6 ms/PES vs ~0,7 ms/PES em SW; não causa degradação progressiva de FPS da UI (cadência estável ~16,7 ms) | Otimização HW é item separado (zero-copy wgpu); não reintroduzir `shift_anchor` como workaround de sync |

**Mecanismo do bug (para revisores de PR):**

```text
now_pts90 = anchor_pts + f(samples_played - latency)   // áudio audível
```

Deslocar `anchor_pts` adianta o relógio sem adiantar o áudio no DAC. A UI comparava `front_pts` do vídeo com esse relógio fictício e exibia frames "no tempo" — mas o ouvinte ouvia conteúdo ~1–2 s atrás.

**Checklist rápido em regressão A/V:**

1. Painel Sync mostra offset ~0 ms mas vídeo claramente adiantado → procurar `shift_anchor` na UI ou equivalente.
2. Vídeo congelado nos primeiros segundos após sintonia → verificar `DEFAULT_CAPACITY` da `VideoQueue` (mín. 64) antes de reintroduzir alinhamento de âncora.
3. Troca de trilha AC3↔MPEG com desync crescente → confirmar re-adoption de `AudioClockHandle` (L-001), não shift de âncora.

Refs: `crates/ui/src/lib.rs` (`poll_video_frames`), `crates/av/src/video_queue.rs`, `crates/av/src/clock.rs`, `.specs/features/spec-08-av-sync/tdd-sprint-01-av-sync.md` §4.1.

### L-003 — Deinterlace bwdif: colorspace do grafo e PTS 2× (2026-06-25)

Sessão de debug em stream multicast H.264 1080i MBAFF (Globo/SKY, `udp://@239.0.0.1:1234`). Sintomas e **invariantes que não podem regredir**:

| Sintoma | Causa raiz | Invariante obrigatória |
| ------- | ---------- | ---------------------- |
| Vídeo congela no 1º frame com deinterlace ativo; log FFmpeg `Changing video frame properties on the fly` (csp/range) | Filtro `buffer` criado sem `colorspace`/`range`; frames decodificados chegam `bt709/tv` | `FfmpegFilterGraph::new_bwdif` declara `colorspace` e `range` no buffer source; chave de recriação do grafo inclui ambos (`deinterlace.rs`) |
| Vídeo congela no 1º frame; métricas `early_frames_held` ↑, `av_sync_offset_ms` ~12,8M; pop `TOO_EARLY_no_resync` | bwdif/yadif divide `time_base` de saída por 2 → PTS bruto 2× maior que 90 kHz; `VideoQueue` vs `AudioClock` nunca alinha | Após `av_buffersink_get_frame`, aplicar `rescale_bwdif_output_pts` (÷2). Teste: `spec_av_005_bwdif_output_pts_halved_to_90khz` |
| Caminho HW ok, SW+deinterlace travado | HW não passa pelo bwdif; só streams que migram para SW (1080i) sofrem | Não remover rescale PTS ao otimizar filtro; validar 1080i interlaced após mudanças em `ffi/mod.rs` ou `deinterlace.rs` |

**Checklist rápido em regressão deinterlace:**

1. Painel: `Deinterlace (bwdif) Ativo`, `Scan type Interlaced`, vídeo **fluindo** (não só 1º frame).
2. `early_frames_held` estável/baixo; `av_sync_offset_ms` na ordem de dezenas de ms, não milhões.
3. Log FFmpeg sem `Changing video frame properties on the fly` repetido por frame.
4. Comparar PTS vídeo ÷ 2 com clock áudio se suspeitar de rescale ausente (`frame_pts` pós-bwdif).

Refs: `crates/av/src/deinterlace.rs`, `crates/av/src/ffi/mod.rs` (`new_bwdif`, `rescale_bwdif_output_pts`, `FfmpegFilterGraph::process`), `crates/av/src/video_queue.rs` (`pop_ready_with_resync`).

### L-004 — HEVC 4:2:2/4:4:4: D3D11VA não decodifica (2026-06-25)

Sessão de debug em stream multicast HEVC Main 4:2:2 10 (Globo REDE HD, `P1171_GLOBO_REDE.ts`). Sintomas e **invariantes que não podem regredir**:

| Sintoma | Causa raiz | Invariante obrigatória |
| ------- | ---------- | ---------------------- |
| Tela preta com áudio ok; vídeo aparece só após 2–5 s | D3D11VA aberto para HEVC; GPU não produz frames para 4:2:2/4:4:4; `hw_init_deadline` (2 s) expira → fallback SW; keyframe inicial consumido na fase HW inútil | Parse leve do SPS HEVC (`detect_hevc_chroma_format`); se `chroma_format_idc ≠ 1` (≠ 4:2:0), abrir decoder SW direto — não tentar D3D11VA |
| Decode/render SW funciona após fallback | `to_yuv_planes` já aceita `YUV422P10LE`; problema é só o caminho HW | Main/Main10 (4:2:0) continuam em D3D11VA quando HW ativo |

**Checklist rápido em regressão HEVC:**

1. Stream 4:2:2 10: vídeo visível em < 1 s (sem `timeout de 2 s sem frame HW` no log).
2. Stream HEVC 4:2:0 (Main10): ainda usa D3D11VA quando disponível.
3. Teste `spec_av_005_hevc_422_sps_chroma_format` verde após mudanças em `scan_type.rs` ou `decoder.rs`.

Refs: `crates/av/src/scan_type.rs` (`detect_hevc_chroma_format`, `hevc_hwaccel_unsupported`), `crates/av/src/decoder.rs` (`skip_hw_for_hevc_chroma`, `reopen_sw_codec_from_hw`).

### L-005 — HEVC: latência de abertura ∝ intervalo de IRAP; status HW por-PID (2026-06-25)

Continuação do debug do Globo REDE HD (HEVC 4:2:2). Após L-004, o vídeo abria mas com latência **muito variável** (2 s a 30 s+) e os cards de status mostravam estado errado. Diagnóstico por instrumentação de runtime (logs NDJSON) + ffprobe:

| Sintoma | Causa raiz (com evidência) | Invariante obrigatória |
| ------- | -------------------------- | ---------------------- |
| Tempo até 1º frame varia 2–30 s conforme o ponto de entrada no multicast | **Inerente ao stream**: o decoder só produz imagem ao receber um IRAP (IDR/CRA). Logs: 1º frame sai ~146 ms **após** o 1º IRAP; tempo-até-IRAP medido em 4 s / 20 s / 3,8 s por join. ffprobe na amostra: keyframes irregulares, lacunas de até ~30 s. `send_err=0` o tempo todo (decoder aceita tudo, só não há IRAP). **Não é bug** — nenhum player abre antes do próximo IRAP | Não "consertar" com timeouts/sleeps; a migração HW→SW deve terminar **antes** do IRAP (mede-se: `hw_decode=false` quando o IRAP chega) para nunca desperdiçar o keyframe |
| Card "Hwaccel" preso em `GPU (hevc_d3d11va)` após migração para SW; `Pool frames` ≠ 0 | `is_hwaccel_active`/`hw_decode_codec`/`hw_frame_pool_in_use` liam o estado **global** `hw_state` (que continua armado), não o `hw_decode` real por PID | Esses 3 métodos devem refletir o estado **por-PID** (`states.values().any(s.is_video && s.hw_decode)`); após migração o card mostra `CPU` + `Pool frames 0` |
| `Scan type` eternamente `Unknown` em HEVC progressivo | `update_scan_type` só resolvia `Progressive` via SPS H.264; HEVC nunca resolvia | Para não-H.264, resolver `Progressive` via `field_order == AV_FIELD_PROGRESSIVE` após o 1º frame (H.264 mantém cautela MBAFF) |

**Checklist rápido em regressão:**

1. HEVC 4:2:2: após migração, card "Hwaccel" = `CPU`, `Pool frames 0`, `Scan type Progressive`.
2. Latência de abertura ≈ tempo até o próximo IRAP do stream (não há piso menor); não introduzir HW deadline para HEVC 4:2:2.
3. Testes `spec_av_005_hevc_progressive_resolves_via_field_order` e os de `is_hwaccel_active`/fallback verdes.

Refs: `crates/av/src/decoder.rs` (`is_hwaccel_active`, `hw_decode_codec`, `hw_frame_pool_in_use`), `crates/av/src/scan_type.rs` (`update_scan_type`), `src/main.rs` (snapshot de `PipelineMetrics`).

### L-006 — UI Slint: conversão CPU de vídeo trava vs egui (zero-copy GPU pendente) (2026-06-26)

POC da UI Slint (spec-11-slint, [D-009](#decisões-arquiteturais)). Em uso ao vivo a 1080p, o vídeo **engasga visivelmente** comparado à UI egui anterior — mesmo em build `--release` e com a conversão fora da thread da UI.

| Sintoma | Causa raiz | Encaminhamento |
| ------- | ---------- | -------------- |
| Travamento/stutter do vídeo na UI Slint, pior que egui | A UI egui fazia **YUV→RGB num shader wgpu (GPU)** — a CPU nunca tocava nos pixels no caminho quente (cf. `crates/av/src/renderer.rs` pipelines NV12/YUV). O POC Slint faz **conversão YUV→RGBA na CPU** por frame + upload de textura RGBA ao femtovg por frame; em 1080p são ~2 Mpx/frame na CPU + ~8 MB de upload/frame | **Próximo passo:** zero-copy GPU via `slint::wgpu_28` — usar o mesmo `wgpu::Device`/`Queue` do Slint (`WGPUConfiguration::Manual` ou `set_rendering_notifier`), fazer YUV→RGB num shader e importar a textura como `slint::Image`. Reaproveitar os shaders de `crates/av/src/renderer.rs` (`nv12_to_rgb.wgsl`, `yuv_to_rgb.wgsl`) |

**Mitigações já aplicadas (não regredir):**

1. Conversão roda em thread worker dedicada (`slint-video-convert`), **nunca** no event loop do Slint — o `Poller` só resolve timing (`VideoQueue`/`MasterClock`) e despacha o frame pronto. Não reintroduzir conversão por pixel na thread da UI.
2. Conversão escreve **direto** nos bytes do `SharedPixelBuffer` (`make_mut_bytes`), sem `Vec` intermediário nem zero-fill redundante.
3. `+crt-static` impede Skia (`skia-bindings`); o renderer é **femtovg**. Ao adotar `slint::wgpu_28`, validar build com CRT estático.

Refs: `crates/ui-slint/src/video.rs` (conversão CPU), `crates/ui-slint/src/lib.rs` (`VideoState`, worker `slint-video-convert`), `crates/av/src/renderer.rs` (pipelines GPU de referência), `.specs/features/spec-11-slint/plan.md`.

---

## Pendências

- [ ] Definir versão exata do FFmpeg a ser distribuída (6.x vs 7.x)
- [ ] Criar script de geração de fixtures sintéticas para tabelas DVB
- [ ] Escolher entre `tokio::sync::watch` vs `arc-swap` para snapshot da UI
- [ ] Definir política de versionamento semântico (SemVer vs CalVer)
- [ ] **Zero-copy GPU para a UI Slint** (`slint::wgpu_28`) — resolver o stutter de vídeo (cf. L-006)
- [ ] Abas Tabelas/Serviços do Slint: detalhar conteúdo (árvore PSI/SI, EIT p/f) além da grade/lista atual
- [ ] Avaliar migrar o modo **Cinema** para Slint (hoje só stub visual no toggle)
