# IronPlayer — Agent Instructions

> MPEG-TS Multicast Player & Stream Analyzer · Rust · Windows 10/11 x86-64
> **Status:** pré-implementação (fase de spec/design)

## Documentação Essencial

| Documento                                              | Conteúdo                                                            |
| ------------------------------------------------------ | ------------------------------------------------------------------- |
| [.specs/README.md](.specs/README.md)                   | Índice de specs, sequência de implementação, convenção de SPEC-IDs  |
| [.specs/project/PROJECT.md](.specs/project/PROJECT.md) | Visão, objetivos, stack, princípios de design                       |
| [.specs/project/ROADMAP.md](.specs/project/ROADMAP.md) | Fases v0.1 → v1.0 e critérios de aceite por fase                    |
| [.specs/project/STATE.md](.specs/project/STATE.md)     | Decisões arquiteturais (D-001 a D-007), riscos, ideias deferidas    |
| [docs/ironstream-spec.md](docs/ironstream-spec.md)     | Spec técnico completo: workspace, crates, contratos de canal, build |

## Workspace Cargo

```
crates/net/   SPEC-NET-*    recepção UDP/RTP multicast
crates/ts/    SPEC-TS-*, SPEC-TABLE-*, SPEC-METRICS-*   demux + parser TS puro
crates/av/    SPEC-AV-*     bridge FFmpeg (decode A/V apenas)
crates/ui/    SPEC-UI-*     egui/eframe app
src/main.rs                 entry point, conecta os canais
```

Regra de dependência: `ui → ts, av, net`; `av → ts`; `ts` e `net` são standalone (zero FFI).

## Convenções Obrigatórias

### SPEC-IDs
- Toda função pública **deve** ter o `SPEC-ID` no doc-comment: `/// SPEC-TS-001`
- Nomes de teste seguem o padrão: `spec_{spec_id_lowercase}_{descrição}`
  ```rust
  #[test]
  fn spec_ts_001_invalid_sync_byte() { ... }
  ```

### Segurança e Qualidade
- **Zero panic em dados externos**: todo parsing de rede/TS retorna `Result<T, E>`, nunca usa `.unwrap()` em dados externos
- **Todo `unsafe` confinado em `av::ffi`**: `ts` e `net` são Rust seguro e puro
- **Canais sempre bounded**: usar `crossbeam-channel` bounded; comportamento de canal cheio é definido na spec — nunca usar `unbounded()`
- **UI nunca bloqueia o pipeline**: UI lê `MetricsSnapshot` imutável via `tokio::sync::watch` a 1 Hz

### Estilo Rust
- MSRV: Rust 1.78 stable
- Erros customizados com `thiserror`; propagação com `anyhow` nos binários
- Rastreamento com `tracing` (não `println!` em código de produção)

## Comandos de Build/Test

```bash
# Checar crate individual
cargo check -p ts

# Testar crate individual (gate por feature)
cargo test -p ts
cargo test -p net

# Lint — CI rejeita warnings
cargo clippy -p ts -- -D warnings

# Rodar testes de um SPEC-ID específico
cargo test -p ts spec_ts_001
```

**Gate de cada feature:** `cargo test -p {crate}` verde + `cargo clippy -- -D warnings` sem avisos.

## Specs por Feature

Cada feature em `.specs/features/spec-XX-*/` tem:
- `spec.md` — requisitos com SPEC-IDs
- `design.md` — TDD: tipos, structs, contratos de API
- `tasks.md` — tasks atômicas com critérios de "Done when"

Implementar seguindo a [sequência do roadmap](.specs/README.md#sequência-recomendada-de-implementação): `spec-01-ts-core` → `spec-02-net` → ...

## Stack de Terceiros

| Lib               | Versão              | Uso                                                 |
| ----------------- | ------------------- | --------------------------------------------------- |
| egui/eframe       | 0.29                | UI immediate-mode                                   |
| wgpu              | backend D3D11       | renderização GPU (Windows)                          |
| FFmpeg 7.x        | `ffmpeg-next` crate | decode H.264/HEVC/AAC/AC-3 **apenas**               |
| cpal              | 0.15                | saída de áudio via WASAPI                           |
| crossbeam-channel | 0.5                 | canais bounded inter-thread                         |
| tokio             | 1                   | async runtime (somente em `net` e camada de wiring) |

FFmpeg é LGPL: linkar dinamicamente, distribuir DLLs separadas (`ffmpeg/` na raiz).
