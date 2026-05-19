# IronPlayer — spec-01-ts-core

## Overview

Implementar o crate `crates/ts`: o núcleo de parsing MPEG-TS do IronPlayer. Cobre parse de pacotes de 188 bytes, demultiplexagem por PID com validação de Continuity Counter, remontagem de seções PSI/SI fragmentadas com CRC-32 MPEG-2, e rastreamento de PCR/jitter. Rust puro, sem FFI. Specs: SPEC-TS-001 · SPEC-TS-002 · SPEC-TS-003 · SPEC-TS-004.

Referências: [spec.md](.specs/features/spec-01-ts-core/spec.md) · [design.md](.specs/features/spec-01-ts-core/design.md) · [tasks.md](.specs/features/spec-01-ts-core/tasks.md)

## Tasks

- [x] Task 1: Scaffold do crate `ts` — criar `crates/ts/Cargo.toml`, `src/lib.rs` e `src/error.rs` com os tipos `TsError`, `TsEvent` e `PcrEvent` (T01). Gate: `cargo check -p ts`.
- [x] Task 2: Implementar CRC-32 MPEG-2 (`src/crc.rs`) com tabela pré-computada (polinômio `0x04C11DB7`) e `TsPacket::parse` em `src/packet.rs` + `src/adaptation.rs` incluindo decode do campo PCR de 42 bits e `pcr_to_duration` (T02, T03, T04).
- [x] Task 3: Implementar `TsDemuxer` em `src/demux.rs`: roteamento por PID, validação de Continuity Counter, recuperação de sync e emissão de eventos via canal bounded (T05). Gate: `cargo test -p ts spec_ts_002`.
- [x] Task 4: Implementar `SectionAssembler` em `src/section.rs`: montagem de seções fragmentadas em múltiplos pacotes, pointer_field/PUSI, validação CRC-32 (T06). Gate: `cargo test -p ts spec_ts_003`.
- [x] Task 5: Implementar `PcrTracker` em `src/pcr.rs`: rastreamento de jitter e descontinuidade por PID com emissão de `PcrEvent` nos thresholds definidos na spec (T07). Gate: `cargo test -p ts spec_ts_004b`.
- [x] Task 6: Gerar fixtures de teste sintéticas (`tests/fixtures/`): `ts_packets_cc_error.bin`, `ts_fragmented_section.bin` e `ts_rtp_wrapped.bin` via script `tests/gen_fixtures.rs` (T08). Gate: `cargo test -p ts` 100% verde + `cargo clippy -p ts -- -D warnings` sem avisos.

## Technical Details

- **Crate:** `crates/ts` — zero FFI, zero `unsafe`, Rust 1.78 stable
- **Dependências:** `bytes`, `byteorder`, `thiserror`, `tracing`, `crossbeam-channel` (todas do workspace)
- **Dev-deps:** `rstest`, `proptest`, `pretty_assertions`
- **Regra de canal:** `crossbeam-channel` bounded; nunca `unbounded()`
- **Sem tokio:** o demuxer roda em thread dedicada com loop síncrono
- **Convenção de testes:** `spec_{spec_id_lowercase}_{descrição}` — ex: `spec_ts_001_invalid_sync_byte`
- **Gate final:** `cargo test -p ts` verde + `cargo clippy -p ts -- -D warnings` sem avisos
