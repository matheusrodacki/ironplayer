# Spec + Tasks: Wiring — Canais e `main.rs`

- **Spec-IDs:** SPEC-CHAN-001, SPEC-CFG-001
- **Fase:** Alpha v0.1

---

## Requisitos

| ID            | Requisito                                      | Critério                                     |
| ------------- | ---------------------------------------------- | -------------------------------------------- |
| SPEC-CHAN-001 | Todos os canais bounded                        | Capacidades conforme tabela no design.md     |
| SPEC-CHAN-001 | 90% → log WARN; 100% → comportamento por canal | Implementado e testado                       |
| SPEC-CFG-001  | `AppConfig` carregada de `ironstream.toml`     | Config ausente usa defaults; inválida também |

---

## Tasks

### T01 — `AppConfig` e carregamento de TOML (SPEC-CFG-001)

**Done when:**
- `AppConfig::load_or_default()` funciona sem `ironstream.toml`
- TOML parcial usa `Default` nos campos ausentes
- TOML inválido loga WARN e usa defaults (não panic)

**Testes:** `spec_cfg_001_*`

---

### T02 — Canais bounded com backpressure (SPEC-CHAN-001)

**Done when:**
- Todos os canais criados com capacidades da tabela
- `try_send` com fallback de drop/log em cada produtor
- Log WARN quando canal ≥ 90% cheio

**Testes:** `spec_chan_001_try_send_drops_on_full`

---

### T03 — Bootstrap: wiring de todos os componentes

**Depende de:** todos os crates implementados (net/T04, ts-core/T05, ts-core/T06, ts-tables/T04, av/T01, av/T03, ui/T02)

**Done when:**
- `main.rs` compila com todos os componentes instanciados
- `cargo run` inicia a janela egui sem stream ativo

---

### T04 — Verificação de DLLs FFmpeg no startup

**Depende de:** av/T06

**Done when:** `main.rs` verifica versão das DLLs antes de iniciar; exibe erro claro se incompatíveis.

---

### T05 — Shutdown limpo

**Depende de:** T03

**Done when:**
- Fechar a janela egui aciona `StopHandle::stop()`
- Todas as threads de backend terminam (join com timeout de 2s)
- Sem threads órfãs após fechar

---

### T06 — `AppConfig` padrão gerado se ausente

**Done when:** Na primeira execução sem `ironstream.toml`, o arquivo padrão é criado com todos os valores default documentados.
