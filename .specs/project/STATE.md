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

_(vazio — preencher durante implementação)_

---

## Pendências

- [ ] Definir versão exata do FFmpeg a ser distribuída (6.x vs 7.x)
- [ ] Criar script de geração de fixtures sintéticas para tabelas DVB
- [ ] Escolher entre `tokio::sync::watch` vs `arc-swap` para snapshot da UI
- [ ] Definir política de versionamento semântico (SemVer vs CalVer)
