# Sprint 1 — Sincronização A/V (spec-08-av-sync)

## Overview

Resolver o engasgo periódico de vídeo no IronPlayer introduzindo um **master clock unificado** (`AudioClock` como padrão), uma **`VideoQueue` ordenada por PTS** com políticas de drop/hold/resync, e tratamento de descontinuidade PCR / wrap de PTS de 33 bits. Paralelamente, implementar três lacunas de PSI/SI (CAT, NIT em PID dinâmico, TOT) e versionar fixtures reais para cobertura de regressão.

Referência: [tdd-sprint-01-av-sync.md](tdd-sprint-01-av-sync.md)

## Tasks

- [x] Task 1: Versionar 10 fixtures reais (15–60 s cada) em `crates/ts/tests/fixtures/real/` e adicionar snapshot tests de PAT/PMT/SDT/NIT contra elas
- [x] Task 2: Implementar CAT (PID 0x0001), NIT em PID dinâmico e TOT (PID 0x0014 / table_id 0x73) em `crates/ts` com dispatch em `src/table_dispatcher.rs`
- [x] Task 3: Implementar `av::clock` (`MasterClock`, `AudioClockHandle`, `WallClockHandle`) com exposição do contador atômico de samples via callback cpal — Fases A e B do TDD
- [x] Task 4: Implementar `av::video_queue` (`VideoQueue` ordenada por PTS, políticas drop-late / hold-early / resync / wrap 33-bit) e substituir o pipeline best-effort existente em `crates/ui` e `src/channels.rs` — Fases C e D do TDD
- [x] Task 5: Estender `MetricsSnapshot` com campos de sync (`av_sync_offset_ms`, `late_frames_dropped`, `early_frames_held`, `pts_discontinuities`, `video_queue_depth`) e adicionar painel "Sync A/V" na UI com gráfico de 60 s — Fase E do TDD
- [x] Task 6: Validação end-to-end: rodar fixtures E2E (drift < 40 ms / 5 min, descontinuidade, wrap simulado, stream FTA + scrambled) e adicionar fuzz targets `cargo-fuzz` para `Cat::parse` e `Tot::parse`

## Technical Details

- **Linguagem / MSRV**: Rust 1.78 stable
- **Crates afetadas**: `crates/ts`, `crates/av`, `crates/ui`, `src/`
- **Unidade interna de PTS**: `i64` em 90 kHz (`Pts90`), compatível com FFmpeg; wrap detectado quando `Δpts > 2^32`
- **Master clock default**: `AudioClock` (samples_played / sample_rate + âncora PTS); fallback `WallClock` quando sem áudio
- **Limiares iniciais**: HOLD 20 ms · DROP 100 ms · RESYNC 500 ms · `VideoQueue::capacity` 16 frames
- **Implementação atômica**: sem feature flag; pipeline legado removido integralmente no merge da Task 4
- **Gate de cada task**: `cargo test -p <crate>` verde + `cargo clippy -p <crate> -- -D warnings` sem avisos + zero `unwrap`/`expect` em paths externos
- **SPEC-IDs novos**: `SPEC-AV-CLOCK-*`, `SPEC-AV-VQ-*`, `SPEC-AV-SYNC-*`, `SPEC-TS-CAT-*`, `SPEC-TS-NIT-DYN-*`, `SPEC-TABLE-TOT-*`, `SPEC-METRICS-SYNC-*`
