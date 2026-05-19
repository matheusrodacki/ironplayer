# TDD: Wiring — Contratos de Canal e `main.rs`

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-CHAN-001, SPEC-CFG-001
- **Fase:** Alpha v0.1

---

## Contexto e Problema

O `main.rs` (crate raiz) é responsável por:

1. Carregar `AppConfig` de `ironstream.toml`
2. Criar todos os canais inter-thread bounded
3. Instanciar e conectar todos os componentes
4. Iniciar as threads de backend
5. Iniciar o loop egui (blocking na main thread)

Este TDD define os contratos de canal (capacidades, comportamento em full) e o processo de bootstrap da aplicação.

---

## Escopo

**In-scope:**
- Diagrama de canais e capacidades padrão (SPEC-CHAN-001)
- Comportamento de backpressure por canal
- `AppConfig` e leitura de `ironstream.toml` (SPEC-CFG-001)
- Sequência de bootstrap e wiring em `main.rs`
- Tratamento de erros fatais de startup

**Out-of-scope:**
- Lógica de negócio (responsabilidade dos outros crates)
- Persistência de estado (futura versão)

---

## Solução Técnica

### Mapa de canais

```
UdpReceiver ─[Bytes, cap:128]─► RtpStripper ─[Bytes, cap:128]─► TsDemuxer
                                                                      │
                  ┌───────────────────────────────────────────────────┤
                  │                        │                          │
         [SectionData, cap:64]   [PesData, cap:256]     [TsEvent, cap:1024]
                  │                        │                          │
        SectionAssembler         PesAssembler              MetricsAggregator
                  │                        │                   ▲      ▲
        [CompleteSection, cap:64] [PesPacket, cap:256]  [PcrEvent] [NetEvent]
                  │                        │                   │
          TableDispatcher          FfmpegDecoder         PcrTracker
                  │                   │      │
        [TableEvent, cap:64]  [VideoFrame] [AudioFrame]
                  │                   │      │         cap:8    cap:32
                AppState         VideoRenderer AudioOutput

MetricsAggregator ─[watch::Sender<MetricsSnapshot>]─► UI update()
UI ────────────────[mpsc::Sender<AppCommand>]────────► CommandHandler
```

### Capacidades dos canais (SPEC-CHAN-001)

| Canal                               | Tipo              | Capacidade | Comportamento em full                   |
| ----------------------------------- | ----------------- | ---------- | --------------------------------------- |
| `net_raw` (UdpReceiver→RtpStripper) | `Bytes`           | 128        | Drop + `UdpBufferOverflow++`            |
| `ts_raw` (RtpStripper→TsDemuxer)    | `Bytes`           | 128        | Drop + log WARN                         |
| `section_data`                      | `SectionData`     | 64         | Drop seção + log                        |
| `pes_data`                          | `PesData`         | 256        | Drop PES + log (frame perdido)          |
| `ts_events`                         | `TsEvent`         | 1024       | Drop evento (métricas perdem precisão)  |
| `complete_sections`                 | `CompleteSection` | 64         | Drop seção + log                        |
| `pes_packets`                       | `PesPacket`       | 256        | Drop PES                                |
| `table_events`                      | `TableEvent`      | 64         | Drop evento + log                       |
| `video_frames`                      | `VideoFrame`      | 8          | Drop frame mais antigo (FIFO invertido) |
| `audio_frames`                      | `AudioFrame`      | 32         | Drop se buffer jitter > 2× nominal      |
| `pcr_events`                        | `PcrEvent`        | 256        | Drop evento                             |
| `net_events`                        | `NetEvent`        | 64         | Drop evento                             |
| `app_commands`                      | `AppCommand`      | 32         | Bloqueia UI (canal pequeno intencional) |

**Regra de monitoramento (SPEC-CHAN-001):**
- Em ≥ 90% da capacidade: `tracing::warn!("canal {name} em {pct}% ({used}/{cap})")`
- Produtores verificam antes de enviar usando `try_send` + fallback de drop/log

---

## `AppConfig` (SPEC-CFG-001)

```rust
pub struct AppConfig {
    pub network:  NetworkConfig,
    pub player:   PlayerConfig,
    pub analyzer: AnalyzerConfig,
    pub ui:       UiConfig,
}

pub struct NetworkConfig {
    pub udp_buffer_bytes: usize,    // padrão: 4_194_304
    pub timeout_ms:       u64,      // padrão: 5_000
    pub preferred_iface:  Option<String>,
}

pub struct PlayerConfig {
    pub jitter_buffer_ms:    u64,   // padrão: 100
    pub volume:              f32,   // padrão: 1.0; range: 0.0–2.0
    pub fallback_cpu_render: bool,  // padrão: false
}

pub struct AnalyzerConfig {
    pub bitrate_window_secs:     u64,   // padrão: 1
    pub bitrate_history_secs:    u64,   // padrão: 60
    pub pcr_jitter_threshold_us: i64,   // padrão: 500
    pub top_pids_count:          usize, // padrão: 10
    pub max_error_log_entries:   usize, // padrão: 1_000
}

pub struct UiConfig {
    pub dark_theme:    bool,  // padrão: true
    pub window_width:  u32,   // padrão: 1400
    pub window_height: u32,   // padrão: 900
}
```

**Carregamento:**
```
1. Tentar ler `ironstream.toml` na pasta do executável
2. Se não existir: usar Default::default() para todos os campos
3. Se existir mas inválido: logar WARN, usar Default::default()
4. Nunca falhar no startup por config ausente ou inválida
```

---

## Sequência de Bootstrap (`main.rs`)

```
1. init tracing (tracing_subscriber com RUST_LOG)
2. Carregar AppConfig (ironstream.toml ou defaults)
3. Verificar DLLs FFmpeg (avcodec_version via ffi)
4. Criar todos os canais bounded conforme tabela de capacidades
5. Criar StopToken + StopHandle
6. Instanciar componentes:
   a. UdpReceiver::new(url, net_raw_tx, net_events_tx, cfg.network)
   b. RtpStripper::new(net_raw_rx, ts_raw_tx, rtp_events_tx)
   c. TsDemuxer::new(section_data_tx, pes_data_tx, ts_events_tx)
   d. SectionAssembler::new(section_data_rx, complete_sections_tx)
   e. TableDispatcher::new(complete_sections_rx, table_events_tx)
   f. PesAssembler::new(pes_data_rx, pes_packets_tx)
   g. FfmpegDecoder::from_stream_type(...)
   h. PcrTracker::new(pcr_events_tx)
   i. MetricsAggregator::new(ts_events_rx, pcr_events_rx, net_events_rx)
   j. VideoRenderer::new(...)
   k. AudioOutput::new(cfg.player)
7. Spawn threads de backend (tokio::spawn ou thread::spawn conforme o componente)
8. eframe::run_native("IronPlayer", options, IronPlayerApp::new(...))   ← bloqueia até fechar
9. StopHandle::stop()  →  todas as threads terminam
```

### Threads de backend

| Thread       | Componente               | Runtime                        |
| ------------ | ------------------------ | ------------------------------ |
| `net-recv`   | `UdpReceiver::run`       | `thread::spawn` (blocking I/O) |
| `rtp-strip`  | `RtpStripper::run`       | `thread::spawn`                |
| `ts-demux`   | `TsDemuxer` loop         | `thread::spawn`                |
| `sec-asm`    | `SectionAssembler` loop  | `thread::spawn`                |
| `table-disp` | `TableDispatcher` loop   | `thread::spawn`                |
| `pes-asm`    | `PesAssembler` loop      | `thread::spawn`                |
| `av-decode`  | `FfmpegDecoder` loop     | `thread::spawn`                |
| `metrics`    | `MetricsAggregator::run` | `tokio::spawn`                 |
| `main`       | egui event loop          | blocking (main thread)         |

> **Decisão:** usar `std::thread::spawn` (não tokio) para as threads de pipeline de TS — são loops síncronos de alta frequência sem I/O async. Tokio apenas para `MetricsAggregator` (usa `watch`).

---

## Estratégia de Testes

```
spec_cfg_001_defaults_when_file_absent   — AppConfig::default() correto
spec_cfg_001_partial_override            — arquivo parcial usa defaults nos campos ausentes
spec_cfg_001_invalid_file_uses_defaults  — TOML inválido não faz panic; usa defaults

spec_chan_001_try_send_drops_on_full     — canal com cap:2; 3º send é drop + log
```

---

## Considerações de Segurança

- **`ironstream.toml` não deve aceitar paths de DLLs configuráveis** — DLLs FFmpeg carregadas apenas do diretório do executável.
- `preferred_iface` em `NetworkConfig` é validado via `Ipv4Addr::parse` antes de uso — nunca passado como string ao OS sem validação.
- Capacidades de canal são constantes em tempo de compilação — não configuráveis pelo usuário (evitar DoS por buffer infinito via config).

---

## Riscos e Mitigações

| Risco                                                        | Mitigação                                                                     |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------- |
| Thread de rede não termina ao receber `StopToken`            | `UdpReceiver::run` verifica `stop.is_stopped()` a cada iteração do loop       |
| Deadlock entre canais se produtor e consumidor trocam papéis | Grafo de canais é DAG (sem ciclos); verificar antes de adicionar novos canais |
| main thread bloqueada pelo egui não permite shutdown limpo   | `eframe::App::on_exit` envia `StopHandle::stop()` antes de retornar           |
