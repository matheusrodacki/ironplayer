# IronPlayer — Roadmap

> Versão sincronizada com `ironstream-spec.md` v0.1-draft

---

## Fases de Entrega

### Alpha v0.1 — Core TS + Tabela de PIDs

**Meta:** pipeline de bytes funcionando do socket até a UI com análise de PIDs em tempo real.

| Spec-ID          | Requisito                                           | Feature      |
| ---------------- | --------------------------------------------------- | ------------ |
| SPEC-NET-001     | `StreamUrl::parse` passa todos os 6 casos           | `net`        |
| SPEC-NET-002     | `UdpReceiver` conecta em multicast local            | `net`        |
| SPEC-TS-001      | `TsPacket::parse` passa todos os 6 cenários         | `ts-core`    |
| SPEC-TS-002      | Demuxer detecta CC errors em fixture                | `ts-core`    |
| SPEC-TS-003      | Seção fragmentada em 3 pacotes montada corretamente | `ts-core`    |
| SPEC-TABLE-001a  | PAT parseada da fixture `pat_section.bin`           | `ts-tables`  |
| SPEC-TABLE-002a  | PMT parseada com stream_types corretos              | `ts-tables`  |
| SPEC-METRICS-001 | BitrateMonitor calculando kbps por PID              | `ts-metrics` |
| SPEC-UI-003      | Tabela de PIDs exibe PID, tipo, bitrate ao vivo     | `ui`         |
| SPEC-CHAN-001    | Canais bounded com backpressure definida            | `wiring`     |

---

### Alpha v0.2 — Player A/V

**Meta:** vídeo e áudio reproduzindo com latência < 2 s.

| Spec-ID      | Requisito                                          | Feature   |
| ------------ | -------------------------------------------------- | --------- |
| SPEC-AV-001  | PesAssembler reconstrói PES H.264 fragmentado      | `av`      |
| SPEC-AV-002  | FfmpegDecoder produz VideoFrame para fixture H.264 | `av`      |
| SPEC-AV-003  | Vídeo exibido na UI sem tearing (1080p/25fps)      | `av`      |
| SPEC-AV-004  | Áudio reproduzido sincronizado (desvio < 40 ms)    | `av`      |
| SPEC-NET-003 | RtpStripper remove header RTP corretamente         | `net`     |
| SPEC-TS-004  | PcrTracker detecta jitter e descontinuidade        | `ts-core` |

---

### Beta v0.3 — Tabelas DVB Completas

**Meta:** análise profissional de multiplexes DVB (NIT, SDT, EIT, TDT, BAT).

| Spec-ID        | Requisito                                     | Feature     |
| -------------- | --------------------------------------------- | ----------- |
| SPEC-TABLE-003 | NIT com network_name e delivery descriptor    | `ts-tables` |
| SPEC-TABLE-004 | SDT com service_name e running_status         | `ts-tables` |
| SPEC-TABLE-005 | EIT com event_name, start_time MJD+BCD        | `ts-tables` |
| SPEC-TABLE-006 | TDT com offset de relógio                     | `ts-tables` |
| SPEC-TABLE-007 | BAT com bouquet_name                          | `ts-tables` |
| SPEC-TABLE-008 | Descriptors genéricos + KnownDescriptor       | `ts-tables` |
| SPEC-UI-004    | TablesPanel com árvore PAT/PMT/NIT/SDT/EIT    | `ui`        |
| SPEC-UI-005    | MetricsPanel com gráficos de bitrate e jitter | `ui`        |

---

### v1.0 — Release Estável

**Meta:** produto completo, testado, com CI verde e documentação final.

| Item                    | Descrição                                                     |
| ----------------------- | ------------------------------------------------------------- |
| CI GitHub Actions       | `cargo test --workspace`, `clippy -D warnings`, `fmt --check` |
| Fixtures de teste       | Fixtures binárias reais para todas as tabelas DVB             |
| Empacotamento           | Binário `ironstream.exe` + DLLs FFmpeg em zip sem instalador  |
| Documentação de usuário | README + guia de instalação + capturas de tela                |
| SPEC-METRICS-002/003    | ErrorTracker + MetricsAggregator completos                    |
| SPEC-UI-006             | StatusBar com indicadores de estado                           |
| SPEC-CFG-001            | AppConfig carregada de `ironstream.toml`                      |

---

### v1.x — Funcionalidades Futuras

| Item                                      | Descrição                                                                                                                                                                                                                                                             |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Legendas DVB (SPEC-SUB-001)               | Parser de subtitling_descriptor na PMT; decodificação de DVB Subtitles (EN 300 743); renderização sobreposta no `VideoPanel` via `egui::Painter`. Requer `AppCommand::SelectSubtitlePid`, novo canal `subtitle_frames` e integração com o menu de contexto do player. |
| Seleção de faixa de áudio (SPEC-AV-005)   | Suporte a múltiplas faixas de áudio por serviço; `AppCommand::SelectAudioPid`; troca dinâmica sem interrupção do vídeo.                                                                                                                                               |
| Exportação de métricas (SPEC-METRICS-004) | Exportar histórico de bitrate e jitter PCR para CSV/JSON.                                                                                                                                                                                                             |
| Gravação de stream (SPEC-NET-004)         | Dump do TS bruto para arquivo `.ts` com buffer circular configurável.                                                                                                                                                                                                 |

---

## Dependências entre Features

```
net ──────────────────────────────────► wiring
ts-core ─────────────────────────────► wiring
ts-tables  (depende de ts-core) ──────► wiring
ts-metrics (depende de ts-core) ──────► wiring
av         (depende de ts-core) ──────► wiring
ui         (depende de todos)   ──────► wiring
```

**Ordem sugerida de implementação:**
1. `net` (sem deps)
2. `ts-core` (sem deps internas)
3. `ts-tables` (usa tipos de `ts-core`)
4. `ts-metrics` (usa tipos de `ts-core`)
5. `av` (usa PES de `ts-core`)
6. `wiring` (main.rs, conecta tudo)
7. `ui` (consome snapshots de todos)
