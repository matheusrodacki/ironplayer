# IronPlayer — Visão do Projeto

- **Data:** 2026-05-19
- **Status:** Ativo — fase de documentação / pré-implementação
- **Owner:** Open Source Community
- **Stack:** Rust · egui/eframe · FFmpeg (decode only) · Windows 10/11 x86-64

---

## Visão

O **IronPlayer** é um player e analisador de MPEG-TS para profissionais de vídeo e streaming, construído para **ser livre das amarras de fornecedores proprietários**. Gratuito, opensource, sem licença por seat, sem call-home, sem feature-wall.

**Problema que resolve:** ferramentas profissionais de análise de MPEG-TS (Wireshark com plugin DVB, TSReader, Bitrate Viewer, etc.) são caras, fechadas, ou desatualizadas. Profissionais de broadcast e OTT precisam de uma alternativa OpenSource de qualidade de produção.

---

## Objetivos

| #   | Objetivo                         | Critério de sucesso                                                            |
| --- | -------------------------------- | ------------------------------------------------------------------------------ |
| O1  | Receber stream UDP/RTP multicast | Conectar e decodificar stream local sem erro                                   |
| O2  | Parser MPEG-TS completo          | Detectar CC errors, montar seções, parsear todas tabelas PSI/SI/DVB            |
| O3  | Player A/V funcional             | Exibir vídeo H.264/HEVC + áudio AAC/AC-3 com latência < 2 s                    |
| O4  | Analisador de métricas           | Bitrate por PID, jitter PCR, log de erros em tempo real                        |
| O5  | UI profissional egui             | Layout de 3 painéis responsivo, tabelas navegáveis, gráficos de série temporal |
| O6  | Portabilidade Windows            | Binário único, DLLs FFmpeg embutidas, sem instalador obrigatório               |

---

## Não-objetivos (v1.0)

- Suporte a Linux / macOS (roadmap futuro)
- Ingesta via HTTP(S) / HLS / DASH
- Codificação ou transcodificação de streams
- Interface de linha de comando (CLI) separada
- Suporte a DVB-S2X (física de satélite)
- Gravação / DVR

---

## Princípios de Design

1. **Segurança por padrão:** `ts` e `net` são crates sem FFI; `av` isola todo `unsafe` em um único módulo.
2. **Zero panic em dados externos:** todo parsing retorna `Result<T, E>`; dados de rede nunca fazem panic.
3. **Rastreabilidade total:** cada requisito tem um `SPEC-ID`; cada teste referencia o ID correspondente.
4. **Backpressure explícita:** todos os canais inter-thread são bounded; comportamento de full é definido na spec.
5. **UI não bloqueia pipeline:** a UI lê snapshots imutáveis a 1 Hz; nunca faz lock no caminho crítico de TS.

---

## Stack de Tecnologia

| Camada              | Tecnologia                              | Justificativa                                                       |
| ------------------- | --------------------------------------- | ------------------------------------------------------------------- |
| Linguagem           | Rust stable (MSRV: 1.78)                | Memory safety, performance, ecossistema de broadcast em crescimento |
| UI                  | egui 0.29 / eframe 0.29                 | Immediate-mode, integra com wgpu, Windows-first                     |
| Renderização GPU    | wgpu (backend D3D11)                    | Nativo Windows, sem OpenGL                                          |
| Decodificação A/V   | FFmpeg 7.x (libavcodec) via ffmpeg-next | Suporte a todos os codecs relevantes                                |
| Áudio               | cpal 0.15 (WASAPI)                      | Nativo Windows, latência baixa                                      |
| Async runtime       | tokio 1.x                               | I/O de rede; thread pool                                            |
| Canais sync         | crossbeam-channel 0.5                   | Pipeline intra-thread de alta performance                           |
| Serialização config | serde + toml                            | Arquivo `ironstream.toml` human-readable                            |
| Logging             | tracing 0.1 + tracing-subscriber        | Structured logging com spans                                        |
| Erros               | thiserror 2                             | Tipos de erro idiomáticos                                           |

---

## Estrutura de Crates

```
ironstream/
├── Cargo.toml              # workspace root
├── crates/
│   ├── net/                # SPEC-NET-*  recepção UDP/RTP multicast
│   ├── ts/                 # SPEC-TS-*, SPEC-TABLE-*, SPEC-METRICS-*
│   ├── av/                 # SPEC-AV-*  FFmpeg bridge + render + áudio
│   └── ui/                 # SPEC-UI-*  egui app
└── src/
    └── main.rs             # wiring de canais + bootstrap
```

**Invariante de dependência:**
```
ui  →  ts, av, net
av  →  ts
ts  →  (sem deps internas)
net →  (sem deps internas)
```

---

## Convenção de SPEC-IDs

| Prefixo          | Crate / Módulo                           |
| ---------------- | ---------------------------------------- |
| `SPEC-NET-*`     | `crates/net`                             |
| `SPEC-TS-*`      | `crates/ts` — demuxer e parser base      |
| `SPEC-TABLE-*`   | `crates/ts::tables` — tabelas PSI/SI/DVB |
| `SPEC-METRICS-*` | `crates/ts::metrics` — bitrate e erros   |
| `SPEC-AV-*`      | `crates/av`                              |
| `SPEC-UI-*`      | `crates/ui`                              |
| `SPEC-CFG-*`     | `AppConfig` (shared)                     |
| `SPEC-CHAN-*`    | Contratos de canal entre crates          |
