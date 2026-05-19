# Spec + Tasks: `ts::metrics` — Bitrate e Erros

- **Spec-IDs:** SPEC-METRICS-001, SPEC-METRICS-002, SPEC-METRICS-003
- **Módulo:** `crates/ts/src/metrics.rs` + `aggregator.rs`
- **Fase:** Alpha v0.1 (METRICS-001) · v1.0 (METRICS-002/003)

---

## Requisitos

| ID                | Requisito                              | Critério                      |
| ----------------- | -------------------------------------- | ----------------------------- |
| SPEC-METRICS-001  | `BitrateMonitor` com janela deslizante | Entradas expiram corretamente |
| SPEC-METRICS-001b | `bitrate_kbps` correto                 | 188 bytes / 1s = 1.504 kbps   |
| SPEC-METRICS-001c | `snapshot` ordenado por bitrate desc   | Lista ordenada                |
| SPEC-METRICS-001d | Bitrate total inclui null packets      | PID 0x1FFF somado             |
| SPEC-METRICS-001e | `null_packet_ratio` range 0.0–1.0      | Proporção correta             |
| SPEC-METRICS-002  | `ErrorTracker` acumula contadores      | CC, PCR, CRC, sync, RTP, UDP  |
| SPEC-METRICS-002a | Snapshot imutável                      | Mudança posterior não reflete |
| SPEC-METRICS-002c | `reset()` zera tudo                    | Todos os contadores = 0       |
| SPEC-METRICS-003  | `MetricsAggregator` publica a 1 Hz     | Watch receiver atualizado     |

---

## Tasks

### T01 — `BitrateMonitor` (SPEC-METRICS-001)

**Done when:** Todos os `spec_metrics_001_*` passam.

```
cargo test -p ts spec_metrics_001
```

### T02 — `ErrorTracker` (SPEC-METRICS-002)

**Depende de:** T01

**Done when:** `spec_metrics_002_*` passam; `reset()` confirmado.

### T03 — `MetricsSnapshot` e `PidEntry`

**Depende de:** T01, T02

**Done when:** `MetricsSnapshot` compila; `PidType` cobre todos os tipos especificados.

### T04 — `MetricsAggregator` (SPEC-METRICS-003)

**Depende de:** T02, T03

**Done when:**
- Loop consome `TsEvent`, `PcrEvent`, `NetEvent`
- `snapshot_tx.send()` a cada 1s
- Parada limpa via `StopToken`

### T05 — Limitar `pcr_jitter_events` pelo `AppConfig`

**Depende de:** T02

**Done when:** `Vec<PcrJitterRecord>` não excede `max_error_log_entries`.
