# IronPlayer

[English](README.en.md) | **Português (BR)**

**MPEG-TS Multicast Player & Stream Analyzer** — Rust · Windows 10/11 x86-64

> Player e analisador de MPEG-TS para profissionais de vídeo e streaming. Gratuito, open source, sem licença por seat, sem call-home, sem feature-wall.

---

## Visão Geral

Ferramentas profissionais de análise de MPEG-TS (TSReader, Bitrate Viewer, Wireshark+DVB) são caras, fechadas ou desatualizadas. O **IronPlayer** nasce dessa lacuna: uma alternativa open source de qualidade de produção que combina em uma única janela:

- Reprodução ao vivo de streams multicast UDP/RTP
- Visualização em tempo real da estrutura do Transport Stream
- Análise completa de tabelas PSI/SI e DVB (PAT, PMT, NIT, SDT, EIT, TDT, BAT)
- Métricas de bitrate por PID com gráfico de histórico
- Detecção de erros de Continuity Counter, jitter de PCR e null packets

## Status

**Fase de Spec/Design — pré-implementação.** Ver [roadmap](.specs/project/ROADMAP.md) para o plano de entregas.

## Stack

| Camada              | Tecnologia                      |
| ------------------- | ------------------------------- |
| Linguagem           | Rust stable (MSRV 1.78)         |
| UI                  | egui 0.29 / eframe 0.29         |
| Renderização GPU    | wgpu (D3D11 — Windows)          |
| Decodificação A/V   | FFmpeg 7.x via `ffmpeg-next`    |
| Áudio               | cpal 0.15 (WASAPI)              |
| Canais inter-thread | crossbeam-channel 0.5 (bounded) |

## Arquitetura

Cargo workspace com 4 crates:

```
crates/net/   — recepção UDP/RTP multicast
crates/ts/    — demuxer + parser MPEG-TS (Rust puro, sem FFI)
crates/av/    — bridge FFmpeg (decode A/V apenas)
crates/ui/    — aplicação egui
src/main.rs   — entry point, conecta os canais
```

Regra de dependência: `ui → ts, av, net` · `av → ts` · `ts` e `net` são standalone.

## Pré-requisitos

- Rust 1.78+ (stable)
- Windows 10/11 x86-64
- DLLs FFmpeg 7.x em `ffmpeg/` na raiz (ver [spec técnica](docs/ironstream-spec.md#workspace--crates))

## Build, Run & Test

```bash
# Build de desenvolvimento (debug)
cargo build

# Build otimizado (release)
cargo build --release

# Executar em modo debug
cargo run --bin ironplayer

# Executar em modo release
cargo run --release --bin ironplayer

# Testar crate individual
cargo test -p ts
cargo test -p net

# Lint (CI rejeita warnings)
cargo clippy -p ts -- -D warnings
```

## Documentação

| Documento                                                  | Conteúdo                                     |
| ---------------------------------------------------------- | -------------------------------------------- |
| [docs/ironstream-spec.md](docs/ironstream-spec.md)         | Spec técnico completo                        |
| [docs/ironstream-prd-v0.1.md](docs/ironstream-prd-v0.1.md) | Product Requirements Document                |
| [.specs/README.md](.specs/README.md)                       | Índice de specs e sequência de implementação |
| [.specs/project/ROADMAP.md](.specs/project/ROADMAP.md)     | Fases v0.1 → v1.0                            |
| [.specs/project/STATE.md](.specs/project/STATE.md)         | Decisões arquiteturais e riscos              |

## Licença

Distribuído sob a licença MIT. Ver [LICENSE](LICENSE) para detalhes.

> **Nota sobre FFmpeg:** as DLLs FFmpeg são distribuídas separadamente sob licença LGPL 2.1+. O IronPlayer as linka dinamicamente e não as incorpora ao binário principal.
