# Spec + Tasks: `crates/ui` — Interface egui

- **Spec-IDs:** SPEC-UI-001 a SPEC-UI-006
- **Fase:** Alpha v0.1 (UI-003) · Alpha v0.2 (UI-001/002) · Beta v0.3 (UI-004/005) · v1.0 (UI-006)

---

## Requisitos

| ID           | Requisito                                    | Critério                                         |
| ------------ | -------------------------------------------- | ------------------------------------------------ |
| SPEC-UI-001  | Layout 3 painéis + header + status bar       | Compilação e exibição correta                    |
| SPEC-UI-002  | `AppState` + `AppCommand` — modelo de estado | URL validada antes de conectar                   |
| SPEC-UI-003  | `PidPanel` com tabela ordenável              | PID, tipo, bitrate ao vivo; linha CC em vermelho |
| SPEC-UI-004a | `TablesPanel` árvore PAT/PMT/NIT/SDT/EIT/TDT | Collapsible; atualiza a 1 Hz                     |
| SPEC-UI-004b | Aba "Serviços" com clique duplo              | `AppCommand::SelectService` enviado              |
| SPEC-UI-005a | Gráfico de bitrate 60s                       | `egui_plot::Plot` com 2 linhas                   |
| SPEC-UI-005b | Gráfico PCR jitter                           | Linha limiar ±500 µs                             |
| SPEC-UI-005c | Log de erros com Copiar/Limpar               | Máx 1000 entradas                                |
| SPEC-UI-006a | StatusBar com ícone + texto                  | Nunca apenas cor                                 |

---

## Tasks

### T01 — `AppState`, `AppCommand`, `TablesSnapshot`

**Done when:** Tipos compilam; `Default` implementado para `AppState`.

### T02 — `IronPlayerApp` (eframe::App scaffold)

**Depende de:** T01

**Done when:** Janela abre em `eframe::run_native`; layout de 3 colunas visível.

### T03 — `PidPanel` (SPEC-UI-003)

**Depende de:** T01, T02

**Done when:**
- Tabela renderiza com 6 colunas
- Ordenação clicável funciona
- Linha CC > 0 com fundo vermelho
- Clique → `AppCommand::SelectPid`

### T04 — `VideoPanel` (SPEC-UI-001)

**Depende de:** T02, av/T05

**Done when:** Frame de vídeo exibido via `egui::Image` com `TextureId`; placeholder quando sem stream.

### T05 — `TablesPanel` (SPEC-UI-004)

**Depende de:** T01, T02

**Done when:**
- Árvore PAT/PMT renderiza com `CollapsingHeader`
- NIT, SDT, EIT, TDT, BAT exibidos quando disponíveis
- Clique duplo em serviço envia `AppCommand::SelectService`

### T06 — `MetricsPanel` (SPEC-UI-005)

**Depende de:** T02

**Done when:**
- `egui_plot::Plot` exibe bitrate total + PID selecionado
- PCR jitter plot visível para PID com PCR
- Log de erros com scroll, Copiar (TSV), Limpar

### T07 — `StatusBar` (SPEC-UI-006)

**Depende de:** T02

**Done when:** Todos os estados de `ConnectionState` exibidos com ícone correto.

### T08 — Integração: AppState recebe snapshot via watch

**Depende de:** T01, ts-metrics/T04

**Done when:** `watch::Receiver<MetricsSnapshot>` atualiza `AppState` na thread egui a cada frame.

### T09 — Testes unitários de formatação

**Done when:** `spec_ui_003_pid_format_hex`, `spec_ui_006_status_bar_text_*` passam.
