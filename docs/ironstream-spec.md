# IronStream — Spec Driven Development

> **Versão:** 0.1-draft · **Stack:** Rust · egui/eframe · FFmpeg (A/V decode only) · Windows 10/11 x86-64
> **Convenção:** cada spec tem um `SPEC-ID` único. Testes de unidade/integração referenciam o ID no nome da função. Toda função pública deve ter seu `SPEC-ID` no doc-comment.

---

## Índice

1. [Workspace & Crates](#1-workspace--crates)
2. [Spec: `net` — Recepção de Stream](#2-spec-net--recepção-de-stream)
3. [Spec: `ts` — Demuxer e Parser](#3-spec-ts--demuxer-e-parser)
4. [Spec: `ts::tables` — Tabelas PSI/SI e DVB](#4-spec-tstables--tabelas-psisi-e-dvb)
5. [Spec: `ts::metrics` — Bitrate e Erros](#5-spec-tsmetrics--bitrate-e-erros)
6. [Spec: `av` — Bridge FFmpeg](#6-spec-av--bridge-ffmpeg)
7. [Spec: `ui` — Interface Principal](#7-spec-ui--interface-principal)
8. [Contratos de Canal entre Componentes](#8-contratos-de-canal-entre-componentes)
9. [Configuração e Estado Global](#9-configuração-e-estado-global)
10. [Build, Testes e CI](#10-build-testes-e-ci)
11. [Critérios de Aceite por Fase](#11-critérios-de-aceite-por-fase)

---

## 1. Workspace & Crates

### Estrutura do workspace Cargo

```
ironstream/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── net/                    # SPEC-NET-*   recepção UDP/RTP
│   ├── ts/                     # SPEC-TS-*    demux + parser TS
│   │   ├── src/
│   │   │   ├── demux.rs
│   │   │   ├── section.rs
│   │   │   ├── tables/
│   │   │   │   ├── pat.rs
│   │   │   │   ├── pmt.rs
│   │   │   │   ├── nit.rs
│   │   │   │   ├── sdt.rs
│   │   │   │   ├── eit.rs
│   │   │   │   ├── tdt.rs
│   │   │   │   └── bat.rs
│   │   │   ├── pcr.rs
│   │   │   └── metrics.rs
│   ├── av/                     # SPEC-AV-*    FFmpeg bridge + render
│   └── ui/                     # SPEC-UI-*    egui app
├── src/
│   └── main.rs                 # entry point, wires channels
└── ffmpeg/                     # DLLs FFmpeg pré-compiladas (Windows)
    ├── avcodec-61.dll
    ├── avutil-59.dll
    ├── swresample-5.dll
    └── swscale-8.dll
```

### `Cargo.toml` raiz (workspace)

```toml
[workspace]
members = ["crates/net", "crates/ts", "crates/av", "crates/ui"]
resolver = "2"

[workspace.dependencies]
tokio        = { version = "1", features = ["full"] }
crossbeam-channel = "0.5"
bytes        = "1"
byteorder    = "1"
serde        = { version = "1", features = ["derive"] }
tracing      = "0.1"
anyhow       = "1"
thiserror    = "2"
```

### Regra de dependência entre crates

```
ui  →  ts, av, net      (somente leitura de dados; nunca o inverso)
av  →  ts               (recebe PES packets do demuxer)
ts  →  (zero deps internas; puro Rust + byteorder + bytes)
net →  (zero deps internas; socket2 + tokio)
```

> **Invariante:** `ts` e `net` não podem depender de `ui` nem de `av`.
> Qualquer violação falha o build via `cargo deny` ou `cargo workspace-hack`.

---

## 2. Spec: `net` — Recepção de Stream

### SPEC-NET-001 · Tipo `StreamUrl`

```rust
/// SPEC-NET-001
/// Representa uma URL de stream suportada pelo IronStream.
/// Formatos aceitos:
///   udp://@239.x.x.x:PORT        multicast (join de grupo)
///   udp://239.x.x.x:PORT         idem (@ é opcional)
///   udp://SOURCE@232.x.x.x:PORT  SSM (source-specific multicast)
///   rtp://@239.x.x.x:PORT        RTP sobre UDP multicast
#[derive(Debug, Clone, PartialEq)]
pub enum StreamUrl {
    UdpMulticast { group: Ipv4Addr, port: u16, iface: Option<Ipv4Addr>, source: Option<Ipv4Addr> },
    RtpMulticast  { group: Ipv4Addr, port: u16, iface: Option<Ipv4Addr>, source: Option<Ipv4Addr> },
}

impl StreamUrl {
    /// SPEC-NET-001a
    /// Faz o parse de uma string no formato udp://[<source>@]<ip>:<port>[?iface=<ip>]
    /// Retorna Err se o IP não for multicast (224.0.0.0/4) ou a porta for 0.
    pub fn parse(s: &str) -> Result<Self, NetError>;
}
```

**Casos de teste obrigatórios — `net::tests::spec_net_001`:**

| Entrada                                      | Resultado esperado                                                                                |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `"udp://@239.1.1.1:1234"`                    | `Ok(UdpMulticast { group: 239.1.1.1, port: 1234, iface: None })`                                  |
| `"rtp://@239.0.0.5:5004"`                    | `Ok(RtpMulticast { group: 239.0.0.5, port: 5004, iface: None })`                                  |
| `"udp://10.0.0.1:1234"`                      | `Err(NetError::NotMulticast)`                                                                     |
| `"udp://@239.1.1.1:0"`                       | `Err(NetError::InvalidPort)`                                                                      |
| `"http://example.com"`                       | `Err(NetError::UnsupportedScheme)`                                                                |
| `"udp://@239.1.1.1:1234?iface=192.168.1.10"` | `Ok(UdpMulticast { .., iface: Some(192.168.1.10) })`                                              |
| `"udp://10.218.152.146@232.15.0.93:50000"`   | `Ok(UdpMulticast { group: 232.15.0.93, port: 50000, source: Some(10.218.152.146), iface: None })` |

---

### SPEC-NET-002 · `UdpReceiver`

```rust
/// SPEC-NET-002
/// Recebe pacotes UDP de um grupo multicast e os envia pelo canal `tx`.
/// Cada item enviado é um `Bytes` contendo exatamente um payload UDP bruto
/// (pode conter múltiplos pacotes TS de 188 bytes ou um cabeçalho RTP).
pub struct UdpReceiver {
    url:        StreamUrl,
    tx:         Sender<Bytes>,          // crossbeam_channel::Sender
    buf_size:   usize,                  // padrão: 4 * 1024 * 1024 (4 MB SO_RCVBUF)
    timeout_ms: u64,                    // padrão: 5_000
}

impl UdpReceiver {
    pub fn new(url: StreamUrl, tx: Sender<Bytes>, cfg: ReceiverConfig) -> Self;

    /// SPEC-NET-002a
    /// Bloqueia em loop de recepção. Retorna apenas em caso de erro fatal
    /// ou ao receber sinal de parada via `stop_token`.
    /// Nunca faz panic. Erros recuperáveis (EAGAIN, EINTR) são logados e
    /// a iteração continua. Erros fatais (socket fechado) retornam Err.
    pub fn run(self, stop_token: StopToken) -> Result<(), NetError>;
}
```

**Contrato:**
- `SPEC-NET-002b`: `buf_size` é aplicado via `SO_RCVBUF` no socket antes do join multicast.
- `SPEC-NET-002c`: se nenhum pacote for recebido em `timeout_ms` ms, emite evento `NetEvent::Timeout` — não faz panic nem retorna erro.
- `SPEC-NET-002d`: ao parar, faz leave do grupo multicast (`IP_DROP_MEMBERSHIP`) antes de fechar o socket.

---

### SPEC-NET-003 · `RtpStripper`

```rust
/// SPEC-NET-003
/// Recebe buffers brutos UDP que podem conter cabeçalho RTP.
/// Se detectar cabeçalho RTP válido (version=2, PT=33), remove os primeiros
/// (12 + 4*CC) bytes e passa o payload ao canal de saída.
/// Se o buffer NÃO tiver cabeçalho RTP, passa-o integralmente (modo UDP puro).
///
/// SPEC-NET-003a: detecta pacotes fora de ordem RTP (sequence number) e
/// emite RtpEvent::OutOfOrder { expected, got } sem descartar o pacote.
pub struct RtpStripper {
    rx: Receiver<Bytes>,
    tx: Sender<Bytes>,
    events: Sender<RtpEvent>,
}
```

**Casos de teste — `net::tests::spec_net_003`:**

| Cenário                                     | Comportamento                                  |
| ------------------------------------------- | ---------------------------------------------- |
| Buffer com RTP header válido (PT=33)        | Remove 12 bytes de header; passa payload       |
| Buffer com CSRC count = 2                   | Remove 12 + 8 = 20 bytes                       |
| Buffer sem RTP (sync byte 0x47 no offset 0) | Passa integralmente                            |
| Sequence number 0xFFFF → 0x0001 (wrap)      | Não emite OutOfOrder (wrap legítimo)           |
| Sequence number 100 → 102 (pulo)            | Emite `OutOfOrder { expected: 101, got: 102 }` |

---

### `NetError` e `NetEvent`

```rust
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("endereço não é multicast (224.0.0.0/4): {0}")]
    NotMulticast(Ipv4Addr),
    #[error("porta inválida (0 não permitido)")]
    InvalidPort,
    #[error("esquema de URL não suportado: {0}")]
    UnsupportedScheme(String),
    #[error("falha no socket: {0}")]
    SocketError(#[from] std::io::Error),
    #[error("join multicast falhou: {0}")]
    JoinFailed(std::io::Error),
}

#[derive(Debug, Clone)]
pub enum NetEvent {
    Connected { group: Ipv4Addr, port: u16 },
    Timeout,
    Disconnected,
    UdpBufferOverflow { dropped_bytes: u64 },
}

#[derive(Debug, Clone)]
pub enum RtpEvent {
    OutOfOrder { expected: u16, got: u16 },
    Duplicate  { sequence: u16 },
}
```

---

## 3. Spec: `ts` — Demuxer e Parser

### SPEC-TS-001 · Pacote TS — `TsPacket`

```rust
/// SPEC-TS-001
/// Representa um pacote TS de 188 bytes já validado.
/// Construído apenas via `TsPacket::parse()`; nunca instanciado diretamente.
#[derive(Debug, Clone)]
pub struct TsPacket {
    pub pid:                u16,    // 13 bits; 0x0000–0x1FFF
    pub tei:                bool,   // Transport Error Indicator
    pub pusi:               bool,   // Payload Unit Start Indicator
    pub priority:           bool,
    pub scrambling:         u8,     // 2 bits
    pub adaptation_field:   Option<AdaptationField>,
    pub payload:            Option<Bytes>,  // até 184 bytes; None se apenas adaptation
    pub continuity_counter: u8,     // 4 bits (0–15)
}

impl TsPacket {
    /// SPEC-TS-001a
    /// Faz o parse de exatamente 188 bytes.
    /// Retorna Err se: sync byte != 0x47, slice.len() != 188,
    /// adaptation_field_length inválido.
    pub fn parse(raw: &[u8; 188]) -> Result<Self, TsError>;
}
```

**Casos de teste — `ts::tests::spec_ts_001`:**

| Cenário                  | Resultado                                          |
| ------------------------ | -------------------------------------------------- |
| Byte 0 != 0x47           | `Err(TsError::InvalidSyncByte)`                    |
| Slice com 187 bytes      | `Err(TsError::InvalidPacketSize)`                  |
| Null packet (PID 0x1FFF) | `Ok` com `pid == 0x1FFF`                           |
| TEI bit setado           | `tei == true`                                      |
| adaptation_only (AFC=10) | `payload == None`, `adaptation_field == Some(...)` |
| payload_only (AFC=01)    | `adaptation_field == None`, `payload == Some(...)` |

---

### SPEC-TS-002 · `TsDemuxer`

```rust
/// SPEC-TS-002
/// Recebe buffers brutos de bytes (múltiplos de 188 bytes em cada chunk).
/// Para cada pacote:
///   1. Valida o sync byte e faz parse via TsPacket::parse().
///   2. Atualiza o estado de CC por PID.
///   3. Roteia o payload para: SectionAssembler, PcrTracker ou PesAssembler.
///   4. Emite TsEvents para o canal de métricas.
pub struct TsDemuxer {
    section_tx: Sender<(Pid, SectionData)>,
    pes_tx:     Sender<PesPacket>,
    event_tx:   Sender<TsEvent>,
    cc_state:   HashMap<Pid, u8>,
}
```

**SPEC-TS-002a — Roteamento por PID:**

| PID                                       | Destino                                                     |
| ----------------------------------------- | ----------------------------------------------------------- |
| 0x0000 (PAT)                              | `section_tx`                                                |
| 0x0010 (NIT)                              | `section_tx`                                                |
| 0x0011 (SDT/BAT)                          | `section_tx`                                                |
| 0x0012 (EIT)                              | `section_tx`                                                |
| 0x0014 (TDT/TOT)                          | `section_tx`                                                |
| PIDs de PMT (descobertos via PAT)         | `section_tx`                                                |
| PIDs de vídeo/áudio (descobertos via PMT) | `pes_tx`                                                    |
| 0x1FFF (Null)                             | descartado (contabilizado em metrics)                       |
| qualquer outro                            | `section_tx` (tentativa; ignorado se table_id desconhecido) |

**SPEC-TS-002b — Validação de Continuity Counter:**

```
CC é válido quando:
  - pid == 0x1FFF         → CC ignorado (null packets não têm CC)
  - adaptation_only       → CC não incrementa (não conta como erro)
  - scrambling != 0       → CC não verificado (stream cifrado)
  - cc_esperado == cc_recebido  → OK
  - cc_esperado != cc_recebido  → emite TsEvent::CcError { pid, expected, got }

cc_esperado = (cc_anterior + 1) & 0x0F
```

**SPEC-TS-002c — Recuperação de sync:**
Se `raw[0] != 0x47` em algum chunk, o demuxer busca o próximo `0x47` no buffer (offset % 188) e loga `TsEvent::SyncLost { bytes_skipped }`. Não retorna erro; continua o processamento.

---

### SPEC-TS-003 · `SectionAssembler`

```rust
/// SPEC-TS-003
/// Reagrupa seções TS que podem ser fragmentadas em múltiplos pacotes.
/// Usa PUSI para detectar início de nova seção.
/// Valida CRC-32 MPEG ao finalizar cada seção completa.
pub struct SectionAssembler {
    buffers: HashMap<Pid, SectionBuffer>,
    tx:      Sender<(Pid, CompleteSection)>,
}

#[derive(Debug)]
pub struct CompleteSection {
    pub table_id:    u8,
    pub data:        Bytes,   // seção completa, sem cabeçalho TS
    pub pid:         Pid,
}
```

**SPEC-TS-003a — Algoritmo de montagem:**

```
Ao receber (pid, payload) com PUSI=true:
  1. Se há buffer pendente para este PID: descarta (seção incompleta) → loga warning.
  2. Lê pointer_field (1 byte): pula os primeiros pointer_field bytes.
  3. Inicia novo SectionBuffer com table_id = payload[pointer_field].
  4. Lê section_length (bytes 2-3, bits 11:0): tamanho total inclui os 3 bytes de cabeçalho.

Ao receber (pid, payload) com PUSI=false:
  1. Appenda ao buffer existente para este PID.
  2. Se buffer atingiu section_length bytes: seção completa → valida CRC → emite.

SPEC-TS-003b — CRC-32:
  Polinômio MPEG-2: 0x04C11DB7
  Os últimos 4 bytes da seção são o CRC.
  Se CRC inválido: descarta seção, emite TsEvent::CrcError { pid, table_id }.
```

**Casos de teste — `ts::tests::spec_ts_003`:**

| Cenário                                     | Comportamento                   |
| ------------------------------------------- | ------------------------------- |
| Seção em pacote único (PUSI=true, completa) | Emitida imediatamente           |
| Seção fragmentada em 3 pacotes              | Emitida só após o 3º pacote     |
| PUSI=true com buffer pendente               | Descarta anterior, inicia nova  |
| CRC inválido                                | Descarta + emite `CrcError`     |
| `section_length` > 4093 (máximo legal)      | `Err(TsError::SectionTooLarge)` |

---

### SPEC-TS-004 · `AdaptationField` e `PcrTracker`

```rust
/// SPEC-TS-004
#[derive(Debug, Clone)]
pub struct AdaptationField {
    pub discontinuity_indicator:   bool,
    pub random_access_indicator:   bool,
    pub pcr:                       Option<u64>,  // 42 bits base + 9 bits ext → 33+9 bits
    pub opcr:                      Option<u64>,
    pub splice_countdown:          Option<i8>,
}

/// SPEC-TS-004a
/// PCR é um valor de 42 bits (base × 300 + ext) em unidades de 27 MHz.
/// Conversão: pcr_seconds = pcr_value / 27_000_000.0
pub fn pcr_to_duration(pcr: u64) -> std::time::Duration;

/// SPEC-TS-004b — PcrTracker
/// Rastreia PCR por PID. Calcula jitter entre PCRs consecutivos do mesmo PID.
/// Emite PcrEvent::Jitter se |delta_esperado - delta_medido| > JITTER_THRESHOLD_US (padrão: 500 µs).
/// Emite PcrEvent::Discontinuity se: flag discontinuity_indicator=true OU salto > 100 ms sem flag.
pub struct PcrTracker {
    state:    HashMap<Pid, PcrState>,
    event_tx: Sender<PcrEvent>,
}

#[derive(Debug, Clone)]
pub enum PcrEvent {
    Jitter        { pid: Pid, expected_us: i64, measured_us: i64 },
    Discontinuity { pid: Pid, reason: DiscontinuityReason },
}

#[derive(Debug, Clone)]
pub enum DiscontinuityReason { Flag, LargeJump { delta_ms: u64 } }
```

---

## 4. Spec: `ts::tables` — Tabelas PSI/SI e DVB

> **Convenção de parsing:** todos os parsers recebem `&[u8]` (o corpo da seção sem CRC) e retornam `Result<T, TableError>`. Nunca fazem panic em dados externos.

---

### SPEC-TABLE-001 · PAT — Program Association Table

```rust
/// SPEC-TABLE-001
/// table_id = 0x00, PID = 0x0000
#[derive(Debug, Clone)]
pub struct Pat {
    pub transport_stream_id: u16,
    pub version:             u8,
    pub current_next:        bool,
    pub programs: Vec<PatProgram>,
}

#[derive(Debug, Clone)]
pub struct PatProgram {
    pub program_number: u16,
    /// Se program_number == 0: este é o NIT PID.
    /// Caso contrário: PMT PID para este programa.
    pub pid: Pid,
}

impl Pat {
    /// SPEC-TABLE-001a
    pub fn parse(section: &[u8]) -> Result<Self, TableError>;
}
```

**Regras de negócio:**
- `SPEC-TABLE-001b`: program_number == 0 indica NIT PID (pode diferir de 0x0010).
- `SPEC-TABLE-001c`: a lista de PMT PIDs resultante é usada pelo demuxer para registrar novos filtros de PID dinamicamente.
- `SPEC-TABLE-001d`: se a versão da PAT mudar (version_number diferente), re-parsear todas as PMTs.

---

### SPEC-TABLE-002 · PMT — Program Map Table

```rust
/// SPEC-TABLE-002
/// table_id = 0x02, PID = conforme PAT
#[derive(Debug, Clone)]
pub struct Pmt {
    pub program_number: u16,
    pub version:        u8,
    pub pcr_pid:        Pid,
    pub program_descriptors: Vec<Descriptor>,
    pub streams: Vec<PmtStream>,
}

#[derive(Debug, Clone)]
pub struct PmtStream {
    pub stream_type: u8,
    pub elementary_pid: Pid,
    pub descriptors: Vec<Descriptor>,
    /// Descrição legível derivada de stream_type
    pub stream_type_label: &'static str,
}

impl Pmt {
    /// SPEC-TABLE-002a
    pub fn parse(section: &[u8]) -> Result<Self, TableError>;
}
```

**Mapeamento `stream_type` → label (SPEC-TABLE-002b):**

| stream_type | label                                |
| ----------- | ------------------------------------ |
| 0x01        | MPEG-1 Video                         |
| 0x02        | MPEG-2 Video                         |
| 0x03        | MPEG-1 Audio (MP1)                   |
| 0x04        | MPEG-2 Audio (MP2)                   |
| 0x0F        | AAC Audio (ADTS)                     |
| 0x11        | AAC Audio (LATM)                     |
| 0x1B        | H.264 / AVC Video                    |
| 0x24        | H.265 / HEVC Video                   |
| 0x81        | AC-3 Audio (ATSC)                    |
| 0x06        | Private Data (verificar descriptors) |
| _           | `"Unknown (0xXX)"`                   |

---

### SPEC-TABLE-003 · NIT — Network Information Table

```rust
/// SPEC-TABLE-003
/// table_id = 0x40 (actual) / 0x41 (other), PID = 0x0010 (ou conforme PAT)
#[derive(Debug, Clone)]
pub struct Nit {
    pub network_id:   u16,
    pub version:      u8,
    pub actual:       bool,   // true = 0x40, false = 0x41
    pub network_name: Option<String>,   // de network_name_descriptor (tag 0x40)
    pub network_descriptors: Vec<Descriptor>,
    pub transport_streams: Vec<NitTransportStream>,
}

#[derive(Debug, Clone)]
pub struct NitTransportStream {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub descriptors: Vec<Descriptor>,
    /// Entrega física decodificada (se presente)
    pub delivery: Option<DeliveryDescriptor>,
}

#[derive(Debug, Clone)]
pub enum DeliveryDescriptor {
    SatelliteDelivery { frequency_hz: u64, polarization: Polarization, symbol_rate: u32 },
    CableDelivery     { frequency_hz: u64, modulation: u8, symbol_rate: u32 },
    TerrestrialDelivery { centre_frequency_hz: u64, bandwidth_hz: u32 },
}
```

---

### SPEC-TABLE-004 · SDT — Service Description Table

```rust
/// SPEC-TABLE-004
/// table_id = 0x42 (actual) / 0x46 (other), PID = 0x0011
#[derive(Debug, Clone)]
pub struct Sdt {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub version:             u8,
    pub actual:              bool,
    pub services: Vec<SdtService>,
}

#[derive(Debug, Clone)]
pub struct SdtService {
    pub service_id:             u16,
    pub eit_schedule_flag:      bool,
    pub eit_present_following:  bool,
    pub running_status:         RunningStatus,
    pub free_ca_mode:           bool,
    pub service_name:           Option<String>,   // de service_descriptor (tag 0x48)
    pub provider_name:          Option<String>,
    pub service_type:           Option<u8>,
    pub descriptors:            Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunningStatus {
    Undefined = 0,
    NotRunning = 1,
    StartsInFewSeconds = 2,
    Pausing = 3,
    Running = 4,
    ServiceOffAir = 5,
}
```

---

### SPEC-TABLE-005 · EIT — Event Information Table

```rust
/// SPEC-TABLE-005
/// table_id = 0x4E (p/f actual) / 0x4F (p/f other) / 0x50–0x5F (sched actual) / 0x60–0x6F (sched other)
/// PID = 0x0012
#[derive(Debug, Clone)]
pub struct Eit {
    pub service_id:          u16,
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub version:             u8,
    pub table_id:            u8,
    pub events:              Vec<EitEvent>,
}

#[derive(Debug, Clone)]
pub struct EitEvent {
    pub event_id:         u16,
    /// UTC-MJD decodificado para NaiveDateTime (chrono)
    pub start_time:       Option<chrono::NaiveDateTime>,
    pub duration_seconds: Option<u32>,  // BCD HH:MM:SS
    pub running_status:   RunningStatus,
    pub free_ca_mode:     bool,
    pub event_name:       Option<String>,   // de short_event_descriptor (tag 0x4D)
    pub short_description: Option<String>,
    pub descriptors:      Vec<Descriptor>,
}

impl EitEvent {
    /// SPEC-TABLE-005a
    /// Converte start_time UTC para horário local do sistema.
    pub fn start_time_local(&self) -> Option<chrono::DateTime<chrono::Local>>;
}
```

**SPEC-TABLE-005b — Decodificação MJD+BCD:**
```
start_time no TS é 5 bytes: 2 bytes MJD + 3 bytes BCD (HH, MM, SS)
MJD → data: Y' = int((MJD - 15078.2) / 365.25), M' = int((MJD - 14956.1 - int(Y'×365.25)) / 30.6001)
Caso especial: se HH=0xFF MM=0xFF SS=0xFF → start_time = None (undefined)
```

---

### SPEC-TABLE-006 · TDT — Time and Date Table

```rust
/// SPEC-TABLE-006
/// table_id = 0x70, PID = 0x0014
/// Seção mínima: 5 bytes de UTC-MJD
#[derive(Debug, Clone)]
pub struct Tdt {
    pub utc_time: chrono::NaiveDateTime,
}

impl Tdt {
    /// SPEC-TABLE-006a
    pub fn parse(section: &[u8]) -> Result<Self, TableError>;

    /// SPEC-TABLE-006b
    /// Diferença entre TDT e relógio local do sistema em segundos.
    /// Positivo = TDT está adiantado; negativo = TDT está atrasado.
    pub fn offset_from_system(&self) -> i64;
}
```

---

### SPEC-TABLE-007 · BAT — Bouquet Association Table

```rust
/// SPEC-TABLE-007
/// table_id = 0x4A, PID = 0x0011
#[derive(Debug, Clone)]
pub struct Bat {
    pub bouquet_id:   u16,
    pub version:      u8,
    pub bouquet_name: Option<String>,   // de bouquet_name_descriptor (tag 0x47)
    pub bouquet_descriptors: Vec<Descriptor>,
    pub transport_streams: Vec<BatTransportStream>,
}

#[derive(Debug, Clone)]
pub struct BatTransportStream {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub descriptors: Vec<Descriptor>,
}
```

---

### SPEC-TABLE-008 · `Descriptor` genérico

```rust
/// SPEC-TABLE-008
/// Descriptor genérico para tags não implementadas explicitamente.
/// Parsers de tabela sempre usam este fallback em vez de retornar erro.
#[derive(Debug, Clone)]
pub struct Descriptor {
    pub tag:  u8,
    pub data: Bytes,    // conteúdo bruto sem tag e length
}

/// SPEC-TABLE-008a — Descriptors decodificados (subset obrigatório v1.0)
#[derive(Debug, Clone)]
pub enum KnownDescriptor {
    NetworkName     { name: String },           // 0x40
    ServiceList     { services: Vec<(u16, u8)> }, // 0x41
    Service         { service_type: u8, provider: String, name: String }, // 0x48
    ShortEvent      { lang: [u8;3], name: String, text: String },         // 0x4D
    BouquetName     { name: String },           // 0x47
    SatelliteDelivery { .. },                   // 0x43
    CableDelivery   { .. },                     // 0x44
    TerrestrialDelivery { .. },                 // 0x5A
    Unknown         { tag: u8, data: Bytes },   // fallback
}

impl Descriptor {
    /// SPEC-TABLE-008b
    /// Tenta decodificar o descriptor para o tipo known.
    /// Nunca retorna Err — sempre cai em Unknown no pior caso.
    pub fn decode(&self) -> KnownDescriptor;
}
```

**SPEC-TABLE-008c — Decodificação de strings DVB:**
Strings DVB podem usar encoding ISO 8859-1 a ISO 8859-15, UTF-8 ou UTF-16.
O primeiro byte indica a tabela de caracteres:
- `0x00` (ausente): ISO 8859-1 implícito
- `0x01`–`0x0B`: ISO 8859-5 a ISO 8859-15
- `0x10 0x00 0xXX`: ISO 8859-XX
- `0x15`: UTF-8

O conversor deve sempre retornar `String` válida em UTF-8 — substituir bytes inválidos por `\u{FFFD}` em vez de retornar Err.

---

### `TableError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum TableError {
    #[error("dados insuficientes: esperado {expected}, got {got}")]
    InsufficientData { expected: usize, got: usize },
    #[error("table_id inesperado: 0x{got:02X} (esperado 0x{expected:02X})")]
    WrongTableId { expected: u8, got: u8 },
    #[error("versão de seção desconhecida")]
    UnknownVersion,
    #[error("CRC inválido")]
    InvalidCrc,
    #[error("section_length inválido: {0}")]
    InvalidSectionLength(u16),
}
```

---

## 5. Spec: `ts::metrics` — Bitrate e Erros

### SPEC-METRICS-001 · `BitrateMonitor`

```rust
/// SPEC-METRICS-001
/// Calcula bitrate por PID usando janela deslizante de duração configurável.
/// Padrão: janela de 1 segundo.
///
/// Algoritmo:
///   - Mantém um VecDeque<(Instant, u64)> por PID com (timestamp, bytes_acumulados).
///   - A cada chamada a update(), remove entradas mais antigas que window_duration.
///   - bitrate_kbps = Σ(bytes na janela) * 8 / window_duration.as_secs_f64() / 1000.0
pub struct BitrateMonitor {
    window: Duration,
    pids:   HashMap<Pid, VecDeque<(Instant, usize)>>,
}

impl BitrateMonitor {
    pub fn new(window: Duration) -> Self;

    /// SPEC-METRICS-001a
    /// Registra `bytes` bytes recebidos para `pid` no instante atual.
    pub fn update(&mut self, pid: Pid, bytes: usize);

    /// SPEC-METRICS-001b
    /// Retorna bitrate atual em kbps para o PID.
    /// Retorna 0.0 se o PID não foi visto na janela atual.
    pub fn bitrate_kbps(&self, pid: Pid) -> f64;

    /// SPEC-METRICS-001c
    /// Retorna snapshot de todos os PIDs com bitrate > 0, ordenado por bitrate desc.
    pub fn snapshot(&self) -> Vec<PidBitrateEntry>;

    /// SPEC-METRICS-001d
    /// Bitrate total (soma de todos os PIDs incluindo null packets).
    pub fn total_bitrate_kbps(&self) -> f64;

    /// SPEC-METRICS-001e
    /// Proporção de null packets (PID 0x1FFF) sobre o total. Range: 0.0–1.0.
    pub fn null_packet_ratio(&self) -> f64;
}

#[derive(Debug, Clone)]
pub struct PidBitrateEntry {
    pub pid:          Pid,
    pub bitrate_kbps: f64,
    pub packet_count: u64,
}
```

---

### SPEC-METRICS-002 · `ErrorTracker`

```rust
/// SPEC-METRICS-002
/// Acumula contadores de erro por tipo e por PID.
/// Thread-safe via Arc<Mutex<ErrorTracker>> ou uso em thread única com snapshot via canal.
#[derive(Debug, Default)]
pub struct ErrorTracker {
    pub cc_errors:          HashMap<Pid, u64>,
    pub pcr_jitter_events:  Vec<PcrJitterRecord>,
    pub pcr_discontinuities: Vec<PcrDiscontinuityRecord>,
    pub crc_errors:         HashMap<(Pid, u8), u64>,  // (pid, table_id)
    pub sync_losses:        u64,
    pub rtp_out_of_order:   u64,
    pub udp_overflows:      u64,
}

#[derive(Debug, Clone)]
pub struct PcrJitterRecord {
    pub pid:         Pid,
    pub timestamp:   Instant,
    pub expected_us: i64,
    pub measured_us: i64,
}

impl ErrorTracker {
    /// SPEC-METRICS-002a
    /// Retorna snapshot imutável para a UI consumir sem bloquear o pipeline.
    pub fn snapshot(&self) -> ErrorSnapshot;

    /// SPEC-METRICS-002b
    /// Total de erros de CC em todos os PIDs.
    pub fn total_cc_errors(&self) -> u64;

    /// SPEC-METRICS-002c
    /// Limpa todos os contadores (ação do usuário na UI).
    pub fn reset(&mut self);
}
```

---

### SPEC-METRICS-003 · `MetricsAggregator`

```rust
/// SPEC-METRICS-003
/// Combina BitrateMonitor e ErrorTracker em uma struct única que a UI consome.
/// Roda em thread dedicada; recebe TsEvent e PcrEvent via canal.
/// Publica MetricsSnapshot a cada 1 segundo via watch channel (tokio::sync::watch).
pub struct MetricsAggregator {
    ts_rx:    Receiver<TsEvent>,
    pcr_rx:   Receiver<PcrEvent>,
    net_rx:   Receiver<NetEvent>,
    snapshot_tx: tokio::sync::watch::Sender<MetricsSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub pid_table:          Vec<PidEntry>,
    pub total_bitrate_kbps: f64,
    pub null_ratio:         f64,
    pub errors:             ErrorSnapshot,
    pub tdt_offset_secs:    Option<i64>,
    pub timestamp:          Instant,
}

#[derive(Debug, Clone)]
pub struct PidEntry {
    pub pid:          Pid,
    pub pid_type:     PidType,
    pub label:        String,
    pub bitrate_kbps: f64,
    pub cc_errors:    u64,
    pub packet_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PidType {
    Pat, Pmt, Nit, Sdt, Eit, Tdt, Bat,
    Video { codec: VideoCodec },
    Audio { codec: AudioCodec },
    Pcr,
    NullPacket,
    Unknown,
}
```

---

## 6. Spec: `av` — Bridge FFmpeg

### SPEC-AV-001 · `PesAssembler`

```rust
/// SPEC-AV-001
/// Monta PES packets a partir de payloads de pacotes TS.
/// Um PES pode se fragmentar em múltiplos pacotes TS; o PES header
/// aparece sempre no primeiro pacote (PUSI=true).
pub struct PesAssembler {
    buffers: HashMap<Pid, PesBuffer>,
    tx:      Sender<PesPacket>,
}

#[derive(Debug, Clone)]
pub struct PesPacket {
    pub pid:          Pid,
    pub stream_id:    u8,
    pub pts:          Option<i64>,   // 90 kHz clock; None se não presente
    pub dts:          Option<i64>,
    pub data:         Bytes,         // payload ES (sem PES header)
}

impl PesPacket {
    /// SPEC-AV-001a
    /// Converte PTS de 90 kHz para Duration.
    pub fn pts_duration(&self) -> Option<std::time::Duration>;
}
```

**SPEC-AV-001b — Decodificação de PTS/DTS:**
```
PTS/DTS são valores de 33 bits no cabeçalho PES.
pts_value = (byte[0] & 0x0E) << 29 | byte[1] << 22 | (byte[2] & 0xFE) << 14
           | byte[3] << 7 | (byte[4] >> 1)
Verificar pts_dts_flags antes de ler: bits 7:6 do PES header byte 2.
  0b10 = apenas PTS presente
  0b11 = PTS e DTS presentes
  0b00 = nenhum presente
```

---

### SPEC-AV-002 · `FfmpegDecoder`

```rust
/// SPEC-AV-002
/// Wrapper seguro sobre libavcodec para decodificação de vídeo e áudio.
/// Usa ffmpeg-next (ffmpeg-sys-next como backend).
/// NUNCA usa unsafe fora deste módulo.
pub struct FfmpegDecoder {
    kind:    DecoderKind,
    context: CodecContext,   // wrapper safe sobre AVCodecContext
}

#[derive(Debug, Clone)]
pub enum DecoderKind {
    Video { codec: VideoCodec, width: u32, height: u32, fps: f64 },
    Audio { codec: AudioCodec, sample_rate: u32, channels: u16 },
}

impl FfmpegDecoder {
    /// SPEC-AV-002a
    /// Inicializa o decoder a partir do stream_type da PMT.
    /// Retorna Err se o codec não for suportado.
    pub fn from_stream_type(stream_type: u8) -> Result<Self, AvError>;

    /// SPEC-AV-002b
    /// Envia um PES packet para decodificação.
    /// Retorna zero ou mais frames decodificados.
    /// Em caso de erro de decodificação: loga warning e retorna Ok(vec![]) —
    /// nunca propaga erro de frame individual ao caller.
    pub fn decode(&mut self, pes: &PesPacket) -> Result<Vec<DecodedFrame>, AvError>;
}

#[derive(Debug)]
pub enum DecodedFrame {
    Video(VideoFrame),
    Audio(AudioFrame),
}

#[derive(Debug)]
pub struct VideoFrame {
    pub width:  u32,
    pub height: u32,
    pub pts:    Option<i64>,
    /// Dados RGB24 ou YUV420P; formato fixado em RGB24 para a UI
    pub data:   Vec<u8>,
    pub stride: u32,
}

#[derive(Debug)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels:    u16,
    pub pts:         Option<i64>,
    /// PCM interleaved f32
    pub samples:     Vec<f32>,
}
```

**Codecs suportados em v1.0 (SPEC-AV-002c):**

| VideoCodec   | libavcodec decoder |
| ------------ | ------------------ |
| `H264`       | `h264`             |
| `Hevc`       | `hevc`             |
| `Mpeg2Video` | `mpeg2video`       |

| AudioCodec | libavcodec decoder |
| ---------- | ------------------ |
| `Aac`      | `aac`              |
| `Ac3`      | `ac3`              |
| `Mp2`      | `mp2`              |
| `Eac3`     | `eac3`             |

---

### SPEC-AV-003 · `VideoRenderer`

```rust
/// SPEC-AV-003
/// Renderiza VideoFrames como textura em widget egui via wgpu (backend D3D11 no Windows).
/// Integrado ao ciclo de UI: chamado a cada frame de tela (60 Hz).
pub struct VideoRenderer {
    device:  wgpu::Device,
    queue:   wgpu::Queue,
    texture: wgpu::Texture,
}

impl VideoRenderer {
    /// SPEC-AV-003a
    /// Upload de um VideoFrame para a textura GPU.
    /// Se frame.width != textura.width: recria a textura com as novas dimensões.
    pub fn upload_frame(&mut self, frame: &VideoFrame);

    /// SPEC-AV-003b
    /// Retorna a textura como egui::TextureId para uso em ui::Image.
    pub fn texture_id(&self) -> egui::TextureId;

    /// SPEC-AV-003c — Fallback de compatibilidade
    /// Se wgpu não conseguir criar device D3D11 (GPU sem suporte):
    /// usa modo CPU: converte VideoFrame → egui::ColorImage via swscale.
    pub fn is_gpu_mode(&self) -> bool;
}
```

---

### SPEC-AV-004 · `AudioOutput`

```rust
/// SPEC-AV-004
/// Saída de áudio via cpal (WASAPI no Windows).
/// Mantém buffer de jitter interno de tamanho configurável (padrão: 100 ms).
pub struct AudioOutput {
    stream:      cpal::Stream,
    buffer:      Arc<Mutex<AudioRingBuffer>>,
    sample_rate: u32,
    channels:    u16,
}

impl AudioOutput {
    /// SPEC-AV-004a
    /// Envia amostras para o buffer de jitter.
    /// Thread-safe; pode ser chamado da thread do decoder.
    pub fn push_samples(&self, frame: &AudioFrame);

    /// SPEC-AV-004b
    /// Ajusta volume (0.0 = mute, 1.0 = nominal, > 1.0 boost com clip).
    pub fn set_volume(&self, volume: f32);

    /// SPEC-AV-004c
    /// Retorna nível de ocupação do buffer de jitter (0.0–1.0).
    /// UI usa para exibir indicador de buffer health.
    pub fn buffer_level(&self) -> f32;
}
```

---

## 7. Spec: `ui` — Interface Principal

### SPEC-UI-001 · Layout e Painéis

```
┌─────────────────────────────────────────────────────────────────┐
│  Header: URL input ──────────────── [Conectar] [Desconectar]    │
├───────────────────────┬──────────────────┬──────────────────────┤
│                       │                  │                      │
│   VideoPanel (40%)    │  AnalysisPanel   │   MetricsPanel       │
│                       │  (35%)           │   (25%)              │
│   [vídeo aqui]        │  ┌─ Aba: PIDs   │   Bitrate graph      │
│                       │  ├─ Aba: Tables │   PCR jitter graph   │
│   Codec / Res / FPS   │  └─ Aba: Svc   │   Error log          │
│   Volume slider       │                  │                      │
├───────────────────────┴──────────────────┴──────────────────────┤
│  StatusBar: estado · bitrate total · CC errors · TDT offset     │
└─────────────────────────────────────────────────────────────────┘
```

---

### SPEC-UI-002 · `AppState`

```rust
/// SPEC-UI-002
/// Estado global imutável (snap lido pela UI a cada frame).
/// A UI nunca escreve diretamente — envia comandos via AppCommand.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub connection:     ConnectionState,
    pub metrics:        MetricsSnapshot,
    pub tables:         TablesSnapshot,
    pub selected_pid:   Option<Pid>,
    pub selected_service: Option<u16>,  // service_id para MPTS
    pub bitrate_history: VecDeque<(Instant, f64)>,  // últimos 60 s
    pub pcr_history:    HashMap<Pid, VecDeque<PcrJitterRecord>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ConnectionState {
    #[default]
    Idle,
    Connecting { url: String },
    Connected   { url: String, since: Instant },
    Error       { url: String, reason: String },
}

/// Comandos enviados pela UI para o backend (via mpsc)
#[derive(Debug, Clone)]
pub enum AppCommand {
    Connect    { url: String, iface: Option<String> },
    Disconnect,
    SelectService { service_id: u16 },
    SelectPid     { pid: Pid },
    SetVolume     { volume: f32 },
    ResetErrors,
    ChangeTheme   { dark: bool },
}
```

---

### SPEC-UI-003 · `PidPanel`

**Colunas obrigatórias:**

| Coluna         | Tipo                | Ordenável          |
| -------------- | ------------------- | ------------------ |
| PID (hex)      | `String` ("0x0100") | sim                |
| Tipo           | `PidType` label     | sim                |
| Descrição      | `String`            | não                |
| Bitrate (kbps) | `f64`               | sim                |
| Pacotes        | `u64`               | sim                |
| Erros CC       | `u64`               | sim (padrão: desc) |

**Comportamento (SPEC-UI-003a):**
- Linha com `cc_errors > 0`: fundo vermelho claro.
- Linha com `pid_type == NullPacket`: cor de texto atenuada.
- Clique em linha: atualiza `AppState::selected_pid` → MetricsPanel exibe histórico do PID selecionado.
- Atualização da tabela: a cada snapshot recebido (≈ 1 Hz), sem flickering (diferença incremental).

---

### SPEC-UI-004 · `TablesPanel`

**Aba "PIDs":** ver SPEC-UI-003.

**Aba "Tables" — árvore (SPEC-UI-004a):**

```
▼ PAT  (v3, TS-ID: 0x0001)
  ├─ NIT PID: 0x0010
  ▼ Programa 1  →  PMT PID: 0x0100
      PCR PID: 0x0100
      ├─ 0x0100  H.264 Video
      └─ 0x0101  AAC Audio
  ▼ Programa 2  →  PMT PID: 0x0200
      ...
▼ NIT  (v2, network: "Operadora BR")
  ▼ TS 0x0001 (original_network: 0x0001)
      Cable delivery: 343.000 MHz, QAM-256, SR: 6900 kBaud
▼ SDT  (v5, TS: 0x0001)
  ├─ Svc 1: "Canal 1"  [Running]  EIT p/f ✓
  └─ Svc 2: "Canal 2"  [Running]
▼ EIT p/f  (Svc 1)
  ├─ Atual: "Jornal Nacional"  21:00–22:00
  └─ Próximo: "Fantástico"  22:00–23:30
▼ TDT  →  2024-11-15 21:34:12 UTC  (sistema: +0s)
▼ BAT  (bouquet: 0x0001 "Pack Básico")
  ├─ Svc 1, Svc 2, ...
```

**Aba "Serviços" (SPEC-UI-004b):**
Lista de serviços cruzando SDT + PMT. Clique duplo em um serviço: envia `AppCommand::SelectService` e o player muda para aquele serviço.

---

### SPEC-UI-005 · `MetricsPanel`

**Gráfico de bitrate (SPEC-UI-005a):**
- Eixo Y: kbps; eixo X: últimos 60 segundos.
- Linha cinza = bitrate total; linha colorida = PID selecionado.
- Implementado com `egui_plot::Plot`.

**Gráfico de PCR jitter (SPEC-UI-005b):**
- Exibido apenas para PIDs com PCR.
- Eixo Y: µs; linha de limiar em ±500 µs tracejada em vermelho.

**Log de erros (SPEC-UI-005c):**
- Tabela rolável com colunas: `Timestamp | Tipo | PID | Detalhe`.
- Máximo de 1000 entradas; entradas mais antigas são descartadas.
- Botão "Copiar para área de transferência": copia o log em TSV.
- Botão "Limpar": envia `AppCommand::ResetErrors`.

---

### SPEC-UI-006 · StatusBar

```
[ ● Conectado  udp://@239.1.1.1:1234 ] [ 18.4 Mbps ] [ CC: 0 ] [ TDT: +0s ] [ v0.1.0 ]
[ ○ Desconectado ]
[ ⚠ Erro: timeout após 5s ]
```

**SPEC-UI-006a:** o indicador de status usa ícone + texto, nunca apenas cor.

---

## 8. Contratos de Canal entre Componentes

> Todos os canais são **bounded** para exercer backpressure. Capacidades abaixo são defaults; configuráveis em `AppConfig`.

```
                    ┌─────────────────────────────────────────────┐
                    │              main.rs (wiring)               │
                    └─────────────────────────────────────────────┘

UdpReceiver ──[Bytes, cap:128]──► RtpStripper ──[Bytes, cap:128]──► TsDemuxer
                                                                        │
                     ┌──────────────────────────────────────────────────┤
                     │                          │                        │
              [SectionData, cap:64]     [PesPacket, cap:256]    [TsEvent, cap:1024]
                     │                          │                        │
            SectionAssembler          PesAssembler              MetricsAggregator
                     │                          │                   ▲    ▲
              [CompleteSection]        [PesPacket]           [PcrEvent] [NetEvent]
                     │                          │                   │
              TableDispatcher          FfmpegDecoder          PcrTracker
                     │                    │      │
              [TableEvent]       [VideoFrame] [AudioFrame]
                     │                    │      │
                  UI/TablesPanel    VideoRenderer AudioOutput

MetricsAggregator ──[watch::Sender<MetricsSnapshot>]──► UI/MetricsPanel
```

**Regra de backpressure (SPEC-CHAN-001):**
Se qualquer canal atingir 90% da capacidade, o produtor emite um log `WARN` com o nome do canal e a capacidade. Se atingir 100%: comportamento por canal:

| Canal              | Comportamento no full                                           |
| ------------------ | --------------------------------------------------------------- |
| `Bytes` (net → ts) | Drop do buffer + incrementa `udp_overflows`                     |
| `SectionData`      | Drop da seção + loga                                            |
| `PesPacket`        | Drop do PES + loga (frame perdido, não é erro fatal)            |
| `TsEvent`          | Drop do evento (métricas podem perder precisão momentaneamente) |
| `VideoFrame`       | Drop do frame mais antigo (FIFO; frame mais novo prevalece)     |
| `AudioFrame`       | Drop do frame se buffer de jitter > 2× tamanho nominal          |

---

## 9. Configuração e Estado Global

### `AppConfig`

```rust
/// SPEC-CFG-001
/// Carregada de ironstream.toml na pasta do executável.
/// Valores padrão sempre definidos via Default.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub network:  NetworkConfig,
    pub player:   PlayerConfig,
    pub analyzer: AnalyzerConfig,
    pub ui:       UiConfig,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct NetworkConfig {
    pub udp_buffer_bytes:    usize,    // padrão: 4_194_304 (4 MB)
    pub timeout_ms:          u64,      // padrão: 5_000
    pub preferred_iface:     Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PlayerConfig {
    pub jitter_buffer_ms:    u64,      // padrão: 100
    pub volume:              f32,      // padrão: 1.0; range: 0.0–2.0
    pub fallback_cpu_render: bool,     // padrão: false
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AnalyzerConfig {
    pub bitrate_window_secs:   u64,    // padrão: 1
    pub bitrate_history_secs:  u64,    // padrão: 60
    pub pcr_jitter_threshold_us: i64,  // padrão: 500
    pub top_pids_count:        usize,  // padrão: 10
    pub max_error_log_entries: usize,  // padrão: 1_000
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UiConfig {
    pub dark_theme:    bool,    // padrão: true
    pub window_width:  u32,     // padrão: 1400
    pub window_height: u32,     // padrão: 900
}
```

---

## 10. Build, Testes e CI

### Estrutura de testes

```
# Testes de unidade (cada módulo)
cargo test -p ts       # SPEC-TS-*, SPEC-TABLE-*
cargo test -p net      # SPEC-NET-*
cargo test -p av       # SPEC-AV-*

# Testes de integração (crate raiz)
cargo test --test integration

# Todos
cargo test --workspace
```

### Convenção de nomenclatura de testes

```rust
// Formato: spec_{spec_id_lowercase}_{descrição_do_caso}
#[test]
fn spec_ts_001_invalid_sync_byte() { ... }

#[test]
fn spec_table_001a_pat_parse_basic() { ... }

#[test]
fn spec_net_003_rtp_header_stripping() { ... }
```

### Fixtures de teste

```
tests/fixtures/
├── pat_section.bin          # seção PAT real capturada de DVB-C
├── pmt_h264_aac.bin
├── nit_cable.bin
├── sdt_actual.bin
├── eit_pf.bin
├── eit_schedule.bin
├── tdt.bin
├── bat.bin
├── ts_packets_cc_error.bin  # stream sintético com erro de CC no pacote 5
├── ts_rtp_wrapped.bin       # pacotes TS encapsulados em RTP
└── ts_fragmented_section.bin # seção PAT fragmentada em 3 pacotes TS
```

> **Regra:** toda fixture deve ter um comentário no topo do test file explicando a origem (capturado de equipamento real, gerado sinteticamente, etc.).

### `Cargo.toml` de dev-dependencies por crate

```toml
# crates/ts/Cargo.toml
[dev-dependencies]
rstest      = "0.23"    # parametrize test cases
proptest    = "1"       # property-based testing para o parser de seções
pretty_assertions = "1"
```

### CI (`.github/workflows/ci.yml` — esqueleto)

```yaml
jobs:
  test:
    runs-on: windows-latest   # target primário
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --workspace --locked
      - run: cargo clippy --workspace -- -D warnings
      - run: cargo fmt --check

  build-release:
    runs-on: windows-latest
    steps:
      - run: cargo build --release --locked
      - run: |
          copy ffmpeg\*.dll target\release\
          # validar que o binário roda sem instalação
          target\release\ironstream.exe --version
```

---

## 11. Critérios de Aceite por Fase

### Alpha v0.1 — Core TS + PIDs

- [ ] `SPEC-NET-001`: `StreamUrl::parse` passa todos os 6 casos da tabela.
- [ ] `SPEC-NET-002`: `UdpReceiver` conecta em stream multicast local (testável com `ffmpeg -re -i test.ts -f mpegts udp://239.1.1.1:1234`).
- [ ] `SPEC-TS-001`: `TsPacket::parse` passa todos os 6 cenários.
- [ ] `SPEC-TS-002`: demuxer detecta e contabiliza erros de CC em fixture `ts_packets_cc_error.bin`.
- [ ] `SPEC-TS-003`: seção fragmentada em 3 pacotes é montada corretamente.
- [ ] `SPEC-TABLE-001a`: PAT parseada corretamente da fixture `pat_section.bin`.
- [ ] `SPEC-TABLE-002a`: PMT parseada com todos os stream_types corretos.
- [ ] `SPEC-UI-003`: tabela de PIDs exibe ao menos PID, tipo e bitrate para um stream ao vivo.

### Alpha v0.2 — Player A/V

- [ ] `SPEC-AV-001`: PesAssembler reconstrói um PES de vídeo H.264 fragmentado em 4 pacotes TS.
- [ ] `SPEC-AV-002`: `FfmpegDecoder::decode` produz `VideoFrame` não-vazio para fixture H.264.
- [ ] `SPEC-AV-003`: vídeo exibido na janela egui sem tearing visível em 1080p/25fps.
- [ ] `SPEC-AV-004`: áudio reproduzido sincronizado com vídeo (desvio < 40 ms).
- [ ] Latência fim-a-fim < 2 s em rede local.

### Beta v0.3 — Tabelas DVB

- [ ] `SPEC-TABLE-003`: NIT parseada corretamente de fixture `nit_cable.bin` (verificar `network_name` e delivery descriptor).
- [ ] `SPEC-TABLE-004`: SDT correta de `sdt_actual.bin`; `service_name` e `running_status` corretos.
- [ ] `SPEC-TABLE-005`: EIT p/f correta de `eit_pf.bin`; `start_time` local correto (comparar manualmente).
- [ ] `SPEC-TABLE-005b`: decodificação MJD+BCD correta para data conhecida.
- [ ] `SPEC-TABLE-006a`: TDT correta de `tdt.bin`; offset_from_system coerente.
- [ ] `SPEC-TABLE-007`: BAT correta de `bat.bin`; `bouquet_name` correto.
- [ ] `SPEC-UI-004a`: árvore de tabelas exibe PAT → PMT → streams e NIT/SDT/EIT/TDT/BAT sem crash para stream DVB-C real.

### Beta v0.4 — Métricas de Bitrate

- [ ] `SPEC-METRICS-001b`: bitrate por PID com desvio ≤ 5% vs. `tsduck tsp --pid-bitrate`.
- [ ] `SPEC-METRICS-001e`: null packet ratio correto (comparar com `tsp --null-packet-stats`).
- [ ] `SPEC-UI-005a`: gráfico de bitrate atualiza a cada 1 s sem freezar a UI.

### Beta v0.5 — Detecção de Erros

- [ ] `SPEC-TS-002b`: 100% dos CC errors na fixture `ts_packets_cc_error.bin` detectados.
- [ ] `SPEC-TS-004b`: jitter de PCR > 500 µs detectado em stream sintético.
- [ ] `SPEC-TS-004b`: PCR discontinuity com flag setada detectada.
- [ ] `SPEC-METRICS-002`: `ErrorTracker::reset` zera todos os contadores; UI reflete o reset.
- [ ] `SPEC-UI-005c`: log de erros exibe entradas corretas; botão "Copiar" gera TSV válido.

### RC v0.9 — Estabilização

- [ ] `SPEC-NET-003`: RTP stripping correto para fixture `ts_rtp_wrapped.bin`.
- [ ] `SPEC-UI-004b`: seleção de serviço em MPTS funciona em < 2 s.
- [ ] `SPEC-CFG-001`: `ironstream.toml` carregado corretamente; valores padrão aplicados quando ausente.
- [ ] Binário único de ≤ 60 MB (incluindo DLLs) roda em Windows 10 21H2 limpo.
- [ ] Zero `unsafe` fora de `crates/av/src/ffmpeg_bridge.rs`.

### v1.0 — Release

- [ ] 8 horas de operação contínua: zero crash, crescimento de memória ≤ 10 MB/h.
- [ ] Todos os testes `--workspace` passando em `cargo test --locked`.
- [ ] `cargo clippy -- -D warnings` sem warnings.
- [ ] README com instruções de build e screenshot da UI.

---

*IronStream Spec — fim do documento*
