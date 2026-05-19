# Spec: `ts` — Demuxer, Parser e PCR

- **Spec-IDs:** SPEC-TS-001 · SPEC-TS-002 · SPEC-TS-003 · SPEC-TS-004
- **Crate:** `crates/ts`
- **Fase:** Alpha v0.1 (TS-001/002/003) · Alpha v0.2 (TS-004)

---

## Requisitos

| ID           | Requisito                                            | Critério de aceite                                  |
| ------------ | ---------------------------------------------------- | --------------------------------------------------- |
| SPEC-TS-001  | `TsPacket::parse` — parse de 188 bytes               | 6 cenários da tabela passam                         |
| SPEC-TS-002  | `TsDemuxer` — roteamento por PID + validação CC      | CC errors detectados em fixture                     |
| SPEC-TS-002a | Tabela de roteamento por PID                         | PIDs conhecidos roteados corretamente               |
| SPEC-TS-002b | Validação de Continuity Counter                      | CcError emitido em sequência inválida               |
| SPEC-TS-002c | Recuperação de sync (byte 0x47)                      | `SyncLost` emitido; processamento continua          |
| SPEC-TS-003  | `SectionAssembler` — montagem de seções multi-pacote | Seção fragmentada em 3 pacotes montada corretamente |
| SPEC-TS-003a | Algoritmo de montagem com pointer_field e PUSI       | Comportamentos de 5 cenários corretos               |
| SPEC-TS-003b | CRC-32 MPEG-2 validado                               | `CrcError` emitido em CRC inválido                  |
| SPEC-TS-004  | `AdaptationField` com PCR 42-bit                     | Campo parseado corretamente                         |
| SPEC-TS-004a | `pcr_to_duration` — conversão 27 MHz → Duration      | Precisão de µs                                      |
| SPEC-TS-004b | `PcrTracker` — jitter e descontinuidade por PID      | Eventos emitidos nos thresholds corretos            |

---

## Casos de Teste Obrigatórios

### SPEC-TS-001

| Cenário                    | Resultado                                          |
| -------------------------- | -------------------------------------------------- |
| Byte 0 != 0x47             | `Err(TsError::InvalidSyncByte)`                    |
| Slice com 187 bytes        | `Err(TsError::InvalidPacketSize)`                  |
| Null packet (PID 0x1FFF)   | `Ok`, `pid == 0x1FFF`                              |
| TEI bit setado             | `tei == true`                                      |
| `AFC=10` (adaptation only) | `payload == None`, `adaptation_field == Some(...)` |
| `AFC=01` (payload only)    | `adaptation_field == None`, `payload == Some(...)` |

### SPEC-TS-003

| Cenário                                     | Comportamento                   |
| ------------------------------------------- | ------------------------------- |
| Seção em pacote único (PUSI=true, completa) | Emitida imediatamente           |
| Seção fragmentada em 3 pacotes              | Emitida só após o 3º pacote     |
| PUSI=true com buffer pendente               | Descarta anterior, inicia nova  |
| CRC inválido                                | Descarta + emite `CrcError`     |
| `section_length` > 4093                     | `Err(TsError::SectionTooLarge)` |

---

## Fixtures de Teste Necessárias

| Arquivo                                    | Conteúdo                                  |
| ------------------------------------------ | ----------------------------------------- |
| `tests/fixtures/ts_packets_cc_error.bin`   | Stream sintético com CC error no pacote 5 |
| `tests/fixtures/ts_fragmented_section.bin` | Seção PAT fragmentada em 3 pacotes TS     |
| `tests/fixtures/ts_rtp_wrapped.bin`        | Pacotes TS encapsulados em RTP            |
