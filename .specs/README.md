# IronPlayer — Índice de TDDs e Tasks

> Mapa de navegação do ambiente documental de Spec-Driven Development.

---

## Estrutura de Documentos

```
.specs/
├── project/
│   ├── PROJECT.md          ← Visão, objetivos, stack, princípios
│   ├── ROADMAP.md          ← Fases v0.1 → v0.2 → v0.3 → v1.0
│   └── STATE.md            ← Decisões, riscos, ideias deferidas
│
└── features/
    ├── spec-01-ts-core/
    │   ├── spec.md         ← Requisitos SPEC-TS-001/002/003/004
    │   ├── design.md       ← TDD: TsPacket, TsDemuxer, SectionAssembler, PcrTracker
    │   └── tasks.md        ← 10 tasks (T01–T10)
    │
    ├── spec-02-net/
    │   ├── spec.md         ← Requisitos SPEC-NET-001/002/003
    │   ├── design.md       ← TDD: StreamUrl, UdpReceiver, RtpStripper
    │   └── tasks.md        ← 7 tasks (T01–T07)
    │
    ├── spec-03-ts-tables/
    │   ├── spec.md         ← Requisitos SPEC-TABLE-001–008
    │   ├── design.md       ← TDD: PAT, PMT, NIT, SDT, EIT, TDT, BAT, Descriptor
    │   └── tasks.md        ← 12 tasks (T01–T12)
    │
    ├── spec-04-ts-metrics/
    │   ├── spec.md         ← Requisitos SPEC-METRICS-001/002/003 + tasks
    │   └── design.md       ← TDD: BitrateMonitor, ErrorTracker, MetricsAggregator
    │
    ├── spec-05-wiring/
    │   ├── spec.md         ← Requisitos SPEC-CHAN-001, SPEC-CFG-001 + tasks
    │   └── design.md       ← TDD: canais bounded, AppConfig, bootstrap, shutdown
    │
    ├── spec-06-ui/
    │   ├── spec.md         ← Requisitos SPEC-UI-001–006 + tasks
    │   └── design.md       ← TDD: AppState, PidPanel, TablesPanel, MetricsPanel
    │
    ├── spec-07-av/
    │   ├── spec.md         ← Requisitos SPEC-AV-001/002/003/004 + tasks
    │   └── design.md       ← TDD: PesAssembler, FfmpegDecoder, VideoRenderer, AudioOutput
    │
    ├── spec-08-av-sync/            ← TDD sprint 01: clock master, VideoQueue
    ├── spec-09-gpu-decode/         ← TDD sprint 02: D3D11VA, zero-copy
    ├── spec-10-mediainfo/          ← SPEC-MI-*: probe de elementary streams
    ├── spec-11-slint/              ← Migração egui → Slint, perf 1080i, zero-copy
    ├── spec-12-deinterlace_mode/   ← Perfis Performance/Quality/Desligado
    │
    ├── spec-13-probe-mode/
    │   └── spec.md         ← SPEC-PROBE-001–026: modo Probe (seletor triplo, mosaico 2 feeds, sessão,
    │                          navegação em 4 níveis, serviços do MPTS, grade de saúde 5 min)
    │                          IMPLEMENTADA em `crates/probe`, `src/feed.rs`, `src/probe_run.rs`,
    │                          `crates/ui-slint/ui/probe.slint`
    │
    ├── spec-14-probe-ip/
        └── spec.md         ← SPEC-PROBE-IP-001–046: camada IP/UDP/RTP/FEC
    │
    ├── spec-15-probe-ts/
    │   ├── spec.md         ← SPEC-PROBE-TS-001–018: checks e medições MPEG-TS
    │   ├── design.md       ← contrato de eventos TS, PSI e PCR para a Probe
    │   └── tasks.md        ← promoção incremental, fixtures e gates

    ├── spec-16-probe-video/
    │   ├── spec.md         ← SPEC-PROBE-VID-001–016: checks de vídeo e metadados/HDR
    │   ├── design.md       ← contrato de observações de frame e análise por serviço
    │   └── tasks.md        ← entrega incremental e fixtures determinísticas
    │
    └── spec-17-probe-audio/
        ├── spec.md         ← SPEC-PROBE-AUD-001–014: checks PCM e metadados de áudio
        ├── design.md       ← contrato de amostras PCM, medidores e estado por canal
        └── tasks.md        ← entrega incremental e fixtures determinísticas
```

---

## Sequência Recomendada de Implementação

| Ordem | Feature                    | Dependências                           | Fase                   |
| ----- | -------------------------- | -------------------------------------- | ---------------------- |
| 1     | `spec-01-ts-core`          | nenhuma                                | Alpha v0.1             |
| 2     | `spec-02-net`              | nenhuma                                | Alpha v0.1             |
| 3     | `spec-03-ts-tables`        | `spec-01-ts-core`                      | Alpha v0.1 + Beta v0.3 |
| 4     | `spec-04-ts-metrics`       | `spec-01-ts-core`                      | Alpha v0.1             |
| 5     | `spec-05-wiring` (parcial) | `spec-01-ts-core`, `spec-02-net`       | Alpha v0.1             |
| 6     | `spec-06-ui` (PidPanel)    | `spec-04-ts-metrics`, `spec-05-wiring` | Alpha v0.1             |
| 7     | `spec-07-av`               | `spec-01-ts-core`                      | Alpha v0.2             |
| 8     | `spec-06-ui` (completo)    | `spec-07-av`, `spec-03-ts-tables`      | Alpha v0.2 + Beta v0.3 |
| 9     | `spec-13-probe-mode` ✅    | `spec-04-ts-metrics`, `spec-11-slint`  | v0.4 Probe             |
| 10    | `spec-14-probe-ip`         | `spec-13-probe-mode`, `spec-02-net`    | v0.4 Probe             |
| 11    | `spec-15-probe-ts`         | `spec-13-probe-mode`, `ts`             | v0.4 Probe             |
| 12    | `spec-16-probe-video`      | `spec-13-probe-mode`, `spec-15`, `av`  | v0.4 Probe             |
| 13    | `spec-17-probe-audio`      | `spec-13-probe-mode`, `spec-15`, `av`  | v0.4 Probe             |

---

## Convenção de SPEC-IDs → Arquivos de Teste

```rust
// Formato: spec_{spec_id_lowercase}_{descrição}
#[test]
fn spec_net_001_valid_udp_multicast() { ... }

#[test]
fn spec_ts_001_invalid_sync_byte() { ... }

#[test]
fn spec_table_001a_pat_parse_basic() { ... }
```

---

## Gate de Qualidade por Crate

```sh
# Antes de considerar um crate "pronto para integração":
cargo test -p <crate>             # todos os spec_* passam
cargo clippy -p <crate> -- -D warnings
cargo fmt --check
```

```sh
# Gate de workspace (antes de merge):
cargo test --workspace --locked
cargo clippy --workspace -- -D warnings
cargo fmt --check
```
