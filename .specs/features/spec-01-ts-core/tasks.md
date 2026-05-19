# Tasks: `crates/ts` — Core (Demuxer, Parser, PCR)

> Gate: `cargo test -p ts` verde + `cargo clippy -p ts -- -D warnings`  
> Fixtures necessárias: `ts_packets_cc_error.bin`, `ts_fragmented_section.bin`

---

## T01 — Scaffold do crate `ts`

**O quê:** `crates/ts/Cargo.toml`, `src/lib.rs`, `src/error.rs` com `TsError`, `TsEvent`, `PcrEvent`.

**Done when:** `cargo check -p ts` passa; tipos de erro compilam.

---

## T02 — CRC-32 MPEG-2 (`crc.rs`) (SPEC-TS-003b)

**O quê:** Tabela CRC-32 polinômio `0x04C11DB7` pré-computada; funções `crc32_mpeg2` e `verify_crc32_mpeg2`.

**Depende de:** T01

**Done when:**
- `verify_crc32_mpeg2` retorna `true` para seção PAT real da fixture
- Teste de vetor conhecido: CRC de `[0x00, 0xB0, 0x0D, ...]` bate com valor calculado externamente

**Testes:** `spec_ts_crc32_known_vector`

---

## T03 — `TsPacket::parse` (SPEC-TS-001)

**O quê:** `src/packet.rs` + `src/adaptation.rs` com parse completo de 188 bytes.

**Depende de:** T01

**Done when:** Todos os 6 cenários `spec_ts_001_*` passam.

**Testes:**
```
cargo test -p ts spec_ts_001
```

---

## T04 — `pcr_to_duration` e decode PCR (SPEC-TS-004a)

**O quê:** Decodificação dos 6 bytes de PCR em `u64`; conversão para `std::time::Duration`.

**Depende de:** T03

**Done when:**
- Valor PCR de 27 MHz convertido corretamente para µs
- Teste com valor PCR conhecido (ex: `0x00_0000_0000` → `Duration::ZERO`)

**Testes:** `spec_ts_004_pcr_to_duration_precision`

---

## T05 — `TsDemuxer` — roteamento e CC (SPEC-TS-002)

**O quê:** `src/demux.rs` com roteamento por PID, validação CC, recuperação de sync.

**Depende de:** T03

**Done when:**
- CC errors detectados e emitidos via `event_tx`
- Null packets descartados e contabilizados
- `SyncLost` emitido quando `raw[0] != 0x47`
- `register_pmt_pid` e `register_av_pid` funcionam dinamicamente

**Testes:**
```
cargo test -p ts spec_ts_002
```

---

## T06 — `SectionAssembler` (SPEC-TS-003)

**O quê:** `src/section.rs` com montagem de seções fragmentadas e validação CRC.

**Depende de:** T02, T05

**Done when:** Todos os 5 cenários `spec_ts_003_*` passam, incluindo fixture de 3 pacotes.

**Testes:**
```
cargo test -p ts spec_ts_003
```

---

## T07 — `PcrTracker` (SPEC-TS-004b)

**O quê:** `src/pcr.rs` com rastreamento de jitter e descontinuidade por PID.

**Depende de:** T04

**Done when:**
- Jitter > 500 µs emite `PcrEvent::Jitter`
- Salto > 100 ms emite `PcrEvent::Discontinuity { reason: LargeJump }`
- Flag `discontinuity_indicator` emite `PcrEvent::Discontinuity { reason: Flag }`

**Testes:**
```
cargo test -p ts spec_ts_004b
```

---

## T08 — Fixtures de teste sintéticas

**O quê:** Script Rust (`tests/gen_fixtures.rs`) que gera:
- `ts_packets_cc_error.bin`: 10 pacotes TS válidos com CC error no pacote 5
- `ts_fragmented_section.bin`: seção PAT dividida em 3 pacotes TS
- `ts_rtp_wrapped.bin`: 5 pacotes TS com header RTP PT=33

**Depende de:** T03

**Done when:** Fixtures geradas e commitadas em `tests/fixtures/`; comentário de origem no topo do script.

---

## T09 — Property-based tests (`proptest`)

**O quê:** Testes de fuzzing para `TsPacket::parse` e `SectionAssembler`.

**Depende de:** T03, T06

**Done when:**
- `proptest` confirma que nenhum slice de 188 bytes causa panic em `TsPacket::parse`
- CRC-32 de seção gerada programaticamente sempre valida

**Testes:** `cargo test -p ts proptest`

---

## T10 — Clippy e cobertura mínima

**Depende de:** T01–T09

**Done when:**
- `cargo clippy -p ts -- -D warnings` passa
- Todos os `spec_ts_*` testes passam em `cargo test -p ts`
