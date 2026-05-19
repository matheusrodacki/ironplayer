# TDD: `crates/ts::metrics` — Bitrate e Rastreamento de Erros

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-METRICS-001, SPEC-METRICS-002, SPEC-METRICS-003
- **Fase:** Alpha v0.1 (METRICS-001) · v1.0 (METRICS-002/003)

---

## Contexto e Problema

Profissionais de broadcast precisam monitorar bitrate por PID e acumular estatísticas de erros em tempo real, sem impactar o pipeline de decodificação. O sistema de métricas deve:

1. Calcular bitrate por PID com janela deslizante (não média total)
2. Acumular contadores de CC errors, jitter PCR, erros de CRC e perdas de sync
3. Publicar snapshots imutáveis para a UI a 1 Hz via `watch` channel, sem bloqueio

O módulo vive em `crates/ts/src/metrics.rs` — é parte do crate `ts` para ter acesso direto aos tipos `Pid`, `TsEvent` e `PcrEvent`.

---

## Escopo

**In-scope:**
- `BitrateMonitor`: janela deslizante por PID com `VecDeque`
- `ErrorTracker`: contadores de CC, jitter PCR, CRC, sync, RTP OOO, UDP overflow
- `MetricsAggregator`: thread dedicada que consome eventos e publica snapshots
- `MetricsSnapshot`: struct de snapshot imutável clonável pela UI
- `PidEntry`: por PID com tipo, label, bitrate, erros CC

**Out-of-scope:**
- Persistência de métricas em disco
- Exportação para Prometheus / InfluxDB (roadmap pós-v1.0)
- Alertas configuráveis (roadmap)

---

## Solução Técnica

### Estrutura de módulos

```
crates/ts/src/
├── metrics.rs          # BitrateMonitor, ErrorTracker, MetricsSnapshot
└── aggregator.rs       # MetricsAggregator (thread loop)
```

### Dependências

`MetricsAggregator` usa `tokio::sync::watch` para publicar snapshots. Adicionar ao `Cargo.toml` de `ts`:

```toml
[dependencies]
tokio = { workspace = true, features = ["sync"] }
```

> **Decisão:** `tokio::sync::watch` (múltiplos readers, 1 writer) é o canal ideal para snapshots de UI — a UI sempre vê o valor mais recente sem fazer poll. Alternativa `arc-swap` foi considerada mas `watch` é mais idiomática e inclui notificação de mudança.

---

## Contratos de Interface

### `BitrateMonitor` (SPEC-METRICS-001)

```rust
pub struct BitrateMonitor {
    window:   std::time::Duration,  // default: 1 segundo
    pids:     HashMap<Pid, VecDeque<(std::time::Instant, usize)>>,
}

impl BitrateMonitor {
    pub fn new(window: Duration) -> Self;

    /// SPEC-METRICS-001a: registra bytes para pid no instante atual
    pub fn update(&mut self, pid: Pid, bytes: usize);

    /// SPEC-METRICS-001b: bitrate em kbps; 0.0 se não visto na janela
    pub fn bitrate_kbps(&self, pid: Pid) -> f64;

    /// SPEC-METRICS-001c: snapshot de todos PIDs com bitrate > 0, desc
    pub fn snapshot(&self) -> Vec<PidBitrateEntry>;

    /// SPEC-METRICS-001d: soma de todos os PIDs (incluindo null)
    pub fn total_bitrate_kbps(&self) -> f64;

    /// SPEC-METRICS-001e: proporção de null packets (0.0–1.0)
    pub fn null_packet_ratio(&self) -> f64;
}

pub struct PidBitrateEntry {
    pub pid:          Pid,
    pub bitrate_kbps: f64,
    pub packet_count: u64,
}
```

**Algoritmo da janela deslizante:**
```
update(pid, bytes):
  Adicionar (Instant::now(), bytes) ao VecDeque do PID
  Remover entradas mais antigas que agora - window

bitrate_kbps(pid):
  sum_bytes = soma de todos os .1 (bytes) na VecDeque
  sum_bytes * 8 / window.as_secs_f64() / 1000.0
```

> **Importante:** `null_packet_ratio` divide bytes do PID 0x1FFF pelos bytes totais de todos os PIDs — não conta pacotes, conta bytes.

---

### `ErrorTracker` (SPEC-METRICS-002)

```rust
pub struct ErrorTracker {
    pub cc_errors:           HashMap<Pid, u64>,
    pub pcr_jitter_events:   Vec<PcrJitterRecord>,
    pub pcr_discontinuities: Vec<PcrDiscontinuityRecord>,
    pub crc_errors:          HashMap<(Pid, u8), u64>,   // (pid, table_id)
    pub sync_losses:         u64,
    pub rtp_out_of_order:    u64,
    pub udp_overflows:       u64,
}

pub struct PcrJitterRecord {
    pub pid:         Pid,
    pub timestamp:   std::time::Instant,
    pub expected_us: i64,
    pub measured_us: i64,
}

impl ErrorTracker {
    /// SPEC-METRICS-002a: snapshot imutável para a UI
    pub fn snapshot(&self) -> ErrorSnapshot;

    /// SPEC-METRICS-002b: total de CC errors em todos os PIDs
    pub fn total_cc_errors(&self) -> u64;

    /// SPEC-METRICS-002c: limpa todos os contadores
    pub fn reset(&mut self);
}
```

### `MetricsAggregator` (SPEC-METRICS-003)

```rust
pub struct MetricsAggregator {
    ts_rx:       crossbeam_channel::Receiver<TsEvent>,
    pcr_rx:      crossbeam_channel::Receiver<PcrEvent>,
    net_rx:      crossbeam_channel::Receiver<NetEvent>,
    snapshot_tx: tokio::sync::watch::Sender<MetricsSnapshot>,
    bitrate:     BitrateMonitor,
    errors:      ErrorTracker,
    pid_info:    HashMap<Pid, PidInfo>,  // tipo e label por PID
}

impl MetricsAggregator {
    pub fn new(
        ts_rx: Receiver<TsEvent>,
        pcr_rx: Receiver<PcrEvent>,
        net_rx: Receiver<NetEvent>,
    ) -> (Self, tokio::sync::watch::Receiver<MetricsSnapshot>);

    /// Loop principal: processa eventos e publica snapshot a cada 1s
    pub fn run(mut self, stop: StopToken);
}
```

**Loop do aggregator:**
```
Último snapshot em: last_publish = Instant::now()
Loop:
  1. Drenar todos os canais (crossbeam::select! non-blocking)
     - TsEvent::CcError         → errors.cc_errors[pid]++
     - TsEvent::CrcError        → errors.crc_errors[(pid,tid)]++
     - TsEvent::SyncLost        → errors.sync_losses++
     - TsEvent::Packet { pid, bytes } → bitrate.update(pid, bytes)
     - PcrEvent::Jitter         → errors.pcr_jitter_events.push(...)
     - PcrEvent::Discontinuity  → errors.pcr_discontinuities.push(...)
     - NetEvent::UdpBufferOverflow → errors.udp_overflows++
     - RtpEvent::OutOfOrder     → errors.rtp_out_of_order++
  2. Se agora - last_publish >= 1s:
     snapshot_tx.send(build_snapshot())
     last_publish = agora
  3. Se stop.is_stopped() → break
  4. sleep(10ms) se canais vazios
```

### `MetricsSnapshot` (SPEC-METRICS-003)

```rust
pub struct MetricsSnapshot {
    pub pid_table:          Vec<PidEntry>,
    pub total_bitrate_kbps: f64,
    pub null_ratio:         f64,
    pub errors:             ErrorSnapshot,
    pub tdt_offset_secs:    Option<i64>,
    pub timestamp:          std::time::Instant,
}

pub struct PidEntry {
    pub pid:          Pid,
    pub pid_type:     PidType,
    pub label:        String,
    pub bitrate_kbps: f64,
    pub cc_errors:    u64,
    pub packet_count: u64,
}

pub enum PidType {
    Pat, Pmt, Nit, Sdt, Eit, Tdt, Bat,
    Video { codec: VideoCodec },
    Audio { codec: AudioCodec },
    Pcr, NullPacket, Unknown,
}
```

---

## Estratégia de Testes

```
spec_metrics_001_bitrate_window_sliding     — janela de 500ms, verifica expiração
spec_metrics_001b_bitrate_single_pid        — bitrate de 1 pacote de 188 bytes / 1s = 1.504 kbps
spec_metrics_001c_snapshot_sorted_desc      — snapshot ordenado por bitrate desc
spec_metrics_001d_total_includes_null       — PID 0x1FFF incluído no total
spec_metrics_001e_null_ratio                — razão null/total
spec_metrics_002a_snapshot_immutable        — snapshot não reflete mudança posterior
spec_metrics_002b_total_cc_errors           — soma correta de todos os PIDs
spec_metrics_002c_reset_zeroes_counters     — reset limpa tudo
```

---

## Considerações de Segurança

- `Vec<PcrJitterRecord>` limitado a `max_error_log_entries` (SPEC-CFG-001) para evitar crescimento ilimitado.
- `MetricsSnapshot` é `Clone` mas não `Copy` — clonagem explícita pelo caller.
- Nenhum dado externo controla alocação de memória (tamanho de `HashMap` cresce proporcionalmente ao número de PIDs, que é finito: máx. 8192 PIDs distintos em um multiplex TS).

---

## Riscos e Mitigações

| Risco                                                                       | Mitigação                                                                                                   |
| --------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `VecDeque` cresce indefinidamente se janela muito grande                    | Limitar `window` máximo a 300s via `AppConfig`                                                              |
| Snapshot publicado a cada 1s pode ser caro em streams com >100 PIDs         | `PidEntry` usa `String` por label — aceitar custo; 100 PIDs × ~50 bytes ≈ 5 KB por snapshot                 |
| `ErrorTracker::reset()` chamado da thread da UI enquanto aggregator escreve | Aggregator roda em thread única; reset envia `AppCommand::ResetErrors` via canal para garantir serialização |
