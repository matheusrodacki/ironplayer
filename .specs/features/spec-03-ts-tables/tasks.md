# Tasks: `crates/ts::tables`

> Gate: `cargo test -p ts -- tables` verde  
> Fase: Alpha v0.1 (T01–T04) · Beta v0.3 (T05–T12)

---

## T01 — `TableError` e trait `SectionParser`

**O quê:** `crates/ts/src/tables/mod.rs` com `TableError` e trait `SectionParser`.

**Depende de:** ts-core/T01

**Done when:** Tipos compilam; `TableError` implementa `thiserror::Error`.

---

## T02 — Strings DVB (`dvb_string.rs`) (SPEC-TABLE-008c)

**O quê:** Decodificação de strings DVB usando `encoding_rs`. Primeiro byte determina encoding.

**Depende de:** T01

**Done when:**
- ISO 8859-1 implícito (sem byte de seleção)
- ISO 8859-5 a 15 (byte 0x01–0x0B)
- UTF-8 (byte 0x15)
- Bytes inválidos substituídos por `\u{FFFD}`

**Testes:** `spec_table_008c_dvb_string_*`

---

## T03 — `Descriptor` e `KnownDescriptor` (SPEC-TABLE-008)

**O quê:** `src/tables/descriptor.rs` — parse genérico de descriptors + `decode()` para tipos conhecidos.

**Depende de:** T02

**Done when:**
- `Descriptor::decode()` nunca retorna `Err`; desconhecido → `Unknown`
- `Service` descriptor (tag 0x48) decodificado com `service_name` e `provider_name`
- `ShortEvent` descriptor (tag 0x4D) decodificado com strings DVB
- `NetworkName` (0x40), `BouquetName` (0x47), `ServiceList` (0x41) decodificados

**Testes:** `spec_table_008b_*`, `spec_table_008_unknown_descriptor_fallback`

---

## T04 — PAT e PMT (SPEC-TABLE-001/002)

**O quê:** `src/tables/pat.rs` + `src/tables/pmt.rs`.

**Depende de:** T03

**Done when:**
- Fixture `pat_section.bin` parseada corretamente
- Fixture `pmt_h264_aac.bin` com streams H.264 + AAC parseados
- `stream_type_label` correto para todos os 10 tipos mapeados
- `program_number == 0` identificado como NIT PID

**Testes:**
```
cargo test -p ts spec_table_001
cargo test -p ts spec_table_002
```

---

## T05 — NIT (SPEC-TABLE-003)

**O quê:** `src/tables/nit.rs` com delivery descriptors (Cable, Satellite, Terrestrial).

**Depende de:** T03

**Done when:**
- Fixture `nit_cable.bin` parseada: `network_name` e `CableDelivery` corretos
- `frequency_hz`, `modulation`, `symbol_rate` decodificados de BCD

**Testes:** `cargo test -p ts spec_table_003`

---

## T06 — SDT (SPEC-TABLE-004)

**O quê:** `src/tables/sdt.rs` com `RunningStatus`, `service_name`, `provider_name`.

**Depende de:** T03

**Done when:** Fixture `sdt_actual.bin` parseada; `service_name` em ISO 8859-1 decodificado corretamente.

**Testes:** `cargo test -p ts spec_table_004`

---

## T07 — TDT (SPEC-TABLE-006)

**O quê:** `src/tables/tdt.rs` com decodificação MJD+BCD e `offset_from_system`.

**Depende de:** T01

**Done when:**
- Data MJD `0x5E6F` (fixture) decodificada para data correta
- `offset_from_system` retorna diferença em segundos

**Testes:** `cargo test -p ts spec_table_006`

---

## T08 — EIT (SPEC-TABLE-005)

**O quê:** `src/tables/eit.rs` com decodificação MJD+BCD e conversão para horário local.

**Depende de:** T06, T03

**Done when:**
- Fixture `eit_pf.bin` parseada com `event_name` e `start_time` corretos
- `HH=0xFF` → `start_time = None`
- `start_time_local()` converte UTC → local

**Testes:** `cargo test -p ts spec_table_005`

---

## T09 — BAT (SPEC-TABLE-007)

**O quê:** `src/tables/bat.rs`.

**Depende de:** T03

**Done when:** Fixture `bat.bin` parseada; `bouquet_name` decodificado.

**Testes:** `cargo test -p ts spec_table_007`

---

## T10 — Fixtures sintéticas de tabelas

**O quê:** Adicionar ao script de fixtures (`ts-core/T08`): seções TDT e BAT sintéticas com valores conhecidos.

**Depende de:** ts-core/T08

**Done when:** `tests/fixtures/tdt.bin` e `tests/fixtures/bat.bin` presentes com comentário de geração.

---

## T11 — Integração: PAT → PMT → `register_pmt_pid`

**O quê:** Teste de integração que simula o fluxo: seção PAT parseada → `register_pmt_pid` chamado no `TsDemuxer` → seção PMT roteada corretamente.

**Depende de:** T04, ts-core/T05

**Done when:** Teste de integração em `tests/integration/tables_flow.rs` passa.

---

## T12 — Clippy e revisão de bounds checking

**Depende de:** T01–T11

**Done when:**
- `cargo clippy -p ts -- -D warnings` passa
- Nenhum indexamento sem verificação de bounds em `tables/`
