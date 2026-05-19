# Spec: `ts::tables` — Tabelas PSI/SI e DVB

- **Spec-IDs:** SPEC-TABLE-001 a SPEC-TABLE-008
- **Módulo:** `crates/ts/src/tables/`
- **Fase:** Alpha v0.1 (TABLE-001/002) · Beta v0.3 (TABLE-003–008)

---

## Requisitos

| ID              | Requisito                              | Critério                                 |
| --------------- | -------------------------------------- | ---------------------------------------- |
| SPEC-TABLE-001  | PAT parseada corretamente              | Programas e PIDs extraídos               |
| SPEC-TABLE-001b | program_number==0 → NIT PID            | NIT PID identificado                     |
| SPEC-TABLE-001c | PIDs de PMT alimentam o demuxer        | `register_pmt_pid` chamado               |
| SPEC-TABLE-001d | Mudança de versão → re-parse de PMTs   | Versão monitorada                        |
| SPEC-TABLE-002  | PMT parseada com todos os stream_types | Streams A/V identificados                |
| SPEC-TABLE-002b | Labels legíveis por stream_type        | 10+ tipos mapeados                       |
| SPEC-TABLE-003  | NIT com network_name e delivery        | Fixture `nit_cable.bin` parseada         |
| SPEC-TABLE-004  | SDT com service_name e running_status  | Fixture `sdt_actual.bin` parseada        |
| SPEC-TABLE-005  | EIT com event_name e start_time        | Fixture `eit_pf.bin` parseada            |
| SPEC-TABLE-005b | Decodificação MJD+BCD                  | `start_time` correto para data conhecida |
| SPEC-TABLE-006  | TDT com offset de relógio              | `offset_from_system` funcional           |
| SPEC-TABLE-007  | BAT com bouquet_name                   | Fixture `bat.bin` parseada               |
| SPEC-TABLE-008  | Descriptor genérico + KnownDescriptor  | Decode nunca retorna Err                 |
| SPEC-TABLE-008c | Strings DVB ISO 8859-x e UTF-8         | Bytes inválidos → U+FFFD                 |

---

## Fixtures de Teste

| Arquivo                           | Origem                          |
| --------------------------------- | ------------------------------- |
| `tests/fixtures/pat_section.bin`  | Capturado de DVB-C (seção real) |
| `tests/fixtures/pmt_h264_aac.bin` | Capturado de DVB-C              |
| `tests/fixtures/nit_cable.bin`    | Capturado de DVB-C              |
| `tests/fixtures/sdt_actual.bin`   | Capturado de DVB-C              |
| `tests/fixtures/eit_pf.bin`       | Capturado de DVB-C              |
| `tests/fixtures/eit_schedule.bin` | Capturado de DVB-C              |
| `tests/fixtures/tdt.bin`          | Gerado sinteticamente           |
| `tests/fixtures/bat.bin`          | Gerado sinteticamente           |
