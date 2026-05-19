# TDD: `crates/ts::tables` — Tabelas PSI/SI e DVB

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-TABLE-001 a SPEC-TABLE-008
- **Fase:** Alpha v0.1 (TABLE-001/002) · Beta v0.3 (TABLE-003–008)

---

## Contexto e Problema

Um multiplex MPEG-TS transporta tabelas de sinalização (PSI/SI) que descrevem a estrutura do serviço: quais programas existem (PAT), como estão mapeados (PMT), informações de rede (NIT), nomes de serviços (SDT), guia de programação (EIT), hora do dia (TDT) e agrupamentos de bouquet (BAT).

Parsear essas tabelas corretamente é essencial para exibir informações profissionais ao usuário. Todos os parsers são **funções puras** que recebem `&[u8]` e retornam `Result<T, TableError>` — sem estado, sem side effects, fáceis de testar com fixtures binárias.

---

## Escopo

**In-scope:**
- PAT (table_id `0x00`) — lista de programas e PIDs de PMT
- PMT (table_id `0x02`) — streams A/V por programa, PCR PID, descriptors
- NIT (table_id `0x40`/`0x41`) — informações de rede e entrega física
- SDT (table_id `0x42`/`0x46`) — nomes e status de serviços
- EIT (table_id `0x4E`/`0x4F`/`0x50`–`0x6F`) — guia eletrônico de programas
- TDT (table_id `0x70`) — hora e data UTC
- BAT (table_id `0x4A`) — associação de bouquet
- Descriptor genérico com decode de `KnownDescriptor`
- Decodificação de strings DVB (ISO 8859-x / UTF-8)
- Decodificação de data/hora MJD + BCD

**Out-of-scope:**
- Tabelas proprietárias (Nagra, Irdeto, etc.)
- Controle de versão / cache de tabelas (responsabilidade do `TsDemuxer`)
- CAT, TSDT, DSMCC, AIT

---

## Solução Técnica

### Estrutura de módulos

```
crates/ts/src/tables/
├── mod.rs          # re-exports; TableError; trait SectionParser
├── pat.rs          # SPEC-TABLE-001: Pat, PatProgram
├── pmt.rs          # SPEC-TABLE-002: Pmt, PmtStream, stream_type_label
├── nit.rs          # SPEC-TABLE-003: Nit, NitTransportStream, DeliveryDescriptor
├── sdt.rs          # SPEC-TABLE-004: Sdt, SdtService, RunningStatus
├── eit.rs          # SPEC-TABLE-005: Eit, EitEvent, MJD decode
├── tdt.rs          # SPEC-TABLE-006: Tdt, offset_from_system
├── bat.rs          # SPEC-TABLE-007: Bat, BatTransportStream
├── descriptor.rs   # SPEC-TABLE-008: Descriptor, KnownDescriptor, decode()
└── dvb_string.rs   # Decodificação de strings DVB (ISO 8859-* / UTF-8)
```

### Dependências adicionais

```toml
# Em crates/ts/Cargo.toml
[dependencies]
chrono = { version = "0.4", default-features = false, features = ["std"] }
encoding_rs = "0.8"   # decodificação ISO 8859-x
```

> **Decisão:** `encoding_rs` (bindings da Mozilla) é a forma canônica de decodificar ISO 8859-x em Rust. Não implementar manualmente.  
> **Decisão:** `chrono` apenas para `NaiveDateTime` / `DateTime<Local>` — sem timezone database externa.

---

## Contratos de Interface

### Trait `SectionParser` (padrão interno)

```rust
/// Todos os parsers de tabela implementam este trait.
trait SectionParser: Sized {
    fn parse(section_body: &[u8]) -> Result<Self, TableError>;
}
```

`section_body` é o conteúdo da seção **sem** os 3 bytes de cabeçalho TS-section e **sem** os 4 bytes de CRC (já validado pelo `SectionAssembler`).

---

### PAT (SPEC-TABLE-001)

```rust
pub struct Pat {
    pub transport_stream_id: u16,
    pub version:             u8,
    pub current_next:        bool,
    pub programs:            Vec<PatProgram>,
}

pub struct PatProgram {
    pub program_number: u16,
    pub pid:            Pid,
}
```

**Regras de negócio:**
- `program_number == 0` → NIT PID (pode diferir de 0x0010) (SPEC-TABLE-001b)
- Mudança de `version_number` → re-parsear todas as PMTs (SPEC-TABLE-001d)
- Lista de PMT PIDs alimenta `TsDemuxer::register_pmt_pid` (SPEC-TABLE-001c)

**Layout do corpo da seção PAT:**
```
[transport_stream_id 2B][reserved 2b|version 5b|current_next 1b][section_number 1B][last_section_number 1B]
[program_number 2B | reserved 3b | pid 13b] × N
```

---

### PMT (SPEC-TABLE-002)

```rust
pub struct Pmt {
    pub program_number:      u16,
    pub version:             u8,
    pub pcr_pid:             Pid,
    pub program_descriptors: Vec<Descriptor>,
    pub streams:             Vec<PmtStream>,
}

pub struct PmtStream {
    pub stream_type:       u8,
    pub elementary_pid:    Pid,
    pub descriptors:       Vec<Descriptor>,
    pub stream_type_label: &'static str,
}

impl PmtStream {
    pub fn stream_type_label(st: u8) -> &'static str;
}
```

**Mapeamento `stream_type → label` (SPEC-TABLE-002b):**

| `stream_type` | label                                     |
| ------------- | ----------------------------------------- |
| 0x01          | `"MPEG-1 Video"`                          |
| 0x02          | `"MPEG-2 Video"`                          |
| 0x03          | `"MPEG-1 Audio (MP1)"`                    |
| 0x04          | `"MPEG-2 Audio (MP2)"`                    |
| 0x0F          | `"AAC Audio (ADTS)"`                      |
| 0x11          | `"AAC Audio (LATM)"`                      |
| 0x1B          | `"H.264 / AVC Video"`                     |
| 0x24          | `"H.265 / HEVC Video"`                    |
| 0x81          | `"AC-3 Audio (ATSC)"`                     |
| 0x86          | `"SCTE-35 Splice"`                        |
| 0x06          | `"Private Data"`                          |
| _             | `"Unknown (0xXX)"` (formatado em runtime) |

---

### NIT (SPEC-TABLE-003)

```rust
pub struct Nit {
    pub network_id:           u16,
    pub version:              u8,
    pub actual:               bool,
    pub network_name:         Option<String>,
    pub network_descriptors:  Vec<Descriptor>,
    pub transport_streams:    Vec<NitTransportStream>,
}

pub struct NitTransportStream {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub descriptors:         Vec<Descriptor>,
    pub delivery:            Option<DeliveryDescriptor>,
}

pub enum DeliveryDescriptor {
    Satellite  { frequency_hz: u64, polarization: Polarization, symbol_rate: u32 },
    Cable      { frequency_hz: u64, modulation: u8, symbol_rate: u32 },
    Terrestrial { centre_frequency_hz: u64, bandwidth_hz: u32 },
}

pub enum Polarization { LinearHorizontal, LinearVertical, CircularLeft, CircularRight }
```

---

### SDT (SPEC-TABLE-004)

```rust
pub struct Sdt {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub version:             u8,
    pub actual:              bool,
    pub services:            Vec<SdtService>,
}

pub struct SdtService {
    pub service_id:            u16,
    pub eit_schedule_flag:     bool,
    pub eit_present_following: bool,
    pub running_status:        RunningStatus,
    pub free_ca_mode:          bool,
    pub service_name:          Option<String>,
    pub provider_name:         Option<String>,
    pub service_type:          Option<u8>,
    pub descriptors:           Vec<Descriptor>,
}

#[repr(u8)]
pub enum RunningStatus {
    Undefined = 0, NotRunning = 1, StartsInFewSeconds = 2,
    Pausing = 3, Running = 4, ServiceOffAir = 5,
}
```

---

### EIT (SPEC-TABLE-005)

```rust
pub struct Eit {
    pub service_id:          u16,
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub version:             u8,
    pub table_id:            u8,
    pub events:              Vec<EitEvent>,
}

pub struct EitEvent {
    pub event_id:          u16,
    pub start_time:        Option<chrono::NaiveDateTime>,  // UTC; None se 0xFF
    pub duration_seconds:  Option<u32>,
    pub running_status:    RunningStatus,
    pub free_ca_mode:      bool,
    pub event_name:        Option<String>,
    pub short_description: Option<String>,
    pub descriptors:       Vec<Descriptor>,
}

impl EitEvent {
    /// SPEC-TABLE-005a
    pub fn start_time_local(&self) -> Option<chrono::DateTime<chrono::Local>>;
}
```

**Decodificação MJD + BCD (SPEC-TABLE-005b):**
```
bytes[0..=1]: MJD (Modified Julian Date)
bytes[2..=4]: BCD HH, MM, SS
Se HH=0xFF → start_time = None

Y' = ((MJD - 15078.2) / 365.25) as u32
M' = ((MJD - 14956.1 - (Y' as f64 * 365.25) as u64) / 30.6001) as u32
dia = MJD - 14956 - (Y' * 365.25) as u64 - (M' * 30.6001) as u64
mes = if M' == 14 || M' == 15 { M' - 1 } else { M' }
ano = if M' == 14 || M' == 15 { Y' + 1 } else { Y' } + 1900
```

---

### TDT (SPEC-TABLE-006)

```rust
pub struct Tdt {
    pub utc_time: chrono::NaiveDateTime,
}

impl Tdt {
    pub fn parse(section: &[u8]) -> Result<Self, TableError>;
    pub fn offset_from_system(&self) -> i64;  // segundos; + = TDT adiantado
}
```

---

### Descriptors (SPEC-TABLE-008)

```rust
pub struct Descriptor {
    pub tag:  u8,
    pub data: Bytes,  // raw, sem tag e length
}

pub enum KnownDescriptor {
    NetworkName     { name: String },                        // tag 0x40
    ServiceList     { services: Vec<(u16, u8)> },            // tag 0x41
    BouquetName     { name: String },                        // tag 0x47
    Service         { service_type: u8, provider: String, name: String }, // tag 0x48
    ShortEvent      { lang: [u8; 3], name: String, text: String },        // tag 0x4D
    SatelliteDelivery { frequency_hz: u64, polarization: Polarization, symbol_rate: u32 }, // tag 0x43
    CableDelivery   { frequency_hz: u64, modulation: u8, symbol_rate: u32 }, // tag 0x44
    TerrestrialDelivery { centre_frequency_hz: u64, bandwidth_hz: u32 },     // tag 0x5A
    Unknown         { tag: u8, data: Bytes },
}

impl Descriptor {
    /// SPEC-TABLE-008b: nunca retorna Err — sempre cai em Unknown
    pub fn decode(&self) -> KnownDescriptor;
}
```

**Decodificação de strings DVB (SPEC-TABLE-008c):**
```
byte[0] == ausente     → ISO 8859-1 implícito
byte[0] in 0x01..=0x0B → ISO 8859-{5..15} (usar encoding_rs)
byte[0..=2] == [0x10, 0x00, XX] → ISO 8859-XX (usar encoding_rs)
byte[0] == 0x15        → UTF-8
Substituir bytes inválidos por U+FFFD (não retornar Err)
```

---

## Estratégia de Testes

### Por tabela (fixtures binárias)

```
spec_table_001a_pat_parse_basic           (fixture: pat_section.bin)
spec_table_001b_pat_nit_pid_zero
spec_table_002a_pmt_h264_aac             (fixture: pmt_h264_aac.bin)
spec_table_002b_stream_type_labels
spec_table_003_nit_cable                 (fixture: nit_cable.bin)
spec_table_004_sdt_actual                (fixture: sdt_actual.bin)
spec_table_005_eit_pf                    (fixture: eit_pf.bin)
spec_table_005a_start_time_local
spec_table_005b_mjd_bcd_decode
spec_table_006a_tdt_parse                (fixture: tdt.bin)
spec_table_006b_offset_from_system
spec_table_007_bat                       (fixture: bat.bin)
spec_table_008b_descriptor_decode_service
spec_table_008c_dvb_string_iso8859
spec_table_008c_dvb_string_utf8
spec_table_008_unknown_descriptor_fallback
```

### Casos de erro

```
spec_table_insufficient_data
spec_table_wrong_table_id
spec_table_invalid_section_length
```

---

## Considerações de Segurança

- `section_length` máximo: 4093 bytes (já validado pelo `SectionAssembler` antes de chegar aqui).
- Parsing de descriptors usa iteração sobre slice com verificação de bounds — nunca `slice[i]` sem verificar `i < len`.
- Strings DVB de tamanho zero retornam `Some("")` ou `None` conforme o campo — nunca panic.
- `encoding_rs` trata bytes inválidos substituindo por `\u{FFFD}`, não fazendo panic.
