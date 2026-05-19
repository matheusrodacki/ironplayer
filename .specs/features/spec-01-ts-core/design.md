# TDD: `crates/ts` — Demuxer e Parser MPEG-TS

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-TS-001, SPEC-TS-002, SPEC-TS-003, SPEC-TS-004
- **Fase:** Alpha v0.1 (TS-001/002/003) · Alpha v0.2 (TS-004)

---

## Contexto e Problema

O coração do IronPlayer é o parsing MPEG-TS. Todo o stream de bytes do socket precisa ser desmultiplexado em pacotes individuais de 188 bytes, roteados por PID, e as seções PSI/SI precisam ser remontadas a partir de fragmentos. Este crate é **puro Rust sem FFI**, garantindo segurança e testabilidade máxima.

O `ts` crate é o mais crítico do projeto: falhas aqui propagam-se para `av`, `ui` e `ts-tables`. Por isso, a cobertura de testes deve ser a mais alta de todos os crates.

---

## Escopo

**In-scope:**
- Parse de pacotes TS de 188 bytes (`TsPacket`)
- Demultiplexagem por PID com roteamento para canais separados (`TsDemuxer`)
- Validação de Continuity Counter por PID
- Recuperação de sync após perda de byte 0x47
- Remontagem de seções PSI/SI fragmentadas em múltiplos pacotes (`SectionAssembler`)
- Validação de CRC-32 MPEG-2 (polinômio `0x04C11DB7`)
- Parse de Adaptation Field incluindo campo PCR de 42 bits
- Rastreamento de jitter PCR por PID (`PcrTracker`)

**Out-of-scope:**
- Parse das tabelas PSI/SI (responsabilidade de `ts::tables`)
- Métricas de bitrate (responsabilidade de `ts::metrics`)
- Montagem de PES packets (responsabilidade de `av::pes`)

---

## Solução Técnica

### Estrutura do crate

```
crates/ts/
├── Cargo.toml
└── src/
    ├── lib.rs           # re-exports públicos; type alias Pid = u16
    ├── packet.rs        # SPEC-TS-001: TsPacket, TsHeader, parse()
    ├── adaptation.rs    # SPEC-TS-004: AdaptationField, pcr_to_duration
    ├── demux.rs         # SPEC-TS-002: TsDemuxer, PidRouter
    ├── section.rs       # SPEC-TS-003: SectionAssembler, SectionBuffer
    ├── pcr.rs           # SPEC-TS-004b: PcrTracker, PcrState
    ├── crc.rs           # CRC-32 MPEG-2
    └── error.rs         # TsError, TsEvent, PcrEvent
```

### Dependências (`Cargo.toml`)

```toml
[dependencies]
bytes     = { workspace = true }
byteorder = { workspace = true }
thiserror = { workspace = true }
tracing   = { workspace = true }
crossbeam-channel = { workspace = true }

[dev-dependencies]
rstest            = "0.23"
proptest          = "1"
pretty_assertions = "1"
```

> **Decisão:** sem `tokio` neste crate — o demuxer roda em thread dedicada com loop síncrono. `bytes::Bytes` evita cópias desnecessárias no roteamento.

---

## Contratos de Interface

### `TsPacket` e `TsPacket::parse` (SPEC-TS-001)

```rust
pub type Pid = u16;  // sempre 13 bits; 0x0000–0x1FFF

pub struct TsPacket {
    pub pid:                Pid,
    pub tei:                bool,
    pub pusi:               bool,
    pub priority:           bool,
    pub scrambling:         u8,           // 2 bits
    pub adaptation_field:   Option<AdaptationField>,
    pub payload:            Option<Bytes>, // até 184 bytes
    pub continuity_counter: u8,           // 4 bits
}

impl TsPacket {
    /// SPEC-TS-001a — parse de exatamente 188 bytes.
    pub fn parse(raw: &[u8; 188]) -> Result<Self, TsError>;
}
```

**Layout dos 4 bytes de header TS:**
```
Byte 0: sync byte (0x47)
Byte 1: [TEI][PUSI][priority][PID high 5 bits]
Byte 2: [PID low 8 bits]
Byte 3: [scrambling 2b][AFC 2b][CC 4b]
```

AFC (Adaptation Field Control):
- `0b00` → Reservado (inválido)
- `0b01` → Payload only
- `0b10` → Adaptation field only
- `0b11` → Adaptation field + payload

### `AdaptationField` (SPEC-TS-004)

```rust
pub struct AdaptationField {
    pub discontinuity_indicator:  bool,
    pub random_access_indicator:  bool,
    pub pcr:                      Option<u64>,  // 42-bit value: base*300 + ext
    pub opcr:                     Option<u64>,
    pub splice_countdown:         Option<i8>,
}

/// SPEC-TS-004a: pcr_value / 27_000_000.0 → Duration
pub fn pcr_to_duration(pcr: u64) -> std::time::Duration;
```

**Decodificação do PCR (6 bytes):**
```
pcr_base = bytes[0]<<25 | bytes[1]<<17 | bytes[2]<<9 | bytes[3]<<1 | bytes[4]>>7  (33 bits)
pcr_ext  = (bytes[4] & 0x01) << 8 | bytes[5]  (9 bits)
pcr_value = pcr_base * 300 + pcr_ext
```

### `TsDemuxer` (SPEC-TS-002)

```rust
pub struct TsDemuxer {
    section_tx: crossbeam_channel::Sender<(Pid, SectionData)>,
    pes_tx:     crossbeam_channel::Sender<PesData>,
    event_tx:   crossbeam_channel::Sender<TsEvent>,
    cc_state:   HashMap<Pid, u8>,
    pmt_pids:   HashSet<Pid>,    // dinâmico: populado ao parsear PAT
    av_pids:    HashSet<Pid>,    // dinâmico: populado ao parsear PMT
}

pub struct SectionData {
    pub pid:     Pid,
    pub pusi:    bool,
    pub payload: Bytes,
}

pub struct PesData {
    pub pid:  Pid,
    pub data: Bytes,
}

impl TsDemuxer {
    pub fn new(
        section_tx: Sender<(Pid, SectionData)>,
        pes_tx:     Sender<PesData>,
        event_tx:   Sender<TsEvent>,
    ) -> Self;

    /// Processa um chunk de bytes (múltiplos de 188).
    /// SPEC-TS-002c: se raw[0] != 0x47, busca próximo sync byte.
    pub fn process_chunk(&mut self, raw: &[u8]);

    /// Registra um PID como PMT (chamado quando PAT é parseada).
    pub fn register_pmt_pid(&mut self, pid: Pid);

    /// Registra um PID como A/V (chamado quando PMT é parseada).
    pub fn register_av_pid(&mut self, pid: Pid);
}
```

**Roteamento de PID (SPEC-TS-002a):**

| PID range / valor  | Destino                    |
| ------------------ | -------------------------- |
| 0x0000 (PAT)       | `section_tx`               |
| 0x0010 (NIT)       | `section_tx`               |
| 0x0011 (SDT/BAT)   | `section_tx`               |
| 0x0012 (EIT)       | `section_tx`               |
| 0x0014 (TDT/TOT)   | `section_tx`               |
| PIDs em `pmt_pids` | `section_tx`               |
| PIDs em `av_pids`  | `pes_tx`                   |
| 0x1FFF (Null)      | descartado + contabilizado |
| qualquer outro     | `section_tx` (tentativa)   |

**Validação de CC (SPEC-TS-002b):**
```
Ignorar CC se: pid == 0x1FFF OU adaptation_only OU scrambling != 0
cc_esperado = (cc_anterior + 1) & 0x0F
Se cc_recebido != cc_esperado → TsEvent::CcError { pid, expected: cc_esperado, got: cc_recebido }
```

### `SectionAssembler` (SPEC-TS-003)

```rust
pub struct SectionAssembler {
    buffers: HashMap<Pid, SectionBuffer>,
    tx:      crossbeam_channel::Sender<CompleteSection>,
}

pub struct CompleteSection {
    pub pid:      Pid,
    pub table_id: u8,
    pub data:     Bytes,  // seção completa sem CRC
}

impl SectionAssembler {
    pub fn push(&mut self, data: SectionData) -> Result<(), TsError>;
}
```

**Estados do `SectionBuffer`:**
```
Vazio → PUSI recebido:
  1. Ler pointer_field (1 byte), avançar payload[pointer_field..]
  2. table_id = payload[0]
  3. section_length = (payload[1] & 0x0F) << 8 | payload[2]  (max 4093)
  4. Se section_length > 4093 → Err(TsError::SectionTooLarge)
  5. Iniciar buffer, copiar bytes restantes

Preenchendo → payload sem PUSI:
  1. Append ao buffer
  2. Se bytes acumulados >= section_length + 3 → seção completa

Seção completa → validar CRC-32:
  CRC nos últimos 4 bytes. Se inválido → TsEvent::CrcError, descartar.
  Se válido → emitir CompleteSection (sem os 4 bytes de CRC)
```

### `PcrTracker` (SPEC-TS-004b)

```rust
pub struct PcrTracker {
    state:    HashMap<Pid, PcrState>,
    event_tx: crossbeam_channel::Sender<PcrEvent>,
}

struct PcrState {
    last_pcr:    u64,
    last_time:   std::time::Instant,
    last_delta_pcr: Option<u64>,  // para calcular jitter
}

pub enum PcrEvent {
    Jitter        { pid: Pid, expected_us: i64, measured_us: i64 },
    Discontinuity { pid: Pid, reason: DiscontinuityReason },
}

pub enum DiscontinuityReason {
    Flag,
    LargeJump { delta_ms: u64 },
}
```

**Lógica de jitter (SPEC-TS-004b):**
```
Dado pcr_atual e pcr_anterior do mesmo PID:
  delta_pcr_ticks = pcr_atual - pcr_anterior
  delta_pcr_us = delta_pcr_ticks / 27.0   (27 MHz → µs)
  delta_real_us = (Instant::now() - last_time).as_micros()
  jitter_us = |delta_real_us - delta_pcr_us|
  
  Se jitter_us > 500 → PcrEvent::Jitter
  Se discontinuity_indicator OU delta_pcr_us > 100_000 → PcrEvent::Discontinuity
```

---

## CRC-32 MPEG-2 (`crc.rs`)

```rust
/// Polinômio MPEG-2: 0x04C11DB7 (big-endian / MSB-first)
/// Tabela pré-computada de 256 entradas; verificação com crc32_mpeg2(data) == 0.
pub fn crc32_mpeg2(data: &[u8]) -> u32;
pub fn verify_crc32_mpeg2(data_with_crc: &[u8]) -> bool;
```

> **Nota:** este CRC difere do CRC-32 padrão (Ethernet) em orientação de bits — usar tabela própria, não `crc32fast`.

---

## Estratégia de Testes

### Unitários por módulo

```
spec_ts_001_invalid_sync_byte
spec_ts_001_invalid_packet_size
spec_ts_001_null_packet
spec_ts_001_tei_bit
spec_ts_001_adaptation_only
spec_ts_001_payload_only

spec_ts_002_cc_error_detection        (fixture: ts_packets_cc_error.bin)
spec_ts_002_cc_null_packet_ignored
spec_ts_002_cc_adaptation_only_no_increment
spec_ts_002_sync_recovery             (fixture: buffer com 0x47 fora do offset)

spec_ts_003_single_packet_section
spec_ts_003_fragmented_3_packets      (fixture: ts_fragmented_section.bin)
spec_ts_003_pusi_discards_pending
spec_ts_003_crc_invalid_discards
spec_ts_003_section_too_large

spec_ts_004_pcr_decode_known_value
spec_ts_004_pcr_to_duration_precision
spec_ts_004b_pcr_jitter_threshold
spec_ts_004b_pcr_discontinuity_flag
spec_ts_004b_pcr_large_jump
```

### Property-based (`proptest`)

```rust
// Qualquer slice de 188 bytes com byte 0 != 0x47 retorna InvalidSyncByte
// Qualquer slice de 188 bytes válidos nunca faz panic
// CRC-32 de seção gerada sinteticamente sempre é válido
```

---

## Considerações de Segurança

- Todos os parsers recebem `&[u8]` de tamanho fixo — verificar `len()` antes de indexar.
- `section_length` máximo de 4093 bytes verificado antes de alocar buffer.
- Nenhum dado externo é usado para controle de fluxo além de validação de tamanho.
- Sem `unsafe` neste crate.

---

## Riscos e Mitigações

| Risco                                               | Mitigação                                              |
| --------------------------------------------------- | ------------------------------------------------------ |
| Streams reais podem ter AFC=0b00 (reservado)        | Tratar como erro recuperável; logar e descartar pacote |
| Seções muito grandes (>4093) em streams corrompidos | Verificar antes de alocar; `TsError::SectionTooLarge`  |
| PCR wrap-around em 2^42 ticks (~26,5 horas)         | Usar subtração com wrapping; não subtrair diretamente  |
| Streams com múltiplos PIDs de PCR                   | `PcrTracker` mantém estado por PID via `HashMap`       |
