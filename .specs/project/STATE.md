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

---

## Pendências

- [ ] Definir versão exata do FFmpeg a ser distribuída (6.x vs 7.x)
- [ ] Criar script de geração de fixtures sintéticas para tabelas DVB
- [ ] Escolher entre `tokio::sync::watch` vs `arc-swap` para snapshot da UI
- [ ] Definir política de versionamento semântico (SemVer vs CalVer)
